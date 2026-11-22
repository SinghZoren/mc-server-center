//! `msmd runner <spec.json>`: a small per-server process that owns the Minecraft JVM and its
//! console (ADR-001 "P2"). The daemon talks to it over a user-private local endpoint, so the
//! daemon can restart (crash, update) without stopping the server or losing console history.
//!
//! Identity: the runner writes `runner.json` with a random nonce; a client must see that nonce
//! in the runner's hello before trusting it. The endpoint itself is created as a first instance
//! (Windows) or in the private run directory (Unix), so it cannot be squatted.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use msm_api::codec::MAX_CONTROL_FRAME;
use msm_api::{ApiError, Message, Outcome, Principal, SoftwareInfo};
use msm_core::ErrorCode;
use msm_engine::process::{LaunchSpec, ServerProcess};
use msm_transport::FramedConn;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};

/// Written by the daemon for the runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerSpec {
    /// How to start the JVM.
    pub launch: LaunchSpec,
    /// Server working directory.
    pub working_dir: PathBuf,
    /// Console log file.
    pub log_path: PathBuf,
    /// Local endpoint to listen on (pipe name or socket path).
    pub endpoint: String,
    /// Where to write [`RunnerInfo`].
    pub info_path: PathBuf,
}

/// Written by the runner once it is listening.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerInfo {
    /// Runner process ID.
    pub runner_pid: u32,
    /// JVM process ID.
    pub java_pid: Option<u32>,
    /// Endpoint to connect to.
    pub endpoint: String,
    /// Secret the runner proves in its hello.
    pub nonce: String,
    /// When the server was started (Unix ms).
    pub started_at_ms: i64,
}

fn now_ms() -> i64 {
    msm_storage::db::now_ms()
}

