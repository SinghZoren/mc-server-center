//! `msm-fsguard`: the file-system security boundary.
//!
//! * [`RelPath`] — a validated, normalized, portable path *relative to an instance root*. Every
//!   path that arrives from a client, an archive or a modpack must become a `RelPath` before it
//!   touches the disk. Construction rejects traversal, absolute/drive/UNC paths, Windows
//!   alternate data streams, reserved device names, 8.3 short-name aliases and other tricks.
//! * [`Root`] — a canonical directory that resolves `RelPath`s while refusing to traverse
//!   symlinks, junctions or other reparse points.
//! * [`archive`] — safe ZIP/tar.gz extraction with path, link, duplicate and size limits.
//! * [`write_atomic`] / [`swap_dir`] — crash-safe replacement of files and directories.
//!
//! Threats addressed: T1 and T2 in `planning/04-data-and-security.md`.
#![forbid(unsafe_code)]

pub mod archive;
mod atomic;
mod confine;
mod relpath;

pub use atomic::{SwappedDir, rename_with_retry, swap_dir, unswap_dir, write_atomic};
pub use confine::{ConfineError, Existence, Root};
pub use relpath::{PathError, RelPath};
