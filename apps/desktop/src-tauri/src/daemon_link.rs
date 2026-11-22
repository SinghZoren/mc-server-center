//! The desktop's connections to daemons (ADR-001, ADR-002, ADR-003).
//!
//! * **Local**: finds the daemon through `<data dir>/run/endpoint`; if nothing answers, launches
//!   `msmd` detached so it keeps running after this window closes (LIFE-03).
//! * **Remote**: TLS 1.3 to a paired Linux (or other) host, with the host's certificate pinned
//!   and this desktop's own certificate as its identity.
//! * One background task per host owns the connection: requests are correlated by ID, events are
//!   forwarded to the webview as `msm-event` and connection changes as `msm-connection`, both
//!   tagged with the host ID. On disconnect it reconnects with backoff and re-subscribes every
//!   topic from the last seen sequence number.
//!
//! The webview never sees endpoints, keys or certificates.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use msm_api::codec::MAX_CONTROL_FRAME;
use msm_api::{ApiError, Message, Outcome, PROTOCOL_MAJOR, PROTOCOL_MIN_SUPPORTED, SoftwareInfo};
use msm_core::ErrorCode;
use msm_core::ids::RequestId;
use msm_storage::layout::DataDir;
use msm_transport::FramedConn;
use msm_transport::remote::Identity;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
pub type Conn = FramedConn<Box<dyn Stream>>;

/// What a link connects to.
#[derive(Clone)]
pub enum Target {
    /// The daemon on this PC.
    Local,
    /// A paired remote daemon.
    Remote {
        /// `host:port`.
        address: String,
        /// Pinned certificate fingerprint.
        fingerprint: String,
        /// This desktop's identity.
        identity: Identity,
    },
}

/// Requests from Tauri commands to the connection task.
pub enum Outgoing {
    Request {
        cmd: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, ApiError>>,
    },
    Subscribe {
        topic: String,
        from_seq: Option<u64>,
    },
}

/// Event payload forwarded to the webview.
#[derive(Debug, Clone, Serialize)]
pub struct UiEvent {
    pub host: String,
    pub topic: String,
    pub seq: u64,
    pub data: Value,
    /// Set when events were missed: `[from, to]`.
    pub gap: Option<[u64; 2]>,
}

/// Connection state forwarded to the webview.
#[derive(Debug, Clone, Serialize)]
pub struct UiConnection {
    pub host: String,
    pub state: &'static str,
    pub detail: Option<String>,
}

/// Handle stored in Tauri state (one per host).
#[derive(Clone)]
pub struct DaemonLink {
    tx: mpsc::Sender<Outgoing>,
}

impl DaemonLink {
    pub fn start(app: AppHandle, host: String, target: Target) -> Self {
        let (tx, rx) = mpsc::channel(256);
        tauri::async_runtime::spawn(run(app, host, target, rx));
        Self { tx }
    }

    pub async fn request(&self, cmd: String, params: Value) -> Result<Value, ApiError> {
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(Outgoing::Request { cmd, params, reply })
            .await
            .map_err(|_| ApiError::new(ErrorCode::Internal, "connection task stopped"))?;
        tokio::time::timeout(Duration::from_secs(120), wait)
            .await
            .map_err(|_| {
                ApiError::new(
                    ErrorCode::Internal,
                    "the MC Server Center service did not answer in time",
                )
            })?
            .map_err(|_| ApiError::new(ErrorCode::Internal, "the connection was lost"))?
    }

    pub async fn subscribe(&self, topic: String, from_seq: Option<u64>) {
        let _ = self.tx.send(Outgoing::Subscribe { topic, from_seq }).await;
    }
}

/// Last known connection state per host, so a window that opens later can ask for it.
#[derive(Default)]
pub struct ConnStates(pub std::sync::Mutex<HashMap<String, UiConnection>>);

fn status(app: &AppHandle, host: &str, state: &'static str, detail: Option<String>) {
    use tauri::Manager;
    let ui = UiConnection {
        host: host.to_owned(),
        state,
        detail: detail.clone(),
    };
    if let Some(states) = app.try_state::<ConnStates>()
        && let Ok(mut map) = states.0.lock()
    {
        map.insert(host.to_owned(), ui);
    }
    let _ = app.emit(
        "msm-connection",
        UiConnection {
            host: host.to_owned(),
            state,
            detail,
        },
    );
}

/// Locates `msmd` next to this executable (installed layout and `target/debug` in development).
fn daemon_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) { "msmd.exe" } else { "msmd" };
    let candidate = exe.parent()?.join(name);
    candidate.is_file().then_some(candidate)
}

/// Starts the daemon so that it outlives this process.
fn launch_daemon() -> std::io::Result<()> {
    let exe = daemon_exe().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "msmd was not found next to the app",
        )
    })?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        if cmd.spawn().is_ok() {
            return Ok(());
        }
        // Breakaway can be denied inside some job objects; fall back without it.
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().map(|_| ())
}

