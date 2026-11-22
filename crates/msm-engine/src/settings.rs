//! Typed editor for common `server.properties` settings (CFG-01, CFG-03).
//!
//! Only a curated, validated set of keys is editable here; everything else (including secrets
//! such as `rcon.password` and the management-server keystore password) is preserved untouched
//! by the lossless properties codec and never sent to the UI. Keys verified against real
//! 1.21.10 and 26.3 servers (2026-11-21); notably `pvp` is no longer a server property.
//!
//! Before every save, the previous file is copied to `config-history/` (BAK-02 for settings).

use std::collections::BTreeMap;
use std::path::Path;

use msm_core::mc::properties::PropertiesDoc;
use msm_core::{ErrorCode, MsmError};
use msm_fsguard::write_atomic;
use serde::Serialize;

/// Value type of a setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    /// `true`/`false`.
    Bool,
    /// Integer range.
    Int {
        /// Minimum.
        min: i64,
        /// Maximum.
        max: i64,
    },
    /// One of a list.
    Choice {
        /// Allowed values.
        options: Vec<&'static str>,
    },
    /// Single-line text.
    Text {
        /// Maximum characters.
        max_len: usize,
    },
}

/// One editable setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingDef {
    /// Property key.
    pub key: &'static str,
    /// UI group.
    pub group: &'static str,
    /// Short label.
    pub label: &'static str,
    /// One-line explanation.
    pub help: &'static str,
    /// Value type.
    #[serde(flatten)]
    pub kind: Kind,
    /// Minecraft's default.
    pub default: &'static str,
    /// Optional warning shown when the value is set to `warn_value`.
    pub warning: Option<(&'static str, &'static str)>,
}

const fn int(min: i64, max: i64) -> Kind {
    Kind::Int { min, max }
}

/// The editable settings, in display order.
#[must_use]
pub fn schema() -> Vec<SettingDef> {
    let choice = |o: &[&'static str]| Kind::Choice {
        options: o.to_vec(),
    };
    let d = |key, group, label, help, kind, default| SettingDef {
        key,
        group,
        label,
        help,
        kind,
        default,
        warning: None,
    };
    vec![
        d(
            "motd",
            "General",
            "Server description",
            "Shown under the server name in the multiplayer list.",
            Kind::Text { max_len: 150 },
            "A Minecraft Server",
        ),
        d(
            "max-players",
            "General",
            "Max players",
            "How many people can be online at once.",
            int(1, 1000),
            "20",
        ),
        d(
            "difficulty",
            "Gameplay",
            "Difficulty",
            "Monster damage and hunger.",
            choice(&["peaceful", "easy", "normal", "hard"]),
            "easy",
        ),
        d(
            "gamemode",
            "Gameplay",
            "Default game mode",
            "Game mode for players joining for the first time.",
            choice(&["survival", "creative", "adventure", "spectator"]),
            "survival",
        ),
        d(
            "force-gamemode",
            "Gameplay",
            "Force game mode",
            "Put everyone in the default game mode each time they join.",
            Kind::Bool,
            "false",
        ),
        d(
            "hardcore",
            "Gameplay",
            "Hardcore",
            "Hard difficulty, and players become spectators when they die. Best decided before the world is created.",
            Kind::Bool,
            "false",
        ),
        d(
            "allow-flight",
            "Gameplay",
            "Allow flight",
            "Don't kick players for flying (needed for some mods and jetpacks).",
            Kind::Bool,
            "false",
        ),
        d(
            "spawn-protection",
            "Gameplay",
            "Spawn protection radius",
            "Blocks around spawn that only operators can change. 0 turns it off.",
            int(0, 1000),
            "16",
        ),
        d(
            "view-distance",
            "Performance",
            "View distance",
            "How far (in chunks) players can see. Lower is lighter on your PC.",
            int(3, 32),
            "10",
        ),
        d(
            "simulation-distance",
            "Performance",
            "Simulation distance",
            "How far (in chunks) the world keeps running around players.",
            int(3, 32),
            "10",
        ),
        d(
            "pause-when-empty-seconds",
            "Performance",
            "Pause when empty after (seconds)",
            "Stop ticking the world when nobody is online. 0 never pauses.",
            int(0, 86_400),
            "60",
        ),
        d(
            "player-idle-timeout",
            "Players",
            "Idle kick (minutes)",
            "Kick players who are idle this long. 0 never kicks.",
            int(0, 1440),
            "0",
        ),
        d(
            "hide-online-players",
            "Players",
            "Hide player list",
            "Don't show who's online in the multiplayer list.",
            Kind::Bool,
            "false",
        ),
        SettingDef {
            warning: Some((
                "false",
                "Offline mode lets anyone join using any name, including yours or an operator's. Only use it on a trusted private network.",
            )),
            ..d(
                "online-mode",
                "Security",
                "Verify accounts (online mode)",
                "Check that players own Minecraft. Keep this on.",
                Kind::Bool,
                "true",
            )
        },
        d(
            "white-list",
            "Security",
            "Whitelist",
            "Only players on the whitelist can join.",
            Kind::Bool,
            "false",
        ),
        d(
            "enforce-whitelist",
            "Security",
            "Kick non-whitelisted players",
            "When the whitelist is turned on or reloaded, kick anyone not on it.",
            Kind::Bool,
            "false",
        ),
        d(
            "level-seed",
            "World",
            "World seed",
            "Only used when a new world is generated.",
            Kind::Text { max_len: 64 },
            "",
        ),
    ]
}

