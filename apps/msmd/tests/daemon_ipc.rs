//! Starts the real `msmd` binary in a temporary data directory and talks to it over local IPC.
//! No network access needed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use msm_api::codec::MAX_CONTROL_FRAME;
use msm_api::{Message, Outcome, SoftwareInfo};
use msm_core::ErrorCode;
use msm_core::ids::RequestId;
use msm_transport::FramedConn;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};

struct Daemon {
    child: Child,
    _dir: tempfile::TempDir,
    endpoint: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(clippy::zombie_processes)] // the Drop impl kills and waits
fn start_daemon() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_msmd"))
        .args(["serve", "--data-dir"])
        .arg(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let endpoint_file = dir.path().join("run").join("endpoint");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(e) = std::fs::read_to_string(&endpoint_file) {
            return Daemon {
                child,
                _dir: dir,
                endpoint: e,
            };
        }
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

async fn raw_connect(endpoint: &str) -> FramedConn<Box<dyn Stream>> {
    #[cfg(windows)]
    let s: Box<dyn Stream> = Box::new(
        msm_transport::windows_pipe::connect(endpoint)
            .await
            .unwrap(),
    );
    #[cfg(unix)]
    let s: Box<dyn Stream> = Box::new(
        msm_transport::unix_socket::connect(std::path::Path::new(endpoint))
            .await
            .unwrap(),
    );
    FramedConn::new(s, MAX_CONTROL_FRAME)
}

fn hello(proto: [u16; 2]) -> Message {
    Message::Hello {
        proto,
        client: SoftwareInfo {
            name: "test".into(),
            version: "0".into(),
        },
        resume: BTreeMap::new(),
    }
}

async fn call(conn: &mut FramedConn<Box<dyn Stream>>, cmd: &str, params: Value) -> Outcome {
    let id = RequestId::new();
    conn.send(&Message::Req {
        id,
        cmd: cmd.into(),
        params,
        idempotency_key: None,
    })
    .await
    .unwrap();
    loop {
        if let Some(Message::Res { id: rid, outcome }) = conn.recv().await.unwrap()
            && rid == id
        {
            return outcome;
        }
    }
}

fn code(o: &Outcome) -> Option<ErrorCode> {
    match o {
        Outcome::Err(e) => Some(e.code),
        Outcome::Ok(_) => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_commands_and_errors() {
    let d = start_daemon();
    let mut conn = raw_connect(&d.endpoint).await;
    conn.send(&hello([1, 1])).await.unwrap();
    let Some(Message::HelloOk {
        proto, principal, ..
    }) = conn.recv().await.unwrap()
    else {
        panic!("no HelloOk")
    };
    assert_eq!(proto, 1);
    assert_eq!(principal, msm_api::Principal::LocalOwner);

    assert_eq!(
        call(&mut conn, "system.ping", Value::Null).await,
        Outcome::Ok(json!("pong"))
    );
    assert_eq!(
        call(&mut conn, "instances.list", Value::Null).await,
        Outcome::Ok(json!([]))
    );
    assert_eq!(
        code(&call(&mut conn, "no.such_command", Value::Null).await),
        Some(ErrorCode::InvalidArgument)
    );
    assert_eq!(
        code(
            &call(
                &mut conn,
                "instances.start",
                json!({"instance": RequestId::new().to_string()})
            )
            .await
        ),
        Some(ErrorCode::NotFound)
    );
    assert_eq!(
        code(
            &call(
                &mut conn,
                "instances.kill",
                json!({"instance": RequestId::new().to_string(), "confirm": false})
            )
            .await
        ),
        Some(ErrorCode::InvalidArgument)
    );
    assert_eq!(
        code(&call(&mut conn, "instances.create", json!({"name": "x"})).await),
        Some(ErrorCode::InvalidArgument),
        "missing fields are rejected before any download"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn incompatible_protocol_is_refused() {
    let d = start_daemon();
    let mut conn = raw_connect(&d.endpoint).await;
    conn.send(&hello([99, 100])).await.unwrap();
    match conn.recv().await.unwrap() {
        Some(Message::Bye { error }) => assert_eq!(error.code, ErrorCode::IncompatibleProtocol),
        other => panic!("expected Bye, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn first_message_must_be_hello() {
    let d = start_daemon();
    let mut conn = raw_connect(&d.endpoint).await;
    conn.send(&Message::Ping { nonce: 1 }).await.unwrap();
    assert!(matches!(
        conn.recv().await.unwrap(),
        Some(Message::Bye { .. })
    ));
}

#[test]
fn second_daemon_on_same_data_dir_is_refused() {
    let d = start_daemon();
    let out = Command::new(env!("CARGO_BIN_EXE_msmd"))
        .args(["serve", "--data-dir"])
        .arg(d._dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already"));
}

/// REM-02/04: pairing with a one-time code, scoped access, single-use codes, and revocation
/// that cuts off a live session.
#[tokio::test(flavor = "multi_thread")]
async fn remote_pairing_scopes_and_revocation() {
    use msm_transport::remote::{Identity, client_proof, connect, server_proof, verify_proof};

    let d = start_daemon();
    let mut local = raw_connect(&d.endpoint).await;
    local.send(&hello([1, 1])).await.unwrap();
    let _ = local.recv().await.unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert!(matches!(
        call(
            &mut local,
            "remote.enable",
            json!({"port": port, "bind": "127.0.0.1"})
        )
        .await,
        Outcome::Ok(_)
    ));
    let Outcome::Ok(code) = call(&mut local, "pair.create_code", json!({"name": "test"})).await
    else {
        panic!()
    };
    let code = code["code"].as_str().unwrap().to_owned();
    let addr = format!("127.0.0.1:{port}");

    async fn session(
        addr: &str,
        id: &Identity,
        pin: Option<&str>,
    ) -> (FramedConn<Box<dyn Stream>>, String) {
        let (tls, fp) = connect(addr, id, pin).await.unwrap();
        let mut c: FramedConn<Box<dyn Stream>> = FramedConn::new(Box::new(tls), MAX_CONTROL_FRAME);
        c.send(&hello([1, 1])).await.unwrap();
        let _ = c.recv().await.unwrap();
        (c, fp)
    }

    // Wrong code is refused.
    let mallory = Identity::generate("mallory").unwrap();
    let (mut m, sfp) = session(&addr, &mallory, None).await;
    let bad = client_proof("AAAAA-BBBBB-CCCCC-DDDDD", &sfp, &mallory.fingerprint());
    assert_eq!(
        code_of(&call(&mut m, "pair.complete", json!({"proof": bad})).await),
        Some(ErrorCode::Unauthenticated)
    );

    // Right code pairs, and the daemon proves it knows the code too.
    let alice = Identity::generate("alice").unwrap();
    let (mut a, sfp) = session(&addr, &alice, None).await;
    let proof = client_proof(&code, &sfp, &alice.fingerprint());
    let Outcome::Ok(r) = call(
        &mut a,
        "pair.complete",
        json!({"proof": proof, "name": "alice"}),
    )
    .await
    else {
        panic!("pairing failed")
    };
    assert!(verify_proof(
        &server_proof(&code, &sfp, &alice.fingerprint()),
        r["server_proof"].as_str().unwrap()
    ));
    let device = r["device"].as_str().unwrap().to_owned();

    // The code is single-use.
    let bob = Identity::generate("bob").unwrap();
    let (mut b, sfp2) = session(&addr, &bob, None).await;
    let reuse = client_proof(&code, &sfp2, &bob.fingerprint());
    assert_eq!(
        code_of(&call(&mut b, "pair.complete", json!({"proof": reuse})).await),
        Some(ErrorCode::Unauthenticated)
    );

    // Paired device: normal commands work, admin commands are forbidden.
    let (mut a, _) = session(&addr, &alice, Some(&sfp)).await;
    assert!(matches!(
        call(&mut a, "instances.list", Value::Null).await,
        Outcome::Ok(_)
    ));
    assert_eq!(
        code_of(&call(&mut a, "devices.list", Value::Null).await),
        Some(ErrorCode::Forbidden)
    );

    // Revocation ends the live session and blocks new ones.
    assert!(matches!(
        call(&mut local, "devices.revoke", json!({"device": device})).await,
        Outcome::Ok(_)
    ));
    let ended = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match a.recv().await {
                Ok(Some(Message::Bye { .. })) | Ok(None) | Err(_) => return true,
                Ok(Some(_)) => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(ended, "live session was not closed on revocation");
    let (mut a2, _) = session(&addr, &alice, Some(&sfp)).await;
    assert_eq!(
        code_of(&call(&mut a2, "instances.list", Value::Null).await),
        Some(ErrorCode::Unauthenticated)
    );
}

fn code_of(o: &Outcome) -> Option<ErrorCode> {
    code(o)
}

/// `system.shutdown` stops the service itself (used for the hand-over after an update).
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_command_stops_the_service() {
    let mut d = start_daemon();
    let mut c = raw_connect(&d.endpoint).await;
    c.send(&hello([1, 1])).await.unwrap();
    let _ = c.recv().await.unwrap();
    assert!(matches!(
        call(&mut c, "system.shutdown", Value::Null).await,
        Outcome::Ok(_)
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = d.child.try_wait().unwrap() {
            assert!(status.success(), "service exited with {status}");
            break;
        }
        assert!(Instant::now() < deadline, "service did not stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