async fn try_local(data: &DataDir) -> std::io::Result<Box<dyn Stream>> {
    let endpoint = std::fs::read_to_string(data.run().join("endpoint"))?;
    #[cfg(windows)]
    let s: Box<dyn Stream> = Box::new(msm_transport::windows_pipe::connect(endpoint.trim()).await?);
    #[cfg(unix)]
    let s: Box<dyn Stream> =
        Box::new(msm_transport::unix_socket::connect(std::path::Path::new(endpoint.trim())).await?);
    Ok(s)
}

/// Set while the app is quitting, so the reconnect loop doesn't start the service again.
pub static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

async fn open_local(app: &AppHandle, host: &str) -> Result<Box<dyn Stream>, String> {
    let data = DataDir::default_for_os().map_err(|e| e.to_string())?;
    if let Ok(s) = try_local(&data).await {
        return Ok(s);
    }
    if QUITTING.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("the app is quitting".into());
    }
    status(
        app,
        host,
        "starting",
        Some("Starting the MC Server Center service…".into()),
    );
    launch_daemon().map_err(|e| format!("could not start the service: {e}"))?;
    let mut last = String::new();
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match try_local(&data).await {
            Ok(s) => return Ok(s),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("the service did not start: {last}"))
}

/// Opens a stream and completes the hello.
/// Opens a stream and completes the hello; returns the connection and the daemon's version.
pub async fn handshake_versioned(stream: Box<dyn Stream>) -> Result<(Conn, String), String> {
    let mut conn = FramedConn::new(stream, MAX_CONTROL_FRAME);
    conn.send(&Message::Hello {
        proto: [PROTOCOL_MIN_SUPPORTED, PROTOCOL_MAJOR],
        client: SoftwareInfo {
            name: "msm-desktop".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        resume: BTreeMap::new(),
    })
    .await
    .map_err(|e| e.to_string())?;
    match conn.recv().await {
        Ok(Some(Message::HelloOk { daemon, .. })) => Ok((conn, daemon.version)),
        Ok(Some(Message::Bye { error })) if error.code == ErrorCode::IncompatibleProtocol => Err(
            "This app and the MC Server Center service are different versions. Update both to the same version.".into(),
        ),
        Ok(Some(Message::Bye { error })) => Err(error.detail),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// Opens a stream and completes the hello.
pub async fn handshake(stream: Box<dyn Stream>) -> Result<Conn, String> {
    handshake_versioned(stream).await.map(|(c, _)| c)
}

/// `a < b` for dotted numeric versions (pre-release suffixes ignored).
fn version_older(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['-', '+'])
            .next()
            .unwrap_or_default()
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parse(a) < parse(b)
}

/// One request on a raw connection (used before the connection task takes over).
async fn raw_request(conn: &mut Conn, cmd: &str) -> Result<Value, ApiError> {
    let id = RequestId::new();
    let req = Message::Req {
        id,
        cmd: cmd.into(),
        params: Value::Null,
        idempotency_key: None,
    };
    conn.send(&req)
        .await
        .map_err(|e| ApiError::new(ErrorCode::Internal, e.to_string()))?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match conn.recv().await {
                Ok(Some(Message::Res { id: rid, outcome })) if rid == id => {
                    return match outcome {
                        Outcome::Ok(v) => Ok(v),
                        Outcome::Err(e) => Err(e),
                    };
                }
                Ok(Some(_)) => {}
                _ => return Err(ApiError::new(ErrorCode::Internal, "connection closed")),
            }
        }
    })
    .await
    .map_err(|_| ApiError::new(ErrorCode::Internal, "no answer"))?
}

/// Whether two paths name the same file (case-insensitive on Windows).
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| {
        let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
        let s = p.to_string_lossy().into_owned();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    norm(a) == norm(b)
}

