//! Confining file access to a root directory.

use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

use msm_core::{ErrorCode, MsmError};

use crate::RelPath;

/// Why a path could not be resolved inside a [`Root`].
#[derive(Debug, thiserror::Error)]
pub enum ConfineError {
    /// The root does not exist, is not a directory, or could not be canonicalized.
    #[error("invalid root directory {path:?}: {reason}")]
    InvalidRoot {
        /// The root that was supplied.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
    },
    /// A component of the path is a symlink, junction or other reparse point.
    #[error("path {0:?} traverses a link or reparse point")]
    TraversesLink(String),
    /// An intermediate component exists but is not a directory.
    #[error("path {0:?} is not a directory")]
    NotADirectory(String),
    /// The path (or one of its parents) does not exist and the mode requires it to.
    #[error("path {0:?} does not exist")]
    NotFound(String),
    /// Another I/O error while inspecting the path.
    #[error("I/O error at {path:?}: {source}")]
    Io {
        /// The relative prefix being inspected.
        path: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl From<ConfineError> for MsmError {
    fn from(e: ConfineError) -> Self {
        let code = match &e {
            ConfineError::TraversesLink(_) => ErrorCode::PathTraversesLink,
            ConfineError::NotFound(_) => ErrorCode::NotFound,
            ConfineError::NotADirectory(_) => ErrorCode::PathRejected,
            ConfineError::InvalidRoot { .. } | ConfineError::Io { .. } => ErrorCode::Internal,
        };
        MsmError::new(code, e.to_string())
    }
}

/// Whether the target (and its parents) must already exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existence {
    /// Every component must exist (reading, deleting, replacing).
    MustExist,
    /// Missing trailing components are fine (creating files or directories). Every component
    /// that *does* exist is still checked.
    MayBeMissing,
}

/// A canonical directory that file operations are confined to (e.g. an instance's `server/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    path: PathBuf,
}

impl Root {
    /// Opens `path` as a root. The path is canonicalized, so a root reached *through* a link
    /// is pinned to its real location; links *inside* the root are refused by [`Root::resolve`].
    ///
    /// # Errors
    /// [`ConfineError::InvalidRoot`] if the path does not exist or is not a directory.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ConfineError> {
        let given = path.as_ref();
        let invalid = |reason: String| ConfineError::InvalidRoot {
            path: given.to_owned(),
            reason,
        };
        let canonical = std::fs::canonicalize(given).map_err(|e| invalid(e.to_string()))?;
        let meta = std::fs::metadata(&canonical).map_err(|e| invalid(e.to_string()))?;
        if !meta.is_dir() {
            return Err(invalid("not a directory".into()));
        }
        Ok(Self { path: canonical })
    }

    /// The canonical root path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolves `rel` to an absolute path inside the root, walking each component and refusing
    /// symlinks, junctions and other reparse points.
    ///
    /// **Limitation:** this check-then-use approach is subject to time-of-check/time-of-use
    /// races if something else modifies the tree concurrently. Instance directories are owned by
    /// the daemon user; handle-relative operations (`openat2`/`NtCreateFile`) are a planned
    /// hardening step (see `planning/04-data-and-security.md`, T2).
    ///
    /// # Errors
    /// See [`ConfineError`].
    pub fn resolve(&self, rel: &RelPath, existence: Existence) -> Result<PathBuf, ConfineError> {
        let mut current = self.path.clone();
        let mut prefix = String::new();
        let components: Vec<&str> = rel.components().collect();
        let last = components.len() - 1;

        for (i, component) in components.iter().enumerate() {
            current.push(component);
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);

            match std::fs::symlink_metadata(&current) {
                Ok(meta) => {
                    if is_link_or_reparse_point(&meta) {
                        return Err(ConfineError::TraversesLink(prefix));
                    }
                    if i < last && !meta.is_dir() {
                        return Err(ConfineError::NotADirectory(prefix));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return match existence {
                        Existence::MustExist => Err(ConfineError::NotFound(prefix)),
                        // Nothing below a missing component can exist, so no link can hide there.
                        Existence::MayBeMissing => Ok(rel.join_onto(&self.path)),
                    };
                }
                Err(source) => {
                    return Err(ConfineError::Io {
                        path: prefix,
                        source,
                    });
                }
            }
        }
        Ok(current)
    }
}

/// True for symlinks and, on Windows, any reparse point (junctions, mount points, cloud
/// placeholders, app-exec links). We do not try to tell "safe" reparse points apart.
fn is_link_or_reparse_point(meta: &Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}
