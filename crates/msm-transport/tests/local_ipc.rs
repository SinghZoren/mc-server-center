//! End-to-end tests of the local IPC transport on the current OS.
#![allow(clippy::unwrap_used)]

use msm_api::Message;
use msm_api::codec::MAX_CONTROL_FRAME;
use msm_transport::FramedConn;

#[cfg(windows)]
fn unique_pipe() -> String {
    format!(r"\\.\pipe\msm-test-{}", msm_core::ids::RequestId::new())
}

#[cfg(windows)]
#[tokio::test]
async fn pipe_round_trip_same_user() {
    use msm_transport::windows_pipe::{PipeListener, connect};
    let name = unique_pipe();
    let mut listener = PipeListener::bind(&name).unwrap();
    let server = tokio::spawn(async move {
        let conn = listener.accept().await.unwrap();
        let mut framed = FramedConn::new(conn, MAX_CONTROL_FRAME);
        let msg = framed.recv().await.unwrap().unwrap();
        let Message::Ping { nonce } = msg else {
            panic!("unexpected {msg:?}")
        };
        framed.send(&Message::Pong { nonce }).await.unwrap();
    });
    let client = connect(&name).await.unwrap();
    let mut framed = FramedConn::new(client, MAX_CONTROL_FRAME);
    framed.send(&Message::Ping { nonce: 42 }).await.unwrap();
    assert_eq!(
        framed.recv().await.unwrap(),
        Some(Message::Pong { nonce: 42 })
    );
    server.await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn pipe_name_cannot_be_squatted() {
    use msm_transport::windows_pipe::PipeListener;
    let name = unique_pipe();
    let _first = PipeListener::bind(&name).unwrap();
    // A second "first instance" with the same name must fail, whoever creates it.
    assert!(PipeListener::bind(&name).is_err());
}

#[cfg(windows)]
#[tokio::test]
async fn pipe_serves_multiple_clients_in_sequence() {
    use msm_transport::windows_pipe::{PipeListener, connect};
    let name = unique_pipe();
    let mut listener = PipeListener::bind(&name).unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let conn = listener.accept().await.unwrap();
            let mut framed = FramedConn::new(conn, MAX_CONTROL_FRAME);
            if let Some(Message::Ping { nonce }) = framed.recv().await.unwrap() {
                framed.send(&Message::Pong { nonce }).await.unwrap();
            }
        }
    });
    for n in 0..3 {
        let mut framed = FramedConn::new(connect(&name).await.unwrap(), MAX_CONTROL_FRAME);
        framed.send(&Message::Ping { nonce: n }).await.unwrap();
        assert_eq!(
            framed.recv().await.unwrap(),
            Some(Message::Pong { nonce: n })
        );
    }
    server.await.unwrap();
}

#[cfg(windows)]
#[test]
fn current_user_sid_looks_like_a_sid() {
    let sid = msm_transport::windows_pipe::current_user_sid().unwrap();
    assert!(sid.starts_with("S-1-"), "{sid}");
    let name = msm_transport::windows_pipe::default_pipe_name().unwrap();
    assert!(name.starts_with(r"\\.\pipe\msm-S-1-"));
}

#[cfg(unix)]
#[tokio::test]
async fn socket_round_trip_and_single_daemon() {
    use msm_transport::unix_socket::{SocketListener, connect};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("msmd.sock");
    let mut listener = SocketListener::bind(&path).await.unwrap();
    assert!(
        SocketListener::bind(&path).await.is_err(),
        "second daemon must not bind"
    );
    let server = tokio::spawn(async move {
        let conn = listener.accept().await.unwrap();
        let mut framed = FramedConn::new(conn, MAX_CONTROL_FRAME);
        if let Some(Message::Ping { nonce }) = framed.recv().await.unwrap() {
            framed.send(&Message::Pong { nonce }).await.unwrap();
        }
    });
    let mut framed = FramedConn::new(connect(&path).await.unwrap(), MAX_CONTROL_FRAME);
    framed.send(&Message::Ping { nonce: 7 }).await.unwrap();
    assert_eq!(
        framed.recv().await.unwrap(),
        Some(Message::Pong { nonce: 7 })
    );
    server.await.unwrap();
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::metadata(&path).map(|m| assert_eq!(m.permissions().mode() & 0o777, 0o600));
}

#[cfg(unix)]
#[tokio::test]
async fn stale_socket_is_replaced() {
    use msm_transport::unix_socket::SocketListener;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("msmd.sock");
    // A leftover socket file with nobody listening.
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    assert!(SocketListener::bind(&path).await.is_ok());
}

#[tokio::test]
async fn framed_conn_rejects_oversized_frames() {
    let (a, b) = tokio::io::duplex(1024);
    let mut sender = FramedConn::new(a, MAX_CONTROL_FRAME);
    let mut receiver = FramedConn::new(b, 16);
    sender.send(&Message::Ping { nonce: 1 }).await.unwrap();
    assert!(receiver.recv().await.is_err());
}
