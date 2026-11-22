//! Crash-safe file and directory replacement.
//!
//! * [`write_atomic`] writes a file so that readers (and a crash) only ever see the old or the
//!   new complete contents, never a torn file.
//! * [`swap_dir`] replaces a directory with a staged one, keeping the previous version so the
//!   caller (normally the operation journal) can roll back.
//!
//! Both require the temporary/staged data to live on the **same volume** as the target so the
//! final step is a rename, never a copy.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::{ConfineError, Existence, RelPath, Root};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// How many times a rename is retried when Windows reports the target as in use (antivirus,
/// indexer or Explorer holding a handle), and the initial backoff.
const RENAME_RETRIES: u32 = 6;
const RENAME_BACKOFF: Duration = Duration::from_millis(50);

fn temp_sibling(target: &Path, tag: &str) -> io::Result<PathBuf> {
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "target has no parent directory",
        )
    })?;
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"))?
        .to_string_lossy();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(".{name}.msm-{tag}-{}-{n}", std::process::id())))
}

/// Renames `from` to `to`, retrying briefly on the transient "in use" errors Windows returns
/// when another process holds a handle. On Windows `std::fs::rename` replaces an existing file
/// (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`).
///
/// # Errors
/// The last I/O error if every attempt fails.
pub fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    let mut delay = RENAME_BACKOFF;
    let mut attempt = 0;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < RENAME_RETRIES && is_transient(&e) => {
                std::thread::sleep(delay);
                delay *= 2;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn is_transient(e: &io::Error) -> bool {
    // ERROR_ACCESS_DENIED (5) and ERROR_SHARING_VIOLATION (32) are what Windows reports when a
    // scanner briefly holds the file. On Unix these do not occur transiently, so do not retry.
    cfg!(windows) && (e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(32))
}

/// Flushes directory metadata so a completed rename survives power loss (Unix only; on Windows
/// NTFS journals the rename and directories cannot be opened for syncing this way).
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Atomically replaces (or creates) `target` with `contents`.
///
/// Writes to a temporary sibling, fsyncs it, renames it over the target and fsyncs the parent
/// directory. If anything fails the temporary file is removed and the original is untouched.
///
/// # Errors
/// Any I/O error; the target is unchanged in that case.
pub fn write_atomic(target: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = temp_sibling(target, "tmp")?;
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        rename_with_retry(&tmp, target)?;
        if let Some(parent) = target.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Result of a successful [`swap_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwappedDir {
    /// Where the previous contents of the target now live, if the target existed. The caller
    /// decides when to delete this (after the change is verified).
    pub previous: Option<PathBuf>,
}

/// Replaces directory `target` with directory `staged` (same volume).
///
/// 1. If `target` exists, it is renamed to `previous` (which must not exist yet).
/// 2. `staged` is renamed to `target`.
///
/// If step 2 fails, step 1 is undone, so on error the target is exactly as before.
///
/// # Errors
/// I/O errors from either rename. `AlreadyExists` if `previous` is already taken.
pub fn swap_dir(staged: &Path, target: &Path, previous: &Path) -> io::Result<SwappedDir> {
    if !staged.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "staged directory does not exist",
        ));
    }
    if fs::symlink_metadata(previous).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "previous path already exists",
        ));
    }
    let had_target = fs::symlink_metadata(target).is_ok();
    if had_target {
        rename_with_retry(target, previous)?;
    }
    if let Err(e) = rename_with_retry(staged, target) {
        if had_target {
            // Best effort rollback; report the original error.
            let _ = rename_with_retry(previous, target);
        }
        return Err(e);
    }
    if let Some(parent) = target.parent() {
        sync_dir(parent)?;
    }
    Ok(SwappedDir {
        previous: had_target.then(|| previous.to_owned()),
    })
}

/// Undoes a [`swap_dir`]: moves the new `target` to `discard` and restores `previous`.
///
/// # Errors
/// I/O errors; on failure of the second rename the first is undone.
pub fn unswap_dir(target: &Path, previous: &Path, discard: &Path) -> io::Result<()> {
    rename_with_retry(target, discard)?;
    if let Err(e) = rename_with_retry(previous, target) {
        let _ = rename_with_retry(discard, target);
        return Err(e);
    }
    Ok(())
}

impl Root {
    /// Atomically writes `contents` to `rel` inside this root. The parent directory must exist
    /// and no component may be a link.
    ///
    /// # Errors
    /// [`ConfineError`] for path problems, or [`ConfineError::Io`] for write failures.
    pub fn write_atomic(&self, rel: &RelPath, contents: &[u8]) -> Result<(), ConfineError> {
        if let Some(parent) = rel.parent() {
            self.resolve(&parent, Existence::MustExist)?;
        }
        let target = self.resolve(rel, Existence::MayBeMissing)?;
        write_atomic(&target, contents).map_err(|source| ConfineError::Io {
            path: rel.to_string(),
            source,
        })
    }

    /// Reads a file inside this root, refusing links.
    ///
    /// # Errors
    /// [`ConfineError`] for path problems or read failures.
    pub fn read(&self, rel: &RelPath) -> Result<Vec<u8>, ConfineError> {
        let path = self.resolve(rel, Existence::MustExist)?;
        fs::read(path).map_err(|source| ConfineError::Io {
            path: rel.to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_creates_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("server.properties");
        write_atomic(&target, b"a=1\n").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"a=1\n");
        write_atomic(&target, b"a=2\n").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"a=2\n");
        // No temp files are left behind.
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn failed_write_leaves_original_and_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        // Target is a non-empty directory: the final rename must fail.
        let target = dir.path().join("occupied");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"k").unwrap();
        assert!(write_atomic(&target, b"new").is_err());
        assert_eq!(fs::read(target.join("keep")).unwrap(), b"k");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["occupied".to_string()]);
    }

    #[test]
    fn swap_dir_replaces_and_keeps_previous() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("server");
        let staged = dir.path().join("stage");
        let previous = dir.path().join("server.prev");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("v"), b"old").unwrap();
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("v"), b"new").unwrap();

        let swapped = swap_dir(&staged, &target, &previous).unwrap();
        assert_eq!(swapped.previous.as_deref(), Some(previous.as_path()));
        assert_eq!(fs::read(target.join("v")).unwrap(), b"new");
        assert_eq!(fs::read(previous.join("v")).unwrap(), b"old");
        assert!(!staged.exists());

        let discard = dir.path().join("server.discard");
        unswap_dir(&target, &previous, &discard).unwrap();
        assert_eq!(fs::read(target.join("v")).unwrap(), b"old");
        assert_eq!(fs::read(discard.join("v")).unwrap(), b"new");
    }

    #[test]
    fn swap_dir_into_missing_target() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("stage");
        fs::create_dir(&staged).unwrap();
        let target = dir.path().join("server");
        let swapped = swap_dir(&staged, &target, &dir.path().join("prev")).unwrap();
        assert_eq!(swapped.previous, None);
        assert!(target.is_dir());
    }

    #[test]
    fn swap_dir_refuses_missing_stage_and_taken_previous() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("server");
        fs::create_dir(&target).unwrap();
        let err = swap_dir(&dir.path().join("nope"), &target, &dir.path().join("p")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let staged = dir.path().join("stage");
        fs::create_dir(&staged).unwrap();
        let taken = dir.path().join("taken");
        fs::create_dir(&taken).unwrap();
        let err = swap_dir(&staged, &target, &taken).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(target.is_dir() && staged.is_dir());
    }

    #[test]
    fn root_write_and_read_are_confined() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config")).unwrap();
        let root = Root::open(dir.path()).unwrap();
        let rel = RelPath::parse("config/a.toml").unwrap();
        root.write_atomic(&rel, b"x = 1").unwrap();
        assert_eq!(root.read(&rel).unwrap(), b"x = 1");
        let missing_parent = RelPath::parse("nope/a.toml").unwrap();
        assert!(matches!(
            root.write_atomic(&missing_parent, b"x"),
            Err(ConfineError::NotFound(_))
        ));
    }
}
