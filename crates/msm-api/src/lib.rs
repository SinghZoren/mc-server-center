//! `msm-api`: the protocol shared by the daemon and every client (ADR-002).
//!
//! * [`envelope`] — message types and protocol-version negotiation.
//! * [`commands`] — the command registry; each command declares the [`Capability`] it needs.
//! * [`codec`] — length-prefixed JSON framing with size limits.
//!
//! TypeScript bindings for the desktop UI will be generated from these types (task M1-09
//! follow-up, when the desktop app is scaffolded).
#![forbid(unsafe_code)]

pub mod capability;
pub mod codec;
pub mod commands;
pub mod envelope;

pub use capability::{Capability, Principal};
pub use envelope::{
    ApiError, Message, Outcome, PROTOCOL_MAJOR, PROTOCOL_MIN_SUPPORTED, SoftwareInfo,
};
