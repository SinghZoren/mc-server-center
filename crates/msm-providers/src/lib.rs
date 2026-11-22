//! `msm-providers`: everything that talks to the internet.
//!
//! * [`http`] — the download engine and egress policy every adapter must use.
//! * Upstream adapters (one module each) translate provider APIs into our domain types and are
//!   contract-tested against recorded fixtures.
#![forbid(unsafe_code)]

pub mod adoptium;
pub mod cloudflare;
pub mod fabric;
pub mod http;
pub mod modrinth;
pub mod mojang;
pub mod paper;