fn validate(def: &SettingDef, value: &str) -> Result<(), String> {
    match &def.kind {
        Kind::Bool => (value == "true" || value == "false")
            .then_some(())
            .ok_or_else(|| "must be true or false".into()),
        Kind::Int { min, max } => match value.parse::<i64>() {
            Ok(n) if (*min..=*max).contains(&n) => Ok(()),
            _ => Err(format!("must be a whole number from {min} to {max}")),
        },
        Kind::Choice { options } => options
            .contains(&value)
            .then_some(())
            .ok_or_else(|| format!("must be one of {}", options.join(", "))),
        Kind::Text { max_len } => {
            if value.chars().any(char::is_control) {
                Err("must be a single line".into())
            } else if value.chars().count() > *max_len {
                Err(format!("must be at most {max_len} characters"))
            } else {
                Ok(())
            }
        }
    }
}

/// Current values of the editable settings (defaults where the file has none).
#[must_use]
pub fn read(server_dir: &Path) -> BTreeMap<&'static str, String> {
    let text = std::fs::read_to_string(server_dir.join("server.properties")).unwrap_or_default();
    let doc = PropertiesDoc::parse(&text);
    schema()
        .into_iter()
        .map(|d| (d.key, doc.get(d.key).unwrap_or(d.default).to_owned()))
        .collect()
}

/// Validates and writes settings, keeping a copy of the previous file in `history_dir`.
/// Returns the keys that actually changed. Unknown keys are rejected; nothing is written if any
/// value is invalid.
///
/// # Errors
/// `InvalidArgument` naming the first invalid key; I/O errors.
pub fn write(
    server_dir: &Path,
    history_dir: &Path,
    values: &BTreeMap<String, String>,
    now_ms: i64,
) -> Result<Vec<String>, MsmError> {
    let defs = schema();
    for (k, v) in values {
        let def = defs.iter().find(|d| d.key == k).ok_or_else(|| {
            MsmError::new(
                ErrorCode::InvalidArgument,
                format!("{k} cannot be edited here"),
            )
        })?;
        validate(def, v).map_err(|why| {
            MsmError::new(ErrorCode::InvalidArgument, format!("{}: {why}", def.label))
        })?;
    }
    let path = server_dir.join("server.properties");
    let original = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = PropertiesDoc::parse(&original);
    let mut changed = Vec::new();
    for (k, v) in values {
        let current = doc.get(k).map(str::to_owned);
        let default = defs.iter().find(|d| d.key == k).map(|d| d.default);
        // A key missing from the file means Minecraft uses its default.
        let effective = current.as_deref().or(default).unwrap_or_default();
        if effective != v {
            doc.set(k, v);
            changed.push(k.clone());
        }
    }
    if changed.is_empty() {
        return Ok(changed);
    }
    let io = |e: std::io::Error| MsmError::new(ErrorCode::Internal, e.to_string());
    std::fs::create_dir_all(history_dir).map_err(io)?;
    write_atomic(
        &history_dir.join(format!("server.properties.{now_ms}")),
        original.as_bytes(),
    )
    .map_err(io)?;
    prune_history(history_dir, 20);
    write_atomic(&path, doc.render().as_bytes()).map_err(io)?;
    Ok(changed)
}

fn prune_history(dir: &Path, keep: usize) {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    files.sort();
    let excess = files.len().saturating_sub(keep);
    for f in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn schema_is_consistent() {
        for d in schema() {
            assert!(
                validate(&d, d.default).is_ok(),
                "default of {} is invalid",
                d.key
            );
            assert_ne!(
                d.key, "pvp",
                "pvp is not a server property in current versions"
            );
        }
        let keys: Vec<_> = schema().iter().map(|d| d.key).collect();
        assert!(
            !keys
                .iter()
                .any(|k| k.contains("password") || k.contains("secret"))
        );
    }

    #[test]
    fn writes_validated_values_preserving_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let server = d.path().join("server");
        std::fs::create_dir(&server).unwrap();
        std::fs::write(
            server.join("server.properties"),
            "#comment\nrcon.password=s3cret\nmotd=Old\nmax-players=20\n",
        )
        .unwrap();
        let changed = write(
            &server,
            &d.path().join("hist"),
            &vals(&[
                ("motd", "Friends: welcome!"),
                ("max-players", "20"),
                ("difficulty", "hard"),
            ]),
            1,
        )
        .unwrap();
        assert_eq!(changed, vec!["difficulty".to_string(), "motd".to_string()]);
        let text = std::fs::read_to_string(server.join("server.properties")).unwrap();
        assert!(text.starts_with("#comment\nrcon.password=s3cret\n"));
        assert_eq!(read(&server)["motd"], "Friends: welcome!");
        assert_eq!(
            std::fs::read_dir(d.path().join("hist")).unwrap().count(),
            1,
            "previous file kept"
        );
    }

    #[test]
    fn rejects_invalid_or_unknown_without_writing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("server.properties"), "motd=Old\n").unwrap();
        for bad in [
            vals(&[("max-players", "0")]),
            vals(&[("difficulty", "nightmare")]),
            vals(&[("motd", "two\nlines")]),
            vals(&[("rcon.password", "x")]),
            vals(&[("online-mode", "maybe")]),
            vals(&[("motd", "fine"), ("view-distance", "99")]),
        ] {
            assert!(
                write(d.path(), &d.path().join("h"), &bad, 1).is_err(),
                "{bad:?}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(d.path().join("server.properties")).unwrap(),
            "motd=Old\n"
        );
    }

    #[test]
    fn history_is_pruned() {
        let d = tempfile::tempdir().unwrap();
        let hist = d.path().join("h");
        for i in 0..25 {
            let mut v = vals(&[("max-players", "20")]);
            v.insert("max-players".into(), (10 + i).to_string());
            write(d.path(), &hist, &v, 1000 + i64::from(i)).unwrap();
        }
        assert_eq!(std::fs::read_dir(&hist).unwrap().count(), 20);
    }
}
