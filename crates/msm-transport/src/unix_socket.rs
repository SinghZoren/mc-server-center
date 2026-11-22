//! Unix-domain-socket local transport (ADR-002, threat T7).
//!
//! The socket lives in the daemon's private `run/` directory (0700) and is itself 0600. Each
//! accepted connection's peer UID (`SO_PEERCRED`) must equal the daemon's UID; the client checks
//! that the server's UID matches the socket file's owner.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

/// Listens on a private Unix socket.
#[derive(Debug)]
pub struct SocketListener {
    path: PathBuf,
    owner_uid: u32,
    inner: UnixListener,
}

impl SocketListener {
    /// Binds `path`, replacing a stale socket file left by a crashed daemon. Refuses if another
    /// daemon is actively listening there.
    ///
    /// # Errors
    /// `AddrInUse` if a live daemon owns the socket; other I/O errors.
    pub async fn bind(path: &Path) -> io::Result<Self> {
        if std::fs::symlink_metadata(path).is_ok() {
            if UnixStream::connect(path).await.is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "a daemon is already listening",
                ));
            }
            std::fs::remove_file(path)?;
        }
        let inner = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let owner_uid = std::fs::metadata(path)?.uid();
        Ok(Self {
            path: path.to_owned(),
            owner_uid,
            inner,
        })
    }

    /// Accepts a connection from the same UID.
    ///
    /// # Errors
    /// I/O errors, or `PermissionDenied` for a foreign peer.
    pub async fn accept(&mut self) -> io::Result<UnixStream> {
        let (stream, _) = self.inner.accept().await?;
        let peer = stream.peer_cred()?;
        if peer.uid() != self.owner_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "peer runs as a different user",
            ));
        }
        Ok(stream)
    }

    /// The socket path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SocketListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Connects and checks the server's UID matches the socket file's owner.
///
/// # Errors
/// I/O errors, or `PermissionDenied` on mismatch.
pub async fn connect(path: &Path) -> io::Result<UnixStream> {
    let owner = std::fs::metadata(path)?.uid();
    let stream = UnixStream::connect(path).await?;
    if stream.peer_cred()?.uid() != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "server is not the socket owner",
        ));
    }
    Ok(stream)
}
