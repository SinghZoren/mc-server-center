//! Minecraft server process supervision, v1: the daemon owns the JVM directly (ADR-001 "P1").
//! M2 moves this behind a per-instance runner process so a daemon restart keeps the console.
//!
//! Responsibilities: spawn the JVM with a **cleared environment** (no secrets leak into mods),
//! stream stdout/stderr into a bounded, sequence-numbered ring buffer plus a log file, detect
//! readiness, send console lines safely, stop gracefully with a timeout, and kill only when told.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{broadcast, watch};

/// `CREATE_NO_WINDOW`: no console window flashes up for the JVM on Windows.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Number of console lines kept in memory per server.
pub const RING_CAPACITY: usize = 5000;
/// Maximum accepted length of one console line from the server (longer lines are truncated).
const MAX_LINE_BYTES: usize = 16 * 1024;

/// Environment variables passed through to the JVM. Everything else is removed.
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "SYSTEMROOT",
    "WINDIR",
    "TEMP",
    "TMP",
    "HOME",
    "USERPROFILE",
    "LANG",
    "LC_ALL",
    "TZ",
    "APPDATA",
    "LOCALAPPDATA",
];

/// How to start a server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    /// Java executable.
    pub java: PathBuf,
    /// JVM options, e.g. `-Xms1G`, `-Xmx4G`.
    pub jvm_args: Vec<String>,
    /// What to run.
    pub target: LaunchTarget,
    /// Arguments after the main jar/class, e.g. `nogui`.
    pub server_args: Vec<String>,
    /// Regex matching the "server is ready" console line.
    pub ready_pattern: String,
}

/// Main jar or argument files (Forge/NeoForge).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaunchTarget {
    /// `java … -jar <path> …` (path relative to the working directory).
    Jar {
        /// Jar path relative to the server directory.
        path: String,
    },
    /// `java … @a.txt @b.txt …` (paths relative to the working directory).
    ArgFiles {
        /// Arg-file paths relative to the server directory.
        paths: Vec<String>,
    },
}

/// Ready line printed by Vanilla, Paper and Fabric: `Done (3.21s)! For help, type "help"`.
pub const DEFAULT_READY_PATTERN: &str = r#"Done \([0-9.,]+s\)! For help, type "help""#;

impl LaunchSpec {
    fn command_args(&self) -> Vec<String> {
        let mut args = self.jvm_args.clone();
        match &self.target {
            LaunchTarget::Jar { path } => {
                args.push("-jar".into());
                args.push(path.clone());
            }
            LaunchTarget::ArgFiles { paths } => {
                args.extend(paths.iter().map(|p| format!("@{p}")));
            }
        }
        args.extend(self.server_args.iter().cloned());
        args
    }
}

/// One console line with its sequence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleLine {
    /// Monotonic sequence number (starts at 1).
    pub seq: u64,
    /// Line text (lossy UTF-8, no trailing newline).
    pub text: String,
    /// Whether it came from stderr.
    pub stderr: bool,
}

/// Errors from supervision.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// Could not start the JVM.
    #[error("failed to start server: {0}")]
    Spawn(#[source] std::io::Error),
    /// The ready pattern is invalid.
    #[error("invalid ready pattern: {0}")]
    BadPattern(#[from] regex::Error),
    /// A console line contained a line break or control character.
    #[error("console input must be a single line without control characters")]
    InvalidInput,
    /// The console is closed (process exited).
    #[error("console is closed")]
    ConsoleClosed,
    /// Graceful stop did not finish in time.
    #[error("server did not stop within {0:?}")]
    StopTimeout(Duration),
    /// I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Default)]
struct Ring {
    lines: VecDeque<ConsoleLine>,
    next_seq: u64,
}

impl Ring {
    fn push(&mut self, text: String, stderr: bool) -> ConsoleLine {
        self.next_seq += 1;
        let line = ConsoleLine {
            seq: self.next_seq,
            text,
            stderr,
        };
        if self.lines.len() == RING_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line.clone());
        line
    }
}

/// A running Minecraft server.
#[derive(Debug)]
pub struct ServerProcess {
    child: Arc<tokio::sync::Mutex<Child>>,
    pid: Option<u32>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    ring: Arc<Mutex<Ring>>,
    lines_tx: broadcast::Sender<ConsoleLine>,
    ready_rx: watch::Receiver<bool>,
    exit_rx: watch::Receiver<Option<Option<i32>>>,
}

fn sanitize_env(cmd: &mut tokio::process::Command) {
    let keep: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| ENV_ALLOWLIST.iter().any(|a| a.eq_ignore_ascii_case(k)))
        .collect();
    cmd.env_clear();
    cmd.envs(keep);
}