/// Connects; for this PC, also makes sure the service is the one shipped next to this app.
///
/// A different `msmd` can still be running: the previous version after an update (servers keep
/// it in use, so the installer moves it aside instead of replacing it), or one from another
/// install or a development build. Such a service is asked to step down; the reconnect then
/// starts the right one, which re-attaches to the running servers. Services too old to step
/// down are left running and the user is told what to do (returned as a warning).
async fn connect(
    app: &AppHandle,
    host: &str,
    target: &Target,
) -> Result<(Conn, Option<String>), String> {
    let Target::Remote {
        address,
        fingerprint,
        identity,
    } = target
    else {
        let (mut conn, version) = handshake_versioned(open_local(app, host).await?).await?;
        let Some(ours) = daemon_exe() else {
            return Ok((conn, None));
        };
        let running = raw_request(&mut conn, "system.info")
            .await
            .ok()
            .and_then(|v| v["exe"].as_str().map(PathBuf::from));
        let stale = version_older(&version, env!("CARGO_PKG_VERSION"))
            || !running.as_deref().is_some_and(|p| same_file(p, &ours));
        if !stale {
            return Ok((conn, None));
        }
        status(
            app,
            host,
            "starting",
            Some("Switching to this version's service…".into()),
        );
        if raw_request(&mut conn, "system.shutdown").await.is_ok() {
            drop(conn);
            // Wait for it to release the data directory lock.
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let conn = handshake(open_local(app, host).await?).await?;
            return Ok((conn, None));
        }
        let from = running
            .map(|p| format!(" (from {})", p.display()))
            .unwrap_or_default();
        let warning = format!(
            "An older MC Server Center service is still running{from}, so some features won't work.              Stop your servers, quit this app from the tray, end \"msmd.exe\" in Task Manager, then open the app again."
        );
        return Ok((conn, Some(warning)));
    };
    let (tls, _) = msm_transport::remote::connect(address, identity, Some(fingerprint)).await?;
    Ok((handshake(Box::new(tls)).await?, None))
}

async fn run(app: AppHandle, host: String, target: Target, mut rx: mpsc::Receiver<Outgoing>) {
    // topic -> last seq seen, so re-subscription after a reconnect resumes where we were.
    let mut cursors: HashMap<String, Option<u64>> = HashMap::new();
    let mut backoff = Duration::from_millis(250);

    loop {
        status(&app, &host, "connecting", None);
        let (mut conn, warning) = match connect(&app, &host, &target).await {
            Ok(c) => c,
            Err(detail) => {
                status(&app, &host, "disconnected", Some(detail));
                // Fail queued requests instead of letting them hang while we retry.
                let deadline = tokio::time::sleep(backoff);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        () = &mut deadline => break,
                        msg = rx.recv() => match msg {
                            Some(Outgoing::Request { reply, .. }) => {
                                let _ = reply.send(Err(ApiError::new(ErrorCode::Internal, "not connected")));
                            }
                            Some(Outgoing::Subscribe { topic, from_seq }) => { cursors.entry(topic).or_insert(from_seq); }
                            None => return,
                        }
                    }
                }
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            }
        };
        backoff = Duration::from_millis(250);
        match warning {
            Some(w) => status(&app, &host, "outdated", Some(w)),
            None => status(&app, &host, "connected", None),
        }
        for (topic, seq) in &cursors {
            let _ = conn
                .send(&Message::Sub {
                    topic: topic.clone(),
                    from_seq: *seq,
                })
                .await;
        }

        let mut pending: HashMap<RequestId, oneshot::Sender<Result<Value, ApiError>>> =
            HashMap::new();
        loop {
            tokio::select! {
                incoming = conn.recv() => match incoming {
                    Ok(Some(Message::Res { id, outcome })) => {
                        if let Some(reply) = pending.remove(&id) {
                            let _ = reply.send(match outcome { Outcome::Ok(v) => Ok(v), Outcome::Err(e) => Err(e) });
                        }
                    }
                    Ok(Some(Message::Event { topic, seq, data })) => {
                        cursors.insert(topic.clone(), Some(seq));
                        let _ = app.emit("msm-event", UiEvent { host: host.clone(), topic, seq, data, gap: None });
                    }
                    Ok(Some(Message::Gap { topic, from, to })) => {
                        cursors.insert(topic.clone(), Some(to));
                        let _ = app.emit("msm-event", UiEvent { host: host.clone(), topic, seq: to, data: Value::Null, gap: Some([from, to]) });
                    }
                    Ok(Some(Message::Bye { error })) => {
                        status(&app, &host, "disconnected", Some(error.detail));
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                },
                out = rx.recv() => match out {
                    Some(Outgoing::Request { cmd, params, reply }) => {
                        let id = RequestId::new();
                        if conn.send(&Message::Req { id, cmd, params, idempotency_key: None }).await.is_err() {
                            let _ = reply.send(Err(ApiError::new(ErrorCode::Internal, "connection lost")));
                            break;
                        }
                        pending.insert(id, reply);
                    }
                    Some(Outgoing::Subscribe { topic, from_seq }) => {
                        let resume = cursors.get(&topic).copied().flatten().or(from_seq);
                        cursors.insert(topic.clone(), resume);
                        if conn.send(&Message::Sub { topic, from_seq: resume.or(Some(0)) }).await.is_err() {
                            break;
                        }
                    }
                    None => return,
                }
            }
        }
        for (_, reply) in pending.drain() {
            let _ = reply.send(Err(ApiError::new(
                ErrorCode::Internal,
                "the connection was lost",
            )));
        }
        status(&app, &host, "disconnected", Some("Reconnecting…".into()));
        tokio::time::sleep(backoff).await;
    }
}
