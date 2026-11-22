//! `msm-transport`: how clients reach a daemon. A security boundary (threat model T7, T8).
//!
//! * Local IPC: a user-private Windows named pipe ([`windows_pipe`]) or Unix socket
//!   ([`unix_socket`]), with peer-identity checks on both ends. There is deliberately **no**
//!   loopback TCP listener, so browsers and other local users cannot reach the daemon.
//! * [`framed::FramedConn`] turns any byte stream into a stream of protocol messages.
//! * [`remote`] — TLS 1.3 with pinned self-signed certificates and one-time pairing codes.
#![deny(unsafe_code)]

pub mod framed;
pub mod remote;
#[cfg(unix)]
pub mod unix_socket;
#[cfg(windows)]
pub mod windows_pipe;

pub use framed::{ConnError, FramedConn};
