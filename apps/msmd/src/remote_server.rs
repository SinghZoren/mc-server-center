//! Remote listener (REM-01..05): TLS 1.3, pinned client certificates, one-time pairing codes,
//! per-device scopes, revocation that disconnects live sessions, and rate limits.
//!
//! Off by default. When enabled, an unknown client certificate gets a *pairing-only* session
//! that can do exactly one thing: prove knowledge of a pairing code issued by the owner.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use msm_api::codec::MAX_CONTROL_FRAME;
use msm_api::{ApiError, Capability, Message, Outcome, Principal, SoftwareInfo};
use msm_core::ErrorCode;
use msm_core::ids::DeviceId;
use msm_transport::FramedConn;
use msm_transport::remote::{self, Identity};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::daemon::Daemon;

/// How long a pairing code is valid.
pub const CODE_TTL: Duration = Duration::from_secs(10 * 60);
/// Failed pairing attempts allowed per 10 minutes before all pending codes are revoked.
const MAX_FAILED_PAIRINGS: usize = 10;

/// A pending pairing code.
#[derive(Debug, Clone)]
struct PendingCode {
    normalized: String,
    name: String,
    scopes: Vec<Capability>,
    expires: Instant,
}

/// Remote state owned by the daemon.
#[derive(Debug, Default)]
pub struct RemoteState {
    codes: Mutex<Vec<PendingCode>>,
    failures: Mutex<Vec<Instant>>,
    sessions: Mutex<HashMap<DeviceId, Vec<CancellationToken>>>,
    listener: Mutex<Option<CancellationToken>>,
}

/// Default scopes for a new device: everything except administration.
#[must_use]
pub fn default_scopes() -> Vec<Capability> {
    Capability::ALL
        .iter()
        .copied()
        .filter(|c| *c != Capability::Admin)
        .collect()
}

impl RemoteState {
    /// Issues a code; returns the display form.
    pub fn issue_code(&self, name: String, scopes: Vec<Capability>) -> String {
        let code = remote::new_pairing_code();
        if let Ok(mut codes) = self.codes.lock() {
            codes.retain(|c| c.expires > Instant::now());
            codes.push(PendingCode {
                normalized: remote::normalize_code(&code),
                name,
                scopes,
                expires: Instant::now() + CODE_TTL,
            });
        }
        code
    }

    /// Checks a proof against pending codes; consumes the matching code.
    fn redeem(&self, proof: &str, server_fp: &str, client_fp: &str) -> Option<PendingCode> {
        let mut codes = self.codes.lock().ok()?;
        codes.retain(|c| c.expires > Instant::now());
        let idx = codes.iter().position(|c| {
            remote::verify_proof(
                &remote::client_proof(&c.normalized, server_fp, client_fp),
                proof,
            )
        })?;
        Some(codes.remove(idx))
    }

    fn record_failure(&self) {
        let now = Instant::now();
        if let Ok(mut f) = self.failures.lock() {
            f.retain(|t| now.duration_since(*t) < Duration::from_secs(600));
            f.push(now);
            if f.len() >= MAX_FAILED_PAIRINGS {
                // Someone is guessing: invalidate every pending code.
                if let Ok(mut codes) = self.codes.lock() {
                    codes.clear();
                }
                f.clear();
            }
        }
    }

    /// Registers a live session so revocation can end it.
    pub fn track(&self, device: DeviceId) -> CancellationToken {
        let token = CancellationToken::new();
        if let Ok(mut s) = self.sessions.lock() {
            s.entry(device).or_default().push(token.clone());
        }
        token
    }

    /// Ends every live session of a device.
    pub fn disconnect(&self, device: DeviceId) {
        if let Ok(mut s) = self.sessions.lock()
            && let Some(tokens) = s.remove(&device)
        {
            for t in tokens {
                t.cancel();
            }
        }
    }

    /// Whether the listener is running.
    pub fn is_listening(&self) -> bool {
        self.listener.lock().map(|l| l.is_some()).unwrap_or(false)
    }

    /// Stops the listener (existing sessions are also ended).
    pub fn stop(&self) {
        if let Ok(mut l) = self.listener.lock()
            && let Some(t) = l.take()
        {
            t.cancel();
        }
        if let Ok(mut s) = self.sessions.lock() {
            for (_, tokens) in s.drain() {
                for t in tokens {
                    t.cancel();
                }
            }
        }
    }
}

/// Loads the daemon's remote identity, creating it on first use. The private key lives in the
/// private `secrets/` directory (0700 on Unix; the per-user profile on Windows).
pub fn load_identity(daemon: &Daemon) -> Result<Identity, String> {
    let path = daemon.data.secrets().join("daemon-identity.json");
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Some(id) = Identity::from_json(&text)
    {
        return Ok(id);
    }
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "msmd".into());
    let id = Identity::generate(&host)?;
    msm_fsguard::write_atomic(&path, id.to_json().as_bytes()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(id)
}

