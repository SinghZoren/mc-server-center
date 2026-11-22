//! The daemon's handle to a server running under `msmd runner`. Offers the same operations the
//! daemon used on a directly owned process: console subscription with replay, sending lines,
//! readiness, exit, graceful stop and kill.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use msm_api::codec::MAX_CONTROL_FRAME;
use msm_api::{ApiError, Message, Outcome};
use msm_core::ErrorCode;
use msm_core::ids::RequestId;
use msm_engine::process::{ConsoleLine, RING_CAPACITY};
use msm_transport::FramedConn;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::runner::{RunnerInfo, client_hello};

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

type Pending = oneshot::Sender<Result<Value, ApiError>>;

/// Connected runner.
#[derive(Debug)]
pub struct RunnerClient {
    java_pid: Option<u32>,
    req_tx: mpsc::Sender<(String, Value, Pending)>,
    ring: Arc<Mutex<VecDeque<ConsoleLine>>>,
    lines_tx: broadcast::Sender<ConsoleLine>,
    ready_rx: watch::Receiver<bool>,
    exit_rx: watch::Receiver<Option<Option<i32>>>,
}

/// Why a runner could not be attached.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    /// No `runner.json` or it is unreadable.
    #[error("no runner information")]
    NoInfo,
    /// The endpoint did not answer.
    #[error("runner not reachable: {0}")]
    Unreachable(String),
    /// Something answered but did not prove the expected nonce.
    #[error("runner identity mismatch")]
    Identity,
}

/// Reads `runner.json`.
pub fn read_info(path: &Path) -> Option<RunnerInfo> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

async fn open(endpoint: &str) -> std::io::Result<Box<dyn Stream>> {
    #[cfg(windows)]
    let s: Box<dyn Stream> = Box::new(msm_transport::windows_pipe::connect(endpoint).await?);
    #[cfg(unix)]
    let s: Box<dyn Stream> =
        Box::new(msm_transport::unix_socket::connect(Path::new(endpoint)).await?);
    Ok(s)
}

