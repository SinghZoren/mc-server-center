//! Self-hosted relay (TUN-01): friends join a server running at home through the user's own
//! VPS, even when the home connection can't accept incoming connections (CGNAT, no port
//! forwarding). Nothing goes through any third party.
//!
//! ```text
//!   friend ──TCP──▶ VPS :25565 ─┐                         ┌─▶ 127.0.0.1:<server port>
//!                               │  msmd on the VPS (hub)  │   msmd at home (client)
//!                               └─◀── TLS 1.3, pinned ───◀┘   (outbound only)
//! ```
//!
//! * The home daemon pairs with the VPS daemon once, using a one-time code, as a device whose
//!   only permission is `Relay`. Both sides pin each other's certificate (ADR-003).
//! * A long-lived **control** connection (client name `msm-relay`) asks the hub to open public
//!   ports (`relay.open`) and receives an event for each player connection.
//! * For each player, home opens a **data** connection (client name `msm-relay-data`), sends
//!   `relay.attach {conn}`, and after the reply the connection carries raw bytes in both
//!   directions. Connection IDs are random and only the owning device can attach to them.
//! * Players appear to the Minecraft server as connecting from 127.0.0.1.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use msm_api::codec::MAX_CONTROL_FRAME;
use msm_api::{ApiError, Capability, Message, Outcome, Principal, SoftwareInfo};
use msm_core::ErrorCode;
use msm_core::ids::{DeviceId, InstanceId, RequestId};
use msm_transport::FramedConn;
use msm_transport::remote;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::daemon::Daemon;

/// Client name of the control connection.
pub const CONTROL_CLIENT: &str = "msm-relay";
/// Client name of a per-player data connection.
pub const DATA_CLIENT: &str = "msm-relay-data";
/// How long a player connection waits for home to pick it up.
const PENDING_TTL: Duration = Duration::from_secs(15);
const MAX_PORTS_PER_DEVICE: usize = 10;
const MAX_PENDING_PER_DEVICE: usize = 64;
/// Settings key holding the home side's configuration.
const CLIENT_SETTING: &str = "relay_client";

fn err(code: ErrorCode, detail: impl Into<String>) -> ApiError {
    ApiError::new(code, detail)
}

/// Topic on which the hub announces new player connections to a device.
#[must_use]
pub fn topic_for(device: &DeviceId) -> String {
    format!("relay/{device}")
}

/// Copies bytes both ways until either side closes. `leftover` was already read from `b`.
async fn splice<A, B>(mut a: A, mut b: B, leftover: Vec<u8>)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    if !leftover.is_empty() && a.write_all(&leftover).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
}

// =========================================================================================
// VPS side: the hub
// =========================================================================================

struct HubListener {
    device: DeviceId,
    label: String,
    cancel: CancellationToken,
}

struct Pending {
    device: DeviceId,
    stream: TcpStream,
}

/// Public ports opened for paired home daemons, and player connections waiting to be picked up.
#[derive(Default)]
pub struct RelayHub {
    listeners: Mutex<HashMap<u16, HubListener>>,
    pending: Mutex<HashMap<String, Pending>>,
    active: Arc<AtomicUsize>,
}