impl ServerProcess {
    /// Starts the server in `working_dir`, appending console output to `log_path`.
    ///
    /// # Errors
    /// [`ProcessError::Spawn`] or [`ProcessError::BadPattern`].
    pub async fn spawn(
        spec: &LaunchSpec,
        working_dir: &Path,
        log_path: &Path,
    ) -> Result<Self, ProcessError> {
        let ready_re = Regex::new(&spec.ready_pattern)?;
        let mut cmd = tokio::process::Command::new(&spec.java);
        cmd.args(spec.command_args())
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        sanitize_env(&mut cmd);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        let mut child = cmd.spawn().map_err(ProcessError::Spawn)?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let log = Arc::new(tokio::sync::Mutex::new(
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
                .await?,
        ));
        let ring = Arc::new(Mutex::new(Ring::default()));
        let (lines_tx, _) = broadcast::channel(1024);
        let (ready_tx, ready_rx) = watch::channel(false);
        let ready_tx = Arc::new(ready_tx);

        let mut pumps = Vec::new();
        for (reader, is_err) in [
            (
                stdout.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                false,
            ),
            (
                stderr.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                true,
            ),
        ] {
            let Some(reader) = reader else { continue };
            let ring = ring.clone();
            let tx = lines_tx.clone();
            let log = log.clone();
            let ready_tx = ready_tx.clone();
            let ready_re = ready_re.clone();
            pumps.push(tokio::spawn(async move {
                let mut reader = BufReader::new(reader);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    buf.truncate(MAX_LINE_BYTES);
                    let text = String::from_utf8_lossy(&buf)
                        .trim_end_matches(['\r', '\n'])
                        .to_owned();
                    {
                        let mut f = log.lock().await;
                        let _ = f.write_all(text.as_bytes()).await;
                        let _ = f.write_all(b"\n").await;
                    }
                    if !is_err && ready_re.is_match(&text) {
                        let _ = ready_tx.send(true);
                    }
                    let line = ring.lock().map(|mut r| r.push(text, is_err));
                    if let Ok(line) = line {
                        let _ = tx.send(line);
                    }
                }
            }));
        }