/// Entry point for `msmd runner <spec>`.
pub async fn run(spec_path: PathBuf) -> Result<(), String> {
    let spec: RunnerSpec =
        serde_json::from_slice(&std::fs::read(&spec_path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("bad runner spec: {e}"))?;
    let proc = Arc::new(
        ServerProcess::spawn(&spec.launch, &spec.working_dir, &spec.log_path)
            .await
            .map_err(|e| e.to_string())?,
    );
    let nonce = msm_core::ids::RequestId::new().to_string();

    #[cfg(windows)]
    let mut listener = msm_transport::windows_pipe::PipeListener::bind(&spec.endpoint)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    let mut listener =
        msm_transport::unix_socket::SocketListener::bind(std::path::Path::new(&spec.endpoint))
            .await
            .map_err(|e| e.to_string())?;

    let info = RunnerInfo {
        runner_pid: std::process::id(),
        java_pid: proc.pid(),
        endpoint: spec.endpoint.clone(),
        nonce: nonce.clone(),
        started_at_ms: now_ms(),
    };
    msm_fsguard::write_atomic(
        &spec.info_path,
        &serde_json::to_vec(&info).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut stopping = false;
    loop {
        tokio::select! {
            conn = listener.accept() => {
                if let Ok(stream) = conn {
                    tokio::spawn(serve(proc.clone(), stream, nonce.clone()));
                }
            }
            _ = proc.wait_exit() => break,
            _ = &mut ctrl_c, if !stopping => {
                // Host shutdown / service stop: save the world properly.
                stopping = true;
                let p = proc.clone();
                tokio::spawn(async move { let _ = p.stop(Duration::from_secs(90)).await; });
            }
        }
    }
    // Give connected clients a moment to receive the exit event.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let _ = std::fs::remove_file(&spec.info_path);
    Ok(())
}

async fn serve<S>(proc: Arc<ServerProcess>, stream: S, nonce: String)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut conn = FramedConn::new(stream, MAX_CONTROL_FRAME);
    let Ok(Some(Message::Hello { resume, .. })) = conn.recv().await else {
        return;
    };
    let ok = Message::HelloOk {
        proto: 1,
        daemon: SoftwareInfo {
            name: "msmd-runner".into(),
            version: nonce,
        },
        features: vec![],
        principal: Principal::System,
    };
    if conn.send(&ok).await.is_err() {
        return;
    }
    let from = resume.get("console").copied().unwrap_or(0);
    let (backlog, mut rx) = proc.subscribe(from);
    for l in backlog {
        if conn
            .send(&line_event(l.seq, &l.text, l.stderr))
            .await
            .is_err()
        {
            return;
        }
    }
    let mut ready = proc.ready_watch();
    let mut exit = proc.exit_watch();
    let mut ready_sent = false;
    if *ready.borrow() {
        let _ = conn.send(&event("ready", Value::Bool(true))).await;
        ready_sent = true;
    }
    loop {
        tokio::select! {
            msg = conn.recv() => match msg {
                Ok(Some(Message::Req { id, cmd, params, .. })) => {
                    let outcome = handle(&proc, &cmd, &params).await;
                    if conn.send(&Message::Res { id, outcome }).await.is_err() { return; }
                }
                Ok(Some(_)) => {}
                _ => return,
            },
            line = rx.recv() => match line {
                Ok(l) => { if conn.send(&line_event(l.seq, &l.text, l.stderr)).await.is_err() { return; } }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let _ = conn.send(&event("dropped", json!(n))).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
            },
            r = ready.changed(), if !ready_sent => {
                if r.is_err() { ready_sent = true; continue; }
                if *ready.borrow() {
                    ready_sent = true;
                    let _ = conn.send(&event("ready", Value::Bool(true))).await;
                }
            }
            e = exit.changed() => {
                let code = *exit.borrow();
                if e.is_err() || code.is_some() {
                    while let Ok(l) = rx.try_recv() {
                        let _ = conn.send(&line_event(l.seq, &l.text, l.stderr)).await;
                    }
                    let _ = conn.send(&event("exit", json!({ "code": code.flatten() }))).await;
                    return;
                }
            }
        }
    }
}

fn event(topic: &str, data: Value) -> Message {
    Message::Event {
        topic: topic.into(),
        seq: 0,
        data,
    }
}

fn line_event(seq: u64, text: &str, stderr: bool) -> Message {
    Message::Event {
        topic: "console".into(),
        seq,
        data: json!({ "text": text, "stderr": stderr }),
    }
}

async fn handle(proc: &ServerProcess, cmd: &str, params: &Value) -> Outcome {
    let res: Result<Value, ApiError> = match cmd {
        "send" => match params.get("line").and_then(Value::as_str) {
            Some(line) => proc
                .send_line(line)
                .await
                .map(|()| Value::Null)
                .map_err(|e| ApiError::new(ErrorCode::InvalidArgument, e.to_string())),
            None => Err(ApiError::new(ErrorCode::InvalidArgument, "missing line")),
        },
        "stop" => {
            let _ = proc.send_line("stop").await;
            Ok(Value::Null)
        }
        "kill" => proc
            .kill()
            .await
            .map(|c| json!({ "code": c }))
            .map_err(|e| ApiError::new(ErrorCode::Internal, e.to_string())),
        "status" => Ok(
            json!({ "java_pid": proc.pid(), "ready": proc.is_ready(), "exit": proc.exit_code() }),
        ),
        other => Err(ApiError::new(
            ErrorCode::InvalidArgument,
            format!("unknown runner command {other}"),
        )),
    };
    match res {
        Ok(v) => Outcome::Ok(v),
        Err(e) => Outcome::Err(e),
    }
}

/// Hello message a client sends to a runner, resuming the console after `from_seq`.
pub fn client_hello(from_seq: u64) -> Message {
    let mut resume = BTreeMap::new();
    resume.insert("console".to_owned(), from_seq);
    Message::Hello {
        proto: [1, 1],
        client: SoftwareInfo {
            name: "msmd".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        resume,
    }
}
