//! On-disk layout of the daemon's data directory (see `planning/03-system-architecture.md` §6).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use msm_core::ids::InstanceId;

/// Folder name used under the per-user application-data directory.
/// TODO(Q-04): rename together with the product name.
pub const APP_DIR_NAME: &str = "MinecraftServerManager";

/// The daemon's data directory and the well-known paths inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// Uses an explicit root (tests, `--data-dir`).
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The default location for this OS:
    /// * Windows: `%LOCALAPPDATA%\MinecraftServerManager`
    /// * Linux under systemd: `$STATE_DIRECTORY` (e.g. `/var/lib/msm`)
    /// * Linux otherwise: `$XDG_STATE_HOME/msm`, falling back to `~/.local/state/msm`
    ///
    /// # Errors
    /// `NotFound` if the relevant environment variables are missing.
    pub fn default_for_os() -> io::Result<Self> {
        let missing =
            |var: &str| io::Error::new(io::ErrorKind::NotFound, format!("{var} is not set"));
        if cfg!(windows) {
            let base = std::env::var_os("LOCALAPPDATA").ok_or_else(|| missing("LOCALAPPDATA"))?;
            return Ok(Self::at(PathBuf::from(base).join(APP_DIR_NAME)));
        }
        if let Some(state) = std::env::var_os("STATE_DIRECTORY") {
            // systemd may pass several colon-separated directories; ours is the first.
            let first = state
                .to_string_lossy()
                .split(':')
                .next()
                .unwrap_or_default()
                .to_owned();
            return Ok(Self::at(first));
        }
        if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
            return Ok(Self::at(PathBuf::from(xdg).join("msm")));
        }
        let home = std::env::var_os("HOME").ok_or_else(|| missing("HOME"))?;
        Ok(Self::at(PathBuf::from(home).join(".local/state/msm")))
    }

    /// The root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// SQLite database.
    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.root.join("state.db")
    }
    /// Pre-migration database copies.
    #[must_use]
    pub fn db_backups(&self) -> PathBuf {
        self.root.join("db-backups")
    }
    /// Daemon logs.
    #[must_use]
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    /// Content-addressed download cache.
    #[must_use]
    pub fn blob_cache(&self) -> PathBuf {
        self.root.join("cache").join("blobs").join("sha256")
    }
    /// Managed Java runtimes.
    #[must_use]
    pub fn java_runtimes(&self) -> PathBuf {
        self.root.join("runtimes").join("java")
    }
    /// In-progress downloads.
    #[must_use]
    pub fn tmp_downloads(&self) -> PathBuf {
        self.root.join("tmp").join("downloads")
    }
    /// Staging area for installs and restores (same volume as `instances/`).
    #[must_use]
    pub fn tmp_stage(&self) -> PathBuf {
        self.root.join("tmp").join("stage")
    }
    /// All instances.
    #[must_use]
    pub fn instances(&self) -> PathBuf {
        self.root.join("instances")
    }
    /// One instance's directory (manifest, `server/`, backups, logs).
    #[must_use]
    pub fn instance(&self, id: InstanceId) -> PathBuf {
        self.instances().join(id.to_string())
    }
    /// File-based secret store (Linux fallback, ADR-003).
    #[must_use]
    pub fn secrets(&self) -> PathBuf {
        self.root.join("secrets")
    }
    /// Daemon TLS identity (public parts).
    #[must_use]
    pub fn tls(&self) -> PathBuf {
        self.root.join("tls")
    }
    /// Local IPC socket directory (Unix).
    #[must_use]
    pub fn run(&self) -> PathBuf {
        self.root.join("run")
    }
    /// Single-instance lock file.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }

    /// Creates the directory tree. On Unix, the root, `secrets/` and `run/` are made private
    /// to the daemon user (0700).
    ///
    /// # Errors
    /// Any I/O error creating directories or setting permissions.
    pub fn ensure(&self) -> io::Result<()> {
        for dir in [
            self.root.clone(),
            self.db_backups(),
            self.logs(),
            self.blob_cache(),
            self.java_runtimes(),
            self.tmp_downloads(),
            self.tmp_stage(),
            self.instances(),
            self.secrets(),
            self.tls(),
            self.run(),
        ] {
            std::fs::create_dir_all(&dir)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [self.root.clone(), self.secrets(), self.run()] {
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }

    /// Takes the exclusive daemon lock. Only one daemon may own a data directory.
    ///
    /// # Errors
    /// [`LockError::AlreadyRunning`] if another process holds it.
    pub fn lock(&self) -> Result<DaemonLock, LockError> {
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())
            .map_err(LockError::Io)?;
        match file.try_lock() {
            Ok(()) => Ok(DaemonLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(LockError::AlreadyRunning),
            Err(std::fs::TryLockError::Error(e)) => Err(LockError::Io(e)),
        }
    }
}

/// Holds the exclusive lock until dropped (or the process exits).
#[derive(Debug)]
pub struct DaemonLock {
    _file: File,
}

/// Failure to take the daemon lock.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another daemon already owns this data directory.
    #[error("another daemon is already using this data directory")]
    AlreadyRunning,
    /// The lock file could not be opened.
    #[error("could not open lock file: {0}")]
    Io(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_creates_tree_and_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path().join("data"));
        data.ensure().unwrap();
        assert!(data.instances().is_dir());
        assert!(data.tmp_stage().is_dir());
        assert!(data.blob_cache().is_dir());

        let first = data.lock().unwrap();
        assert!(matches!(data.lock(), Err(LockError::AlreadyRunning)));
        drop(first);
        assert!(data.lock().is_ok());
    }

    #[test]
    fn instance_paths_use_the_id() {
        let data = DataDir::at("/x");
        let id = InstanceId::new();
        assert_eq!(
            data.instance(id),
            Path::new("/x").join("instances").join(id.to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_dirs_are_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::at(dir.path().join("d"));
        data.ensure().unwrap();
        for d in [data.root().to_owned(), data.secrets()] {
            let mode = std::fs::metadata(d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }
}