impl RelayHub {
    /// Opens public `port` for `device`. Idempotent for the same device.
    ///
    /// # Errors
    /// Port in use, invalid, or too many ports.
    pub async fn open(
        daemon: &Arc<Daemon>,
        device: DeviceId,
        port: u16,
        label: String,
    ) -> Result<Value, ApiError> {
        let hub = daemon.relay_hub();
        if port < 1024 {
            return Err(err(
                ErrorCode::InvalidArgument,
                "relay ports must be 1024 or higher",
            ));
        }
        if daemon.remote_port_setting() == Some(port) {
            return Err(err(
                ErrorCode::InvalidArgument,
                "that port is used for remote control",
            ));
        }
        if daemon.local_instance_ports().await.contains(&port) {
            return Err(err(
                ErrorCode::InvalidArgument,
                format!("port {port} is used by a server on the relay machine"),
            ));
        }
        {
            let listeners = hub
                .listeners
                .lock()
                .map_err(|_| err(ErrorCode::Internal, "lock"))?;
            if let Some(l) = listeners.get(&port) {
                return if l.device == device {
                    Ok(json!({ "port": port, "already_open": true }))
                } else {
                    Err(err(
                        ErrorCode::InvalidArgument,
                        format!("port {port} is already relayed for another device"),
                    ))
                };
            }
            if listeners.values().filter(|l| l.device == device).count() >= MAX_PORTS_PER_DEVICE {
                return Err(err(
                    ErrorCode::InvalidArgument,
                    "too many relayed ports for this device",
                ));
            }
        }
        let bind = daemon.remote_listen_setting().map_or(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            |(ip, _)| ip,
        );
        let listener = TcpListener::bind((bind, port)).await.map_err(|e| {
            err(
                ErrorCode::InvalidArgument,
                format!("port {port} can't be opened on the relay machine: {e}"),
            )
        })?;
        let cancel = CancellationToken::new();
        hub.listeners
            .lock()
            .map_err(|_| err(ErrorCode::Internal, "lock"))?
            .insert(
                port,
                HubListener {
                    device,
                    label: label.chars().take(64).collect(),
                    cancel: cancel.clone(),
                },
            );
        let d = Arc::clone(daemon);
        tokio::spawn(async move {
            loop {
                let stream = tokio::select! {
                    r = listener.accept() => match r { Ok((s, _)) => s, Err(_) => continue },
                    () = cancel.cancelled() => break,
                };
                let _ = stream.set_nodelay(true);
                d.relay_hub().announce(&d, device, port, stream);
            }
        });
        Ok(json!({ "port": port, "already_open": false }))
    }

    fn announce(&self, daemon: &Arc<Daemon>, device: DeviceId, port: u16, stream: TcpStream) {
        let id = {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            if pending.values().filter(|p| p.device == device).count() >= MAX_PENDING_PER_DEVICE {
                return; // home isn't picking connections up; drop instead of piling up
            }
            let id = hex::encode(rand::random::<[u8; 16]>());
            pending.insert(id.clone(), Pending { device, stream });
            id
        };
        daemon
            .events
            .publish(&topic_for(&device), json!({ "conn": id, "port": port }));
        let d = Arc::clone(daemon);
        tokio::spawn(async move {
            tokio::time::sleep(PENDING_TTL).await;
            if let Ok(mut p) = d.relay_hub().pending.lock() {
                p.remove(&id);
            }
        });
    }

    /// Closes `port` if `device` owns it.
    pub fn close(&self, device: DeviceId, port: u16) -> bool {
        let Ok(mut l) = self.listeners.lock() else {
            return false;
        };
        if l.get(&port).is_some_and(|x| x.device == device)
            && let Some(x) = l.remove(&port)
        {
            x.cancel.cancel();
            return true;
        }
        false
    }