        let child = Arc::new(tokio::sync::Mutex::new(child));
        let (exit_tx, exit_rx) = watch::channel(None);
        let waiter = child.clone();
        tokio::spawn(async move {
            // Poll rather than hold the lock across `wait`, so `kill` can take it.
            let status = loop {
                {
                    let mut c = waiter.lock().await;
                    if let Ok(Some(status)) = c.try_wait() {
                        break status;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            };
            // Let the pumps drain the final output before announcing exit.
            for p in pumps {
                let _ = p.await;
            }
            let _ = exit_tx.send(Some(status.code()));
        });

        Ok(Self {
            child,
            pid,
            stdin: tokio::sync::Mutex::new(stdin),
            ring,
            lines_tx,
            ready_rx,
            exit_rx,
        })
    }

    /// OS process ID.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Subscribes to new console lines, returning buffered lines after `after_seq` first.
    /// If lines between `after_seq` and the oldest buffered line were dropped, the first
    /// returned line's `seq` reveals the gap.
    #[must_use]
    pub fn subscribe(
        &self,
        after_seq: u64,
    ) -> (Vec<ConsoleLine>, broadcast::Receiver<ConsoleLine>) {
        // Subscribe before snapshotting so no line falls between the two.
        let rx = self.lines_tx.subscribe();
        let backlog = self
            .ring
            .lock()
            .map(|r| {
                r.lines
                    .iter()
                    .filter(|l| l.seq > after_seq)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        (backlog, rx)
    }

    /// Sends one console line (without the leading `/`).
    ///
    /// # Errors
    /// [`ProcessError::InvalidInput`] for multi-line/control input; [`ProcessError::ConsoleClosed`].
    pub async fn send_line(&self, line: &str) -> Result<(), ProcessError> {
        if line.is_empty() || line.chars().any(char::is_control) {
            return Err(ProcessError::InvalidInput);
        }
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().ok_or(ProcessError::ConsoleClosed)?;
        let mut data = line.as_bytes().to_vec();
        data.push(b'\n');
        stdin
            .write_all(&data)
            .await
            .map_err(|_| ProcessError::ConsoleClosed)?;
        stdin
            .flush()
            .await
            .map_err(|_| ProcessError::ConsoleClosed)?;
        Ok(())
    }

    /// Whether the ready line has been seen.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        *self.ready_rx.borrow()
    }

    /// A receiver that flips to `true` when the server is ready.
    #[must_use]
    pub fn ready_watch(&self) -> watch::Receiver<bool> {
        self.ready_rx.clone()
    }

    /// A receiver that becomes `Some(exit code)` when the process exits.
    #[must_use]
    pub fn exit_watch(&self) -> watch::Receiver<Option<Option<i32>>> {
        self.exit_rx.clone()
    }

    /// Waits until the ready line appears, the process exits, or the timeout elapses.
    /// Returns `true` only if the server became ready.
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

    /// Waits for the process to exit; returns its exit code (`None` if killed by a signal).
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

    /// Returns the exit code if the process has exited.
    #[must_use]
    pub fn exit_code(&self) -> Option<Option<i32>> {
        *self.exit_rx.borrow()
    }

    /// Graceful stop: sends `stop` and waits up to `timeout`.
    ///
    /// # Errors
    /// [`ProcessError::StopTimeout`] if it is still running; the caller must ask the user before
    /// calling [`ServerProcess::kill`].
    pub async fn stop(&self, timeout: Duration) -> Result<Option<i32>, ProcessError> {
        if let Some(code) = self.exit_code() {
            return Ok(code);
        }
        // If stdin is already closed the process is exiting anyway.
        let _ = self.send_line("stop").await;
        tokio::time::timeout(timeout, self.wait_exit())
            .await
            .map_err(|_| ProcessError::StopTimeout(timeout))
    }

    /// Force-kills the process. Only call after explicit user confirmation.
    ///
    /// # Errors
    /// I/O errors from the OS.
    pub async fn kill(&self) -> Result<Option<i32>, ProcessError> {
        {
            let mut c = self.child.lock().await;
            if c.try_wait()?.is_none() {
                c.start_kill()?;
            }
        }
        Ok(self.wait_exit().await)
    }
}

/// Converts an exit status to a code for the state machine.
#[must_use]
pub fn exit_code(status: ExitStatus) -> Option<i32> {
    status.code()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_args_for_jar_and_argfiles() {
        let mut spec = LaunchSpec {
            java: "java".into(),
            jvm_args: vec!["-Xmx2G".into()],
            target: LaunchTarget::Jar {
                path: "server.jar".into(),
            },
            server_args: vec!["nogui".into()],
            ready_pattern: DEFAULT_READY_PATTERN.into(),
        };
        assert_eq!(
            spec.command_args(),
            ["-Xmx2G", "-jar", "server.jar", "nogui"]
        );
        spec.target = LaunchTarget::ArgFiles {
            paths: vec![
                "user_jvm_args.txt".into(),
                "libraries/x/win_args.txt".into(),
            ],
        };
        assert_eq!(
            spec.command_args(),
            [
                "-Xmx2G",
                "@user_jvm_args.txt",
                "@libraries/x/win_args.txt",
                "nogui"
            ]
        );
    }

    #[test]
    fn ready_pattern_matches_real_lines() {
        let re = Regex::new(DEFAULT_READY_PATTERN).unwrap();
        assert!(
            re.is_match(r#"[14:02:11] [Server thread/INFO]: Done (3.123s)! For help, type "help""#)
        );
        assert!(re.is_match(r#"[14:02:11 INFO]: Done (12,5s)! For help, type "help""#));
        assert!(!re.is_match("<Steve> Done (1s)! For help, type help"));
    }

    #[test]
    fn ring_is_bounded_and_sequenced() {
        let mut r = Ring::default();
        for i in 0..(RING_CAPACITY + 10) {
            r.push(i.to_string(), false);
        }
        assert_eq!(r.lines.len(), RING_CAPACITY);
        assert_eq!(r.lines.front().unwrap().seq, 11);
        assert_eq!(r.lines.back().unwrap().seq, (RING_CAPACITY + 10) as u64);
    }
}