/// Starts listening on `bind:port`.
pub fn start(
    daemon: Arc<Daemon>,
    bind: std::net::IpAddr,
    port: u16,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>> {
    // Boxed: sessions started here can call back into `remote.enable`, which calls `start`.
    Box::pin(start_inner(daemon, bind, port))
}

async fn start_inner(
    daemon: Arc<Daemon>,
    bind: std::net::IpAddr,
    port: u16,
) -> Result<String, String> {
    let identity = load_identity(&daemon)?;
    let acceptor = remote::acceptor(&identity)?;
    let server_fp = identity.fingerprint();
    let listener = tokio::net::TcpListener::bind((bind, port))
        .await
        .map_err(|e| format!("cannot listen on port {port}: {e}"))?;
    let stop = CancellationToken::new();
    {
        let remote = daemon.remote();
        remote.stop();
        if let Ok(mut l) = remote.listener.lock() {
            *l = Some(stop.clone());
        }
    }
    let fp = server_fp.clone();
    tokio::spawn(async move {
        let per_ip: Arc<Mutex<HashMap<std::net::IpAddr, Vec<Instant>>>> = Arc::default();
        loop {
            let (tcp, peer) = tokio::select! {
                r = listener.accept() => match r { Ok(x) => x, Err(_) => continue },
                () = stop.cancelled() => break,
            };
            // Connection rate limit per source address (T11).
            let allowed = per_ip.lock().map(|mut m| {
                let now = Instant::now();
                let v = m.entry(peer.ip()).or_default();
                v.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
                v.push(now);
                // Relayed players each open one connection from the home server's address.
                v.len() <= 240
            });
            if !matches!(allowed, Ok(true)) {
                continue;
            }
            let (acc, daemon, fp) = (acceptor.clone(), daemon.clone(), fp.clone());
            tokio::spawn(async move {
                let Ok((tls, client_fp)) = remote::accept(&acc, tcp).await else {
                    return;
                };
                match daemon.device_by_fingerprint(&client_fp) {
                    Some((id, scopes)) => {
                        let cancel = daemon.remote().track(id);
                        daemon.touch_device(id);
                        crate::session::serve(
                            daemon,
                            tls,
                            Principal::Device { id, scopes },
                            Some(cancel),
                        )
                        .await;
                    }
                    None => pairing_session(daemon, tls, &fp, &client_fp, peer.ip()).await,
                }
            });
        }
    });
    Ok(server_fp)
}

async fn pairing_session(
    daemon: Arc<Daemon>,
    stream: remote::ServerStream,
    server_fp: &str,
    client_fp: &str,
    ip: std::net::IpAddr,
) {
    let mut conn = FramedConn::new(stream, 64 * 1024.min(MAX_CONTROL_FRAME));
    let Ok(Ok(Some(Message::Hello { .. }))) =
        tokio::time::timeout(Duration::from_secs(15), conn.recv()).await
    else {
        return;
    };
    let hello = Message::HelloOk {
        proto: 1,
        daemon: SoftwareInfo {
            name: "msmd".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        features: vec!["pairing".into()],
        principal: Principal::Device {
            id: DeviceId::new(),
            scopes: vec![],
        },
    };
    if conn.send(&hello).await.is_err() {
        return;
    }
    let Ok(Ok(Some(Message::Req {
        id, cmd, params, ..
    }))) = tokio::time::timeout(Duration::from_secs(60), conn.recv()).await
    else {
        return;
    };
    let outcome = if cmd != "pair.complete" {
        Outcome::Err(ApiError::new(
            ErrorCode::Unauthenticated,
            "this device is not paired",
        ))
    } else {
        let proof = params
            .get("proof")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name: String = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Controller")
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();
        match daemon.remote().redeem(proof, server_fp, client_fp) {
            Some(code) => {
                let device = daemon.add_device(
                    if name.trim().is_empty() {
                        &code.name
                    } else {
                        &name
                    },
                    client_fp,
                    &code.scopes,
                );
                daemon.audit_remote(&format!("ip:{ip}"), "pair.complete", device.is_ok());
                match device {
                    Ok(id) => Outcome::Ok(json!({
                        "device": id,
                        "scopes": code.scopes,
                        "server_proof": remote::server_proof(&code.normalized, server_fp, client_fp),
                    })),
                    Err(e) => Outcome::Err(e),
                }
            }
            None => {
                daemon.remote().record_failure();
                daemon.audit_remote(&format!("ip:{ip}"), "pair.complete", false);
                // Slow down guessing.
                tokio::time::sleep(Duration::from_secs(2)).await;
                Outcome::Err(ApiError::new(
                    ErrorCode::Unauthenticated,
                    "pairing code is wrong or expired",
                ))
            }
        }
    };
    let _ = conn.send(&Message::Res { id, outcome }).await;
}