impl RunnerClient {
    /// Connects to a runner described by `info` and verifies its nonce.
    pub async fn attach(info: &RunnerInfo) -> Result<Arc<Self>, AttachError> {
        let stream = open(&info.endpoint)
            .await
            .map_err(|e| AttachError::Unreachable(e.to_string()))?;
        let mut conn = FramedConn::new(stream, MAX_CONTROL_FRAME);
        conn.send(&client_hello(0))
            .await
            .map_err(|e| AttachError::Unreachable(e.to_string()))?;
        match conn.recv().await {
            Ok(Some(Message::HelloOk { daemon, .. }))
                if daemon.name == "msmd-runner" && daemon.version == info.nonce => {}
            Ok(Some(Message::HelloOk { .. })) => return Err(AttachError::Identity),
            other => return Err(AttachError::Unreachable(format!("{other:?}"))),
        }
        let (req_tx, req_rx) = mpsc::channel(64);
        let (lines_tx, _) = broadcast::channel(1024);
        let (ready_tx, ready_rx) = watch::channel(false);
        let (exit_tx, exit_rx) = watch::channel(None);
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(1024)));
        tokio::spawn(pump(
            conn,
            req_rx,
            ring.clone(),
            lines_tx.clone(),
            ready_tx,
            exit_tx,
        ));
        Ok(Arc::new(Self {
            java_pid: info.java_pid,
            req_tx,
            ring,
            lines_tx,
            ready_rx,
            exit_rx,
        }))
    }

    /// Waits for `runner.json` to appear (fresh launch) and attaches.
    pub async fn attach_when_ready(
        info_path: &Path,
        timeout: Duration,
    ) -> Result<Arc<Self>, AttachError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(info) = read_info(info_path) {
                return Self::attach(&info).await;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AttachError::NoInfo);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// JVM process ID.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.java_pid
    }

    /// Buffered lines after `after_seq`, plus a receiver for new ones.
    #[must_use]
    pub fn subscribe(
        &self,
        after_seq: u64,
    ) -> (Vec<ConsoleLine>, broadcast::Receiver<ConsoleLine>) {
        let rx = self.lines_tx.subscribe();
        let backlog = self
            .ring
            .lock()
            .map(|r| r.iter().filter(|l| l.seq > after_seq).cloned().collect())
            .unwrap_or_default();
        (backlog, rx)
    }

    async fn request(&self, cmd: &str, params: Value) -> Result<Value, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.req_tx
            .send((cmd.to_owned(), params, tx))
            .await
            .map_err(|_| {
                ApiError::new(
                    ErrorCode::ConsoleUnavailable,
                    "server process is not connected",
                )
            })?;
        rx.await.map_err(|_| {
            ApiError::new(ErrorCode::ConsoleUnavailable, "server process disconnected")
        })?
    }

    /// Sends one console line.
    pub async fn send_line(&self, line: &str) -> Result<(), ApiError> {
        if line.is_empty() || line.chars().any(char::is_control) {
            return Err(ApiError::new(
                ErrorCode::InvalidArgument,
                "console input must be a single line without control characters",
            ));
        }
        self.request("send", json!({ "line": line }))
            .await
            .map(|_| ())
    }

    /// Waits for readiness (true), exit or timeout (false).
    pub async fn wait_ready(&self, timeout: Duration) -> bool {
        let mut ready = self.ready_rx.clone();
        let mut exit = self.exit_rx.clone();
        let wait = async {
            loop {
                if *ready.borrow() {
                    return true;
                }
                if exit.borrow().is_some() {
                    return false;
                }
                tokio::select! {
                    r = ready.changed() => if r.is_err() { return *ready.borrow(); },
                    e = exit.changed() => if e.is_err() { return false; },
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.unwrap_or(false)
    }

    /// Whether the server reported ready.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        *self.ready_rx.borrow()
    }

    /// Waits for the process to exit.
    pub async fn wait_exit(&self) -> Option<i32> {
        let mut exit = self.exit_rx.clone();
        loop {
            if let Some(code) = *exit.borrow() {
                return code;
            }
            if exit.changed().await.is_err() {
                return None;
            }
        }
    }

    /// Exit code if exited.
    #[must_use]
    pub fn exit_code(&self) -> Option<Option<i32>> {
        *self.exit_rx.borrow()
    }

    /// Graceful stop with timeout.
    pub async fn stop(&self, timeout: Duration) -> Result<Option<i32>, ApiError> {
        if let Some(code) = self.exit_code() {
            return Ok(code);
        }
        let _ = self.request("stop", Value::Null).await;
        tokio::time::timeout(timeout, self.wait_exit())
            .await
            .map_err(|_| {
                ApiError::new(
                    ErrorCode::InvalidStateTransition,
                    "server did not stop in time",
                )
            })
    }

    /// Force kill (after explicit confirmation).
    pub async fn kill(&self) -> Result<Option<i32>, ApiError> {
        if let Some(code) = self.exit_code() {
            return Ok(code);
        }
        let _ = self.request("kill", Value::Null).await;
        Ok(self.wait_exit().await)
    }
}

async fn pump(
    mut conn: FramedConn<Box<dyn Stream>>,
    mut req_rx: mpsc::Receiver<(String, Value, Pending)>,
    ring: Arc<Mutex<VecDeque<ConsoleLine>>>,
    lines_tx: broadcast::Sender<ConsoleLine>,
    ready_tx: watch::Sender<bool>,
    exit_tx: watch::Sender<Option<Option<i32>>>,
) {
    let mut pending: HashMap<RequestId, Pending> = HashMap::new();
    let mut exit_code: Option<i32> = None;
    loop {
        tokio::select! {
            msg = conn.recv() => match msg {
                Ok(Some(Message::Event { topic, seq, data })) => match topic.as_str() {
                    "console" => {
                        let line = ConsoleLine {
                            seq,
                            text: data["text"].as_str().unwrap_or_default().to_owned(),
                            stderr: data["stderr"].as_bool().unwrap_or(false),
                        };
                        if let Ok(mut r) = ring.lock() {
                            if r.len() == RING_CAPACITY {
                                r.pop_front();
                            }
                            r.push_back(line.clone());
                        }
                        let _ = lines_tx.send(line);
                    }
                    "ready" => { let _ = ready_tx.send(true); }
                    "exit" => {
                        exit_code = data["code"].as_i64().and_then(|c| i32::try_from(c).ok());
                        break;
                    }
                    _ => {}
                },
                Ok(Some(Message::Res { id, outcome })) => {
                    if let Some(tx) = pending.remove(&id) {
                        let _ = tx.send(match outcome { Outcome::Ok(v) => Ok(v), Outcome::Err(e) => Err(e) });
                    }
                }
                Ok(Some(_)) => {}
                // Lost the runner without an exit event: it died. Report an unclean exit.
                Ok(None) | Err(_) => break,
            },
            req = req_rx.recv() => match req {
                Some((cmd, params, tx)) => {
                    let id = RequestId::new();
                    if conn.send(&Message::Req { id, cmd, params, idempotency_key: None }).await.is_err() {
                        let _ = tx.send(Err(ApiError::new(ErrorCode::ConsoleUnavailable, "server process disconnected")));
                        break;
                    }
                    pending.insert(id, tx);
                }
                None => break,
            }
        }
    }
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(ApiError::new(
            ErrorCode::ConsoleUnavailable,
            "server process exited",
        )));
    }
    let _ = exit_tx.send(Some(exit_code));
}
