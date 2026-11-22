//! Authorization scopes (see `planning/04-data-and-security.md` §4).

use serde::{Deserialize, Serialize};

/// A permission a principal must hold to run a command. Local owners hold all of them; paired
/// devices hold the subset granted at pairing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// List and read state, logs, findings.
    View,
    /// Start, stop, restart.
    Control,
    /// Send console commands, quick commands and macros.
    ConsoleWrite,
    /// Op/de-op, kick, ban/pardon, whitelist.
    Moderate,
    /// Typed and raw configuration.
    Configure,
    /// Mods, plugins, modpacks.
    Content,
    /// Create and verify backups.
    Backup,
    /// Restore backups (destructive).
    Restore,
    /// Firewall, UPnP, relay client configuration.
    Network,
    /// DNS planning/apply and DNS credentials.
    Domains,
    /// Create/delete instances, manage Java runtimes.
    Install,
    /// Act as an external reachability probe for another host.
    Probe,
    /// On a relay host: manage relay grants.
    Relay,
    /// Pairing, devices, daemon settings.
    Admin,
}

impl Capability {
    /// Every capability.
    pub const ALL: &'static [Capability] = &[
        Self::View,
        Self::Control,
        Self::ConsoleWrite,
        Self::Moderate,
        Self::Configure,
        Self::Content,
        Self::Backup,
        Self::Restore,
        Self::Network,
        Self::Domains,
        Self::Install,
        Self::Probe,
        Self::Relay,
        Self::Admin,
    ];
}

/// The authenticated party behind a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    /// The OS user who owns the daemon, connected over local IPC.
    LocalOwner,
    /// A paired remote controller.
    Device {
        /// Device ID.
        id: msm_core::ids::DeviceId,
        /// Granted scopes.
        scopes: Vec<Capability>,
    },
    /// Internal jobs (DDNS, scheduled tasks).
    System,
}

impl Principal {
    /// Whether this principal holds `cap`.
    #[must_use]
    pub fn has(&self, cap: Capability) -> bool {
        match self {
            Self::LocalOwner | Self::System => true,
            Self::Device { scopes, .. } => scopes.contains(&cap),
        }
    }

    /// Audit label: `local-owner`, `device:<id>` or `system`.
    #[must_use]
    pub fn audit_label(&self) -> String {
        match self {
            Self::LocalOwner => "local-owner".into(),
            Self::Device { id, .. } => format!("device:{id}"),
            Self::System => "system".into(),
        }
    }
}