    /// Closes every port of a device (on revocation).
    pub fn close_device(&self, device: DeviceId) {
        if let Ok(mut l) = self.listeners.lock() {
            l.retain(|_, x| {
                if x.device == device {
                    x.cancel.cancel();
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Relayed ports and active player connections.
    #[must_use]
    pub fn list(&self) -> Value {
        let ports: Vec<Value> = self
            .listeners
            .lock()
            .map(|l| {
                let mut v: Vec<_> = l
                    .iter()
                    .map(|(p, x)| json!({ "port": p, "label": x.label, "device": x.device }))
                    .collect();
                v.sort_by_key(|x| x["port"].as_u64());
                v
            })
            .unwrap_or_default();
        json!({ "ports": ports, "active_connections": self.active.load(Ordering::Relaxed) })
    }

    fn take(&self, device: DeviceId, conn: &str) -> Option<TcpStream> {
        let mut p = self.pending.lock().ok()?;
        if p.get(conn).is_some_and(|x| x.device == device) {
            return p.remove(conn).map(|x| x.stream);
        }
        None
    }
}

/// Serves a data connection on the hub (after the hello): attach to a waiting player
/// connection, then forward bytes.
pub async fn serve_data<S>(daemon: Arc<Daemon>, mut conn: FramedConn<S>, principal: Principal)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Principal::Device { id: device, scopes } = principal else {
        return;
    };
    let Ok(Ok(Some(Message::Req {
        id, cmd, params, ..
    }))) = tokio::time::timeout(Duration::from_secs(10), conn.recv()).await
    else {
        return;
    };
    let reply = |outcome| Message::Res { id, outcome };
    if cmd != "relay.attach" || !scopes.contains(&Capability::Relay) {
        let _ = conn
            .send(&reply(Outcome::Err(err(
                ErrorCode::Forbidden,
                "not a relay request",
            ))))
            .await;
        return;
    }
    let hub = daemon.relay_hub();
    let Some(player) = params["conn"].as_str().and_then(|c| hub.take(device, c)) else {
        let _ = conn
            .send(&reply(Outcome::Err(err(
                ErrorCode::NotFound,
                "that connection is gone",
            ))))
            .await;
        return;
    };
    if conn.send(&reply(Outcome::Ok(json!({})))).await.is_err() {
        return;
    }
    let active = Arc::clone(&hub.active);
    active.fetch_add(1, Ordering::Relaxed);
    let (stream, leftover) = conn.into_parts();
    splice(player, stream, leftover).await;
    active.fetch_sub(1, Ordering::Relaxed);
}

// =========================================================================================
// Home side: the client
// =========================================================================================

/// Connection status of the home side.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ClientStatus {
    /// Control connection is up.
    pub connected: bool,
    /// Last problem, in words.
    pub last_error: Option<String>,
    /// Public port → error opening it (None = open).
    pub ports: BTreeMap<u16, Option<String>>,
}

/// The home daemon's relay client.
#[derive(Default)]
pub struct RelayClient {
    status: Mutex<ClientStatus>,
    run: Mutex<Option<CancellationToken>>,
    active: Arc<AtomicUsize>,
}

/// Stored configuration (settings key `relay_client`).
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct ClientConfig {
    /// VPS remote-control address, `host:port`.
    pub address: String,
    /// The VPS daemon's certificate fingerprint (pinned).
    pub fingerprint: String,
    /// Our device ID on the VPS.
    pub device: DeviceId,
    /// Instance → public port on the VPS.
    #[serde(default)]
    pub ports: BTreeMap<InstanceId, u16>,
}

impl ClientConfig {
    /// Host part of `address` (what friends connect to).
    #[must_use]
    pub fn host(&self) -> String {
        match self.address.rsplit_once(':') {
            Some((h, _)) => h.trim_start_matches('[').trim_end_matches(']').to_owned(),
            None => self.address.clone(),
        }
    }

    /// What friends type for a relayed server.
    #[must_use]
    pub fn share_address(&self, port: u16) -> String {
        msm_engine::domains::share_address(&self.host(), port)
    }
}

impl RelayClient {
    fn set(&self, f: impl FnOnce(&mut ClientStatus)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> ClientStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Active player connections through the relay.
    #[must_use]
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// (Re)starts the client from the stored configuration (stops it if there is none).
    pub fn restart(daemon: &Arc<Daemon>) {
        let me = daemon.relay_client();
        if let Ok(mut run) = me.run.lock() {
            if let Some(old) = run.take() {
                old.cancel();
            }
            *me.status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = ClientStatus::default();
            let Some(cfg) = load_config(daemon) else {
                return;
            };
            let cancel = CancellationToken::new();
            *run = Some(cancel.clone());
            let d = Arc::clone(daemon);
            tokio::spawn(async move { client_loop(d, cfg, cancel).await });
        }
    }
}

/// The stored home-side configuration.
#[must_use]
pub fn load_config(daemon: &Daemon) -> Option<ClientConfig> {
    daemon
        .setting(CLIENT_SETTING)
        .and_then(|v| serde_json::from_value(v).ok())
}

/// Saves (or clears) the home-side configuration.
///
/// # Errors
/// Storage errors.
pub fn save_config(daemon: &Daemon, cfg: Option<&ClientConfig>) -> Result<(), ApiError> {
    let v = cfg.map_or(Value::Null, |c| json!(c));
    daemon.put_setting(CLIENT_SETTING, &v)
}

fn hello(name: &str) -> Message {
    Message::Hello {
        proto: [msm_api::PROTOCOL_MIN_SUPPORTED, msm_api::PROTOCOL_MAJOR],
        client: SoftwareInfo {
            name: name.into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        resume: BTreeMap::new(),
    }
}

type Tls = FramedConn<remote::ClientStream>;

/// Connects to the VPS with our identity, pinning its certificate, and completes the hello.
async fn dial(
    daemon: &Daemon,
    address: &str,
    fingerprint: &str,
    client: &str,
) -> Result<Tls, String> {
    let identity = crate::remote_server::load_identity(daemon)?;
    let (tls, _) = tokio::time::timeout(
        Duration::from_secs(15),
        remote::connect(address, &identity, Some(fingerprint)),
    )
    .await
    .map_err(|_| format!("{address} didn't answer"))??;
    let mut conn = FramedConn::new(tls, MAX_CONTROL_FRAME);
    conn.send(&hello(client)).await.map_err(|e| e.to_string())?;
    match tokio::time::timeout(Duration::from_secs(15), conn.recv()).await {
        // An unpaired (or revoked) certificate only gets a pairing session.
        Ok(Ok(Some(Message::HelloOk { features, .. }))) if features.iter().any(|f| f == "pairing") => Err(
            "the relay no longer accepts this server (it was removed on the VPS); set the relay up again".into(),
        ),
        Ok(Ok(Some(Message::HelloOk { .. }))) => Ok(conn),
        Ok(Ok(Some(Message::Bye { error }))) => Err(error.detail),
        _ => Err("the relay closed the connection".into()),
    }
}

async fn request(conn: &mut Tls, cmd: &str, params: Value) -> Result<Value, ApiError> {
    let id = RequestId::new();
    conn.send(&Message::Req {
        id,
        cmd: cmd.into(),
        params,
        idempotency_key: None,
    })
    .await
    .map_err(|e| err(ErrorCode::UpstreamError, e.to_string()))?;
    loop {
        match tokio::time::timeout(Duration::from_secs(30), conn.recv()).await {
            Ok(Ok(Some(Message::Res { id: rid, outcome }))) if rid == id => {
                return match outcome {
                    Outcome::Ok(v) => Ok(v),
                    Outcome::Err(e) => Err(e),
                };
            }
            Ok(Ok(Some(_))) => {}
            _ => {
                return Err(err(
                    ErrorCode::UpstreamError,
                    "the relay closed the connection",
                ));
            }
        }
    }
}

/// Pairs this daemon with a relay (VPS) daemon using a one-time code; returns our device ID.
///
/// # Errors
/// Readable reasons (wrong code, unreachable, identity mismatch).
pub async fn pair(
    daemon: &Daemon,
    address: &str,
    fingerprint: &str,
    code: &str,
) -> Result<DeviceId, ApiError> {
    let identity =
        crate::remote_server::load_identity(daemon).map_err(|e| err(ErrorCode::Internal, e))?;
    let (tls, server_fp) = tokio::time::timeout(
        Duration::from_secs(15),
        remote::connect(address, &identity, Some(fingerprint)),
    )
    .await
    .map_err(|_| err(ErrorCode::UpstreamError, format!("{address} didn't answer")))?
    .map_err(|e| err(ErrorCode::UpstreamError, e))?;
    let mut conn = FramedConn::new(tls, MAX_CONTROL_FRAME);
    conn.send(&hello(CONTROL_CLIENT))
        .await
        .map_err(|e| err(ErrorCode::UpstreamError, e.to_string()))?;
    let _ = conn.recv().await;
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "home".into());
    let proof = remote::client_proof(code, &server_fp, &identity.fingerprint());
    let v = request(
        &mut conn,
        "pair.complete",
        json!({ "proof": proof, "name": format!("Relay: {name}") }),
    )
    .await?;
    let expected = remote::server_proof(code, &server_fp, &identity.fingerprint());
    if !remote::verify_proof(&expected, v["server_proof"].as_str().unwrap_or_default()) {
        return Err(err(
            ErrorCode::Unauthenticated,
            "the relay couldn't prove it knows the code",
        ));
    }
    v["device"]
        .as_str()
        .and_then(|d| d.parse().ok())
        .ok_or_else(|| {
            err(
                ErrorCode::UpstreamError,
                "the relay didn't return a device ID",
            )
        })
}

async fn client_loop(daemon: Arc<Daemon>, cfg: ClientConfig, cancel: CancellationToken) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let result = control_session(&daemon, &cfg, &cancel).await;
        let me = daemon.relay_client();
        me.set(|s| {
            s.connected = false;
            if let Err(e) = &result {
                s.last_error = Some(e.clone());
            }
        });
        if cancel.is_cancelled() {
            return;
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            () = cancel.cancelled() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn control_session(
    daemon: &Arc<Daemon>,
    cfg: &ClientConfig,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let mut conn = dial(daemon, &cfg.address, &cfg.fingerprint, CONTROL_CLIENT).await?;
    conn.send(&Message::Sub {
        topic: topic_for(&cfg.device),
        from_seq: None,
    })
    .await
    .map_err(|e| e.to_string())?;
    // Open the public ports; replies are matched by request ID in the loop below.
    let mut opening: HashMap<RequestId, u16> = HashMap::new();
    for (instance, port) in &cfg.ports {
        let label = daemon
            .instance_name(*instance)
            .await
            .unwrap_or_else(|| instance.to_string());
        let id = RequestId::new();
        conn.send(&Message::Req {
            id,
            cmd: "relay.open".into(),
            params: json!({ "port": port, "label": label }),
            idempotency_key: None,
        })
        .await
        .map_err(|e| e.to_string())?;
        opening.insert(id, *port);
    }
    let me = daemon.relay_client();
    me.set(|s| {
        s.connected = true;
        s.last_error = None;
    });
    let mut ping = tokio::time::interval(Duration::from_secs(25));
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            _ = ping.tick() => {
                conn.send(&Message::Ping { nonce: 0 }).await.map_err(|e| e.to_string())?;
            }
            msg = conn.recv() => match msg {
                Ok(Some(Message::Res { id, outcome })) => {
                    if let Some(port) = opening.remove(&id) {
                        let e = match outcome { Outcome::Ok(_) => None, Outcome::Err(e) => Some(e.detail) };
                        me.set(|s| { s.ports.insert(port, e); });
                    }
                }
                Ok(Some(Message::Event { data, .. })) => {
                    let (Some(conn_id), Some(port)) = (data["conn"].as_str(), data["port"].as_u64()) else { continue };
                    let Some(instance) = cfg.ports.iter().find(|(_, p)| u64::from(**p) == port).map(|(i, _)| *i) else { continue };
                    let (d, c, conn_id) = (Arc::clone(daemon), cfg.clone(), conn_id.to_owned());
                    tokio::spawn(async move { data_connection(d, c, instance, conn_id).await });
                }
                Ok(Some(Message::Bye { error })) => return Err(error.detail),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return Err("lost the connection to the relay".into()),
            }
        }
    }
}

async fn data_connection(
    daemon: Arc<Daemon>,
    cfg: ClientConfig,
    instance: InstanceId,
    conn_id: String,
) {
    let local = match daemon.instance_port(instance).await {
        Some(port) => tokio::time::timeout(
            Duration::from_secs(5),
            TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .ok()
        .and_then(Result::ok),
        None => None,
    };
    let Ok(mut conn) = dial(&daemon, &cfg.address, &cfg.fingerprint, DATA_CLIENT).await else {
        return;
    };
    if request(&mut conn, "relay.attach", json!({ "conn": conn_id }))
        .await
        .is_err()
    {
        return;
    }
    // If the server isn't running, the player's connection was claimed and is now closed, so
    // the game says it can't connect right away instead of waiting for a timeout.
    let Some(local) = local else { return };
    let _ = local.set_nodelay(true);
    let active = Arc::clone(&daemon.relay_client().active);
    active.fetch_add(1, Ordering::Relaxed);
    let (stream, leftover) = conn.into_parts();
    splice(local, stream, leftover).await;
    active.fetch_sub(1, Ordering::Relaxed);
}

impl std::fmt::Debug for RelayHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelayHub")
    }
}

impl std::fmt::Debug for RelayClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelayClient")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_address_uses_the_relay_host() {
        let cfg = ClientConfig {
            address: "203.0.113.9:47625".into(),
            fingerprint: String::new(),
            device: DeviceId::new(),
            ports: BTreeMap::new(),
        };
        assert_eq!(cfg.host(), "203.0.113.9");
        assert_eq!(cfg.share_address(25565), "203.0.113.9");
        assert_eq!(cfg.share_address(25570), "203.0.113.9:25570");
        let v6 = ClientConfig {
            address: "[2001:db8::1]:47625".into(),
            ..cfg
        };
        assert_eq!(v6.host(), "2001:db8::1");
    }
}
