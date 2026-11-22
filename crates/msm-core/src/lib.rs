//! `msm-core`: domain types shared by every other crate.
//!
//! This crate is deliberately free of I/O so that its logic can be unit- and property-tested
//! exhaustively. See `planning/03-system-architecture.md` §3.
#![forbid(unsafe_code)]

pub mod error;
pub mod ids;
pub mod instance_state;
pub mod mc;

pub use error::{ErrorCode, MsmError};
