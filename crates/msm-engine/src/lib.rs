//! `msm-engine`: application services used by the daemon.
//!
//! * [`backups`] — verified ZIP backups, verification and non-destructive restore.
//! * [`diagnostics`] — deterministic crash explanations from console output.
//! * [`java`] — discover and probe Java runtimes; choose one for a server.
//! * [`java_install`] — download, verify and install a managed Temurin JRE.
//! * [`process`] — supervise a running Minecraft server (console, ready detection, stop/kill).
//! * [`players`] — online tracking, player lists, moderation (console or offline file edits).
//! * [`quick_commands`] — built-in quick actions (version-gated) and user macros.
//! * [`vanilla`] — staged Vanilla install, instance manifest, EULA acceptance.
#![forbid(unsafe_code)]

pub mod backups;
pub mod content;
pub mod diagnostics;
pub mod domains;
pub mod java;
pub mod java_install;
pub mod mrpack;
pub mod network;
pub mod players;
pub mod process;
pub mod quick_commands;
pub mod settings;
pub mod vanilla;
