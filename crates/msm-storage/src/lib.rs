//! `msm-storage`: the daemon's persistent state.
//!
//! * [`layout::DataDir`] — where everything lives on disk, plus the single-instance lock.
//! * [`db`] — SQLite opening, pragmas and migrations with automatic pre-migration backups.
//! * [`repo`] — small shared repositories (audit trail, settings).
//!
//! The connection type is synchronous `rusqlite`; the daemon owns one writer connection on a
//! dedicated thread (ADR-006).
#![forbid(unsafe_code)]

pub mod db;
pub mod layout;
pub mod repo;

pub use rusqlite;
