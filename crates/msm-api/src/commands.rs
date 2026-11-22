//! The command registry: every command name and the capability it requires.
//!
//! The dispatcher refuses any command that is not in this table, and checks the capability
//! centrally before calling a handler. Tests iterate this table to prove every command is denied
//! without its capability (REM-04, PLY-03). Add new commands here first.

use crate::Capability;

/// Static description of a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// Wire name, `area.verb` or `area.sub.verb`.
    pub name: &'static str,
    /// Capability required to call it.
    pub capability: Capability,
    /// Whether it changes state (mutating commands are audited and accept idempotency keys).
    pub mutating: bool,
    /// Whether its parameters contain secrets (never logged or audited verbatim).
    pub sensitive: bool,
}

const fn read(name: &'static str, capability: Capability) -> CommandSpec {
    CommandSpec {
        name,
        capability,
        mutating: false,
        sensitive: false,
    }
}

const fn write(name: &'static str, capability: Capability) -> CommandSpec {
    CommandSpec {
        name,
        capability,
        mutating: true,
        sensitive: false,
    }
}

const fn secret(name: &'static str, capability: Capability) -> CommandSpec {
    CommandSpec {
        name,
        capability,
        mutating: true,
        sensitive: true,
    }
}

use Capability as C;

/// All commands known to this protocol version.
pub const COMMANDS: &[CommandSpec] = &[
    // System
    read("system.ping", C::View),
    read("system.info", C::View),
    write("system.stop_all", C::Control),
    write("system.shutdown", C::Admin),
    read("system.running_count", C::View),
    // Tasks
    read("tasks.list", C::View),
    read("tasks.get", C::View),
    write("tasks.cancel", C::Control),
    // Catalog & Java
    read("catalog.list", C::View),
    read("java.list", C::View),
    write("java.install", C::Install),
    // Instances
    read("instances.list", C::View),
    read("instances.get", C::View),
    write("instances.create", C::Install),
    write("instances.delete", C::Install),
    write("instances.update", C::Configure),
    write("instances.accept_eula", C::Install),
    write("instances.start", C::Control),
    write("instances.stop", C::Control),
    write("instances.restart", C::Control),
    write("instances.kill", C::Control),
    // Console
    read("console.subscribe", C::View),
    write("console.send", C::ConsoleWrite),
    // Players (PLY-01..03)
    read("players.list", C::View),
    write("players.op", C::Moderate),
    write("players.deop", C::Moderate),
    write("players.kick", C::Moderate),
    write("players.ban", C::Moderate),
    write("players.pardon", C::Moderate),
    write("players.ban_ip", C::Moderate),
    write("players.pardon_ip", C::Moderate),
    write("players.whitelist_add", C::Moderate),
    write("players.whitelist_remove", C::Moderate),
    write("players.whitelist_set_enabled", C::Moderate),
    // Quick commands & macros (CMD-01..02)
    read("commands.catalog", C::View),
    read("commands.preview", C::View),
    write("commands.run", C::ConsoleWrite),
    read("macros.list", C::View),
    write("macros.save", C::ConsoleWrite),
    write("macros.delete", C::ConsoleWrite),
    // Mods and plugins (MOD-01..03)
    read("content.search", C::View),
    read("content.plan", C::View),
    write("content.install", C::Content),
    read("content.list", C::View),
    write("content.remove", C::Content),
    // Modpacks (PACK-01..04)
    read("modpacks.search", C::View),
    write("modpacks.install", C::Install),
    // Backups (BAK-01..04)
    read("backups.list", C::View),
    write("backups.create", C::Backup),
    read("backups.verify", C::View),
    write("backups.restore", C::Restore),
    write("backups.delete", C::Restore),
    // Configuration
    read("config.get", C::View),
    write("config.set_typed", C::Configure),
    read("config.raw_get", C::Configure),
    write("config.raw_set", C::Configure),
    // Networking
    read("network.info", C::View),
    read("network.check", C::View),
    write("network.upnp_open", C::Network),
    write("network.upnp_close", C::Network),
    write("network.external_test", C::Network),
    write("network.firewall_allow", C::Network),
    write("network.firewall_remove", C::Network),
    // Diagnostics
    read("diagnostics.list", C::View),
    write("diagnostics.export", C::View),
    // Remote access, pairing and devices (REM-01..05)
    read("remote.status", C::Admin),
    write("remote.enable", C::Admin),
    write("remote.disable", C::Admin),
    write("pair.create_code", C::Admin),
    read("devices.list", C::Admin),
    write("devices.revoke", C::Admin),
    write("devices.rotate", C::View),
    // Domains (DNS-01..05). DNS credentials are sensitive.
    read("domains.status", C::View),
    read("domains.check", C::View),
    read("domains.records", C::View),
    read("domains.zones", C::Domains),
    write("domains.link", C::Domains),
    write("domains.unlink", C::Domains),
    write("domains.clear_token", C::Domains),
    secret("domains.set_token", C::Domains),
];

/// Looks up a command by wire name.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.name == name)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::Principal;

    #[test]
    fn names_are_unique_and_well_formed() {
        let mut seen = HashSet::new();
        for c in COMMANDS {
            assert!(seen.insert(c.name), "duplicate {}", c.name);
            let parts: Vec<_> = c.name.split('.').collect();
            assert!(parts.len() >= 2, "{}", c.name);
            assert!(
                parts
                    .iter()
                    .all(|p| !p.is_empty()
                        && p.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_')),
                "{}",
                c.name
            );
        }
    }

    /// REM-04 / PLY-03: for every command and every capability, a device holding everything
    /// *except* the required capability is denied, and a device holding only it is allowed.
    #[test]
    fn every_command_requires_its_capability() {
        let id = msm_core::ids::DeviceId::new();
        for c in COMMANDS {
            let without: Vec<_> = Capability::ALL
                .iter()
                .copied()
                .filter(|x| *x != c.capability)
                .collect();
            let denied = Principal::Device {
                id,
                scopes: without,
            };
            assert!(
                !denied.has(c.capability),
                "{} allowed without {:?}",
                c.name,
                c.capability
            );
            let only = Principal::Device {
                id,
                scopes: vec![c.capability],
            };
            assert!(only.has(c.capability));
        }
    }

    #[test]
    fn moderation_is_separate_from_console() {
        for name in ["players.op", "players.ban", "players.kick"] {
            assert_eq!(lookup(name).unwrap().capability, Capability::Moderate);
        }
        assert_eq!(
            lookup("console.send").unwrap().capability,
            Capability::ConsoleWrite
        );
    }

    #[test]
    fn secrets_are_flagged() {
        assert!(lookup("domains.set_token").unwrap().sensitive);
        assert!(lookup("nope.nope").is_none());
    }
}
