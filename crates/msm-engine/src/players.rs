//! Player management (PLY-01..03).
//!
//! * While the server **runs**, moderation goes through the server's own commands, built with
//!   injection-proof [`CommandTemplate`]s.
//! * While it is **stopped**, we edit `ops.json`, `whitelist.json` and `banned-players.json`
//!   directly (atomic writes, unknown fields preserved), because the server would overwrite any
//!   change made to those files while it runs.
//! * Online players are tracked from console lines (`joined/left the game`) and can be
//!   re-synced with `list uuids`.
//!
//! Formats verified against a real Minecraft 26.3 server on 2026-11-21
//! (`fixtures/minecraft/player-files/`).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use md5::Digest as _;
use msm_core::mc::command::{ArgValue, CommandTemplate, Param, ParamType, PlayerName};
use msm_core::mc::properties::PropertiesDoc;
use msm_core::{ErrorCode, MsmError};
use msm_fsguard::write_atomic;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A moderation action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Moderation {
    /// Make operator (level 4).
    Op,
    /// Remove operator.
    Deop,
    /// Disconnect (running servers only).
    Kick {
        /// Optional reason shown to the player.
        #[serde(default)]
        reason: Option<String>,
    },
    /// Ban by name/UUID.
    Ban {
        /// Optional reason.
        #[serde(default)]
        reason: Option<String>,
    },
    /// Lift a ban.
    Pardon,
    /// Add to whitelist.
    WhitelistAdd,
    /// Remove from whitelist.
    WhitelistRemove,
}

fn template(line: &str, with_reason: bool) -> CommandTemplate {
    let mut params = vec![Param {
        name: "player".into(),
        label: "player".into(),
        ty: ParamType::Player,
        optional: false,
    }];
    if with_reason {
        params.push(Param {
            name: "reason".into(),
            label: "reason".into(),
            ty: ParamType::Text { max_len: 200 },
            optional: true,
        });
    }
    #[allow(clippy::expect_used)]
    CommandTemplate::new(params, &[line]).expect("built-in templates are valid")
}

/// The console command for a moderation action.
///
/// # Errors
/// Invalid reason text (line breaks, too long).
pub fn console_command(action: &Moderation, player: &PlayerName) -> Result<String, MsmError> {
    let (t, reason) = match action {
        Moderation::Op => (template("op {player}", false), None),
        Moderation::Deop => (template("deop {player}", false), None),
        Moderation::Kick { reason } => {
            (template("kick {player} {reason}", true), reason.as_deref())
        }
        Moderation::Ban { reason } => (template("ban {player} {reason}", true), reason.as_deref()),
        Moderation::Pardon => (template("pardon {player}", false), None),
        Moderation::WhitelistAdd => (template("whitelist add {player}", false), None),
        Moderation::WhitelistRemove => (template("whitelist remove {player}", false), None),
    };
    let mut args = BTreeMap::new();
    args.insert(
        "player".to_owned(),
        ArgValue::Str(player.as_str().to_owned()),
    );
    if let Some(r) = reason {
        args.insert("reason".to_owned(), ArgValue::Str(r.to_owned()));
    }
    let lines = t.render(&args)?;
    lines
        .into_iter()
        .next()
        .ok_or_else(|| MsmError::new(ErrorCode::Internal, "empty command"))
}

/// An entry in one of the server's player lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedPlayer {
    /// Player name as recorded.
    pub name: String,
    /// UUID if recorded.
    pub uuid: Option<String>,
    /// Ban reason (bans only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Operator level (ops only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
}

/// Snapshot of the server's player files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerLists {
    /// `ops.json`.
    pub ops: Vec<ListedPlayer>,
    /// `whitelist.json`.
    pub whitelist: Vec<ListedPlayer>,
    /// `banned-players.json`.
    pub banned: Vec<ListedPlayer>,
    /// `banned-ips.json` (IP shown in `name`).
    pub banned_ips: Vec<ListedPlayer>,
    /// `usercache.json`: players the server has seen or looked up.
    pub known: Vec<ListedPlayer>,
    /// `white-list` / `enforce-whitelist` setting from `server.properties`.
    pub whitelist_enabled: bool,
}

fn read_array(path: &Path) -> Vec<Map<String, Value>> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<Map<String, Value>>>(&t).ok())
        .unwrap_or_default()
}

fn str_field(m: &Map<String, Value>, k: &str) -> Option<String> {
    m.get(k).and_then(Value::as_str).map(str::to_owned)
}

fn to_listed(m: &Map<String, Value>, name_key: &str) -> ListedPlayer {
    ListedPlayer {
        name: str_field(m, name_key).unwrap_or_default(),
        uuid: str_field(m, "uuid"),
        reason: str_field(m, "reason"),
        level: m
            .get("level")
            .and_then(Value::as_u64)
            .and_then(|l| u8::try_from(l).ok()),
    }
}

/// Reads all player lists from a server directory. Missing or unreadable files are empty.
#[must_use]
pub fn read_lists(server_dir: &Path) -> PlayerLists {
    let list = |f: &str, key: &str| {
        read_array(&server_dir.join(f))
            .iter()
            .map(|m| to_listed(m, key))
            .collect()
    };
    let props = std::fs::read_to_string(server_dir.join("server.properties"))
        .map(|t| PropertiesDoc::parse(&t))
        .unwrap_or_default();
    PlayerLists {
        ops: list("ops.json", "name"),
        whitelist: list("whitelist.json", "name"),
        banned: list("banned-players.json", "name"),
        banned_ips: list("banned-ips.json", "ip"),
        known: list("usercache.json", "name"),
        whitelist_enabled: props.get("white-list") == Some("true"),
    }
}

/// Whether the server runs in online mode (default true).
#[must_use]
pub fn online_mode(server_dir: &Path) -> bool {
    std::fs::read_to_string(server_dir.join("server.properties"))
        .map(|t| PropertiesDoc::parse(&t).get("online-mode") != Some("false"))
        .unwrap_or(true)
}

/// Looks a name up in `usercache.json` (case-insensitive).
#[must_use]
pub fn usercache_uuid(server_dir: &Path, name: &str) -> Option<String> {
    read_array(&server_dir.join("usercache.json"))
        .iter()
        .find(|m| str_field(m, "name").is_some_and(|n| n.eq_ignore_ascii_case(name)))
        .and_then(|m| str_field(m, "uuid"))
}

/// Formats 32 hex digits as a dashed UUID.
#[must_use]
pub fn dashed_uuid(hex32: &str) -> Option<String> {
    let h = hex32.replace('-', "").to_ascii_lowercase();
    (h.len() == 32 && h.chars().all(|c| c.is_ascii_hexdigit())).then(|| {
        format!(
            "{}-{}-{}-{}-{}",
            &h[0..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..32]
        )
    })
}

/// The UUID an offline-mode server assigns: a version-3 UUID of `OfflinePlayer:<name>`.
#[must_use]
pub fn offline_uuid(name: &str) -> String {
    let mut bytes: [u8; 16] = md5::Md5::digest(format!("OfflinePlayer:{name}").as_bytes()).into();
    bytes[6] = (bytes[6] & 0x0f) | 0x30; // version 3
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // IETF variant
    dashed_uuid(&hex::encode(bytes)).unwrap_or_default()
}

/// Minecraft's file timestamp format, in UTC: `2026-11-21 15:17:56 +0000`.
#[must_use]
pub fn minecraft_timestamp(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs = unix_secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} +0000",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// Edits the player files of a **stopped** server. `uuid` must already be resolved.
///
/// # Errors
/// `InvalidArgument` for actions that need a running server (kick); I/O errors.
pub fn apply_offline(
    server_dir: &Path,
    action: &Moderation,
    player: &PlayerName,
    uuid: &str,
    now_unix_secs: i64,
) -> Result<(), MsmError> {
    let name = player.as_str();
    let (file, add): (&str, Option<Map<String, Value>>) = match action {
        Moderation::Kick { .. } => {
            return Err(MsmError::new(
                ErrorCode::InvalidArgument,
                "kick needs a running server",
            ));
        }
        Moderation::Op => (
            "ops.json",
            Some(entry(&[
                ("uuid", Value::from(uuid)),
                ("name", Value::from(name)),
                ("level", Value::from(4)),
                ("bypassesPlayerLimit", Value::from(false)),
            ])),
        ),
        Moderation::Deop => ("ops.json", None),
        Moderation::WhitelistAdd => (
            "whitelist.json",
            Some(entry(&[
                ("uuid", Value::from(uuid)),
                ("name", Value::from(name)),
            ])),
        ),
        Moderation::WhitelistRemove => ("whitelist.json", None),
        Moderation::Ban { reason } => {
            if let Some(r) = reason
                && (r.chars().any(char::is_control) || r.chars().count() > 200)
            {
                return Err(MsmError::new(
                    ErrorCode::InvalidArgument,
                    "invalid ban reason",
                ));
            }
            (
                "banned-players.json",
                Some(entry(&[
                    ("uuid", Value::from(uuid)),
                    ("name", Value::from(name)),
                    ("created", Value::from(minecraft_timestamp(now_unix_secs))),
                    ("source", Value::from("Server")),
                    ("expires", Value::from("forever")),
                    (
                        "reason",
                        Value::from(
                            reason
                                .clone()
                                .unwrap_or_else(|| "Banned by an operator.".into()),
                        ),
                    ),
                ])),
            )
        }
        Moderation::Pardon => ("banned-players.json", None),
    };
    let path = server_dir.join(file);
    let mut entries = read_array(&path);
    let matches = |m: &Map<String, Value>| {
        str_field(m, "uuid").is_some_and(|u| u.eq_ignore_ascii_case(uuid))
            || str_field(m, "name").is_some_and(|n| n.eq_ignore_ascii_case(name))
    };
    entries.retain(|m| !matches(m));
    if let Some(new) = add {
        entries.push(new);
    }
    let text = serde_json::to_string_pretty(&entries)
        .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
    write_atomic(&path, text.as_bytes())
        .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))
}

fn entry(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// A player event parsed from a console line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerEvent {
    /// Player joined.
    Joined(String),
    /// Player left.
    Left(String),
    /// The server authenticated a player and logged their UUID.
    Uuid {
        /// Name.
        name: String,
        /// Dashed UUID.
        uuid: String,
    },
}

// Messages must follow the log prefix directly (optionally "System chat: "), so a player typing
// "<Steve> Bob joined the game" in chat cannot spoof an event.
static JOIN_LEAVE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"^(?:\[[^\]]*\] \[Server thread/INFO\]|\[\d{2}:\d{2}:\d{2} INFO\]): (?:System chat: )?([A-Za-z0-9_]{1,16}) (joined|left) the game$")
        .expect("valid regex")
});
static UUID_OF: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"^(?:\[[^\]]*\] \[[^\]]*/INFO\]|\[\d{2}:\d{2}:\d{2} INFO\]): UUID of player ([A-Za-z0-9_]{1,16}) is ([0-9a-fA-F-]{32,36})$")
        .expect("valid regex")
});
static LIST_UUIDS: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"^(?:\[[^\]]*\] \[Server thread/INFO\]|\[\d{2}:\d{2}:\d{2} INFO\]): (?:System chat: )?There are (\d+) of a max of (\d+) players online:(.*)$")
        .expect("valid regex")
});
static NAME_UUID: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"([A-Za-z0-9_]{1,16}) \(([0-9a-fA-F-]{36})\)").expect("valid regex")
});

/// Parses a join/leave/UUID console line.
#[must_use]
pub fn parse_event(line: &str) -> Option<PlayerEvent> {
    if let Some(c) = JOIN_LEAVE.captures(line) {
        let name = c[1].to_owned();
        return Some(if &c[2] == "joined" {
            PlayerEvent::Joined(name)
        } else {
            PlayerEvent::Left(name)
        });
    }
    UUID_OF.captures(line).and_then(|c| {
        Some(PlayerEvent::Uuid {
            name: c[1].to_owned(),
            uuid: dashed_uuid(&c[2])?,
        })
    })
}

/// Parsed reply to `list uuids`: `(online, max, [(name, uuid)])`.
pub type ListReply = (u32, u32, Vec<(String, String)>);

/// Parses the reply to `list uuids`.
#[must_use]
pub fn parse_list(line: &str) -> Option<ListReply> {
    let c = LIST_UUIDS.captures(line)?;
    let players = NAME_UUID
        .captures_iter(&c[3])
        .map(|p| (p[1].to_owned(), p[2].to_ascii_lowercase()))
        .collect();
    Some((c[1].parse().ok()?, c[2].parse().ok()?, players))
}

/// A player currently online.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnlinePlayer {
    /// Name.
    pub name: String,
    /// UUID if seen.
    pub uuid: Option<String>,
    /// When they joined (Unix ms), if observed.
    pub joined_at_ms: Option<i64>,
}

/// Online-player tracker fed with console lines.
#[derive(Debug, Default, Clone)]
pub struct OnlineTracker {
    players: BTreeMap<String, OnlinePlayer>,
    pending_uuids: BTreeMap<String, String>,
}

impl OnlineTracker {
    /// Feeds one console line; returns `true` if the online set changed.
    pub fn observe(&mut self, line: &str, now_ms: i64) -> bool {
        if let Some((_, _, list)) = parse_list(line) {
            let before = self.players.clone();
            self.players
                .retain(|n, _| list.iter().any(|(ln, _)| ln == n));
            for (name, uuid) in list {
                let e = self.players.entry(name.clone()).or_insert(OnlinePlayer {
                    name,
                    uuid: None,
                    joined_at_ms: None,
                });
                e.uuid = Some(uuid);
            }
            return before != self.players;
        }
        match parse_event(line) {
            Some(PlayerEvent::Uuid { name, uuid }) => {
                self.pending_uuids.insert(name, uuid);
                false
            }
            Some(PlayerEvent::Joined(name)) => {
                let uuid = self.pending_uuids.remove(&name);
                self.players.insert(
                    name.clone(),
                    OnlinePlayer {
                        name,
                        uuid,
                        joined_at_ms: Some(now_ms),
                    },
                );
                true
            }
            Some(PlayerEvent::Left(name)) => self.players.remove(&name).is_some(),
            None => false,
        }
    }

    /// Clears everything (server stopped).
    pub fn clear(&mut self) {
        self.players.clear();
        self.pending_uuids.clear();
    }

    /// Current online players, sorted by name.
    #[must_use]
    pub fn online(&self) -> Vec<OnlinePlayer> {
        self.players.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/minecraft/player-files")
    }

    fn name(s: &str) -> PlayerName {
        PlayerName::parse(s).unwrap()
    }

    #[test]
    fn reads_real_26_3_files() {
        let lists = read_lists(&fixture_dir());
        assert_eq!(lists.ops[0].name, "Notch");
        assert_eq!(lists.ops[0].level, Some(4));
        assert_eq!(lists.whitelist[0].name, "jeb_");
        assert_eq!(lists.banned[0].reason.as_deref(), Some("Test reason"));
        assert_eq!(lists.known.len(), 3);
        assert_eq!(
            usercache_uuid(&fixture_dir(), "notch").as_deref(),
            Some("069a79f4-44e9-4726-a5be-fca90e38aaf5")
        );
    }

    #[test]
    fn console_commands_are_built_safely() {
        assert_eq!(
            console_command(&Moderation::Op, &name("Steve")).unwrap(),
            "op Steve"
        );
        assert_eq!(
            console_command(
                &Moderation::Ban {
                    reason: Some("griefing spawn".into())
                },
                &name("Steve")
            )
            .unwrap(),
            "ban Steve griefing spawn"
        );
        assert_eq!(
            console_command(&Moderation::Kick { reason: None }, &name("Steve")).unwrap(),
            "kick Steve"
        );
        assert!(
            console_command(
                &Moderation::Kick {
                    reason: Some("x\nop Mallory".into())
                },
                &name("Steve")
            )
            .is_err()
        );
        assert_eq!(
            console_command(&Moderation::WhitelistAdd, &name("jeb_")).unwrap(),
            "whitelist add jeb_"
        );
    }

    #[test]
    fn offline_edits_preserve_other_entries_and_fields() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["ops.json", "whitelist.json", "banned-players.json"] {
            std::fs::copy(fixture_dir().join(f), dir.path().join(f)).unwrap();
        }
        let steve = "8667ba71-b85a-4004-af54-457a9734eed7";
        apply_offline(dir.path(), &Moderation::Op, &name("Steve"), steve, 0).unwrap();
        apply_offline(
            dir.path(),
            &Moderation::Ban { reason: None },
            &name("Steve"),
            steve,
            1_795_000_000,
        )
        .unwrap();
        apply_offline(
            dir.path(),
            &Moderation::Pardon,
            &name("Dinnerbone"),
            "61699b2e-d327-4a01-9f1e-0ea8c3f06bc6",
            0,
        )
        .unwrap();
        apply_offline(
            dir.path(),
            &Moderation::WhitelistRemove,
            &name("JEB_"),
            "x",
            0,
        )
        .unwrap();
        let lists = read_lists(dir.path());
        assert_eq!(
            lists
                .ops
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Notch", "Steve"]
        );
        assert_eq!(lists.banned.len(), 1);
        assert_eq!(lists.banned[0].name, "Steve");
        assert!(lists.whitelist.is_empty(), "name match is case-insensitive");
        let raw = std::fs::read_to_string(dir.path().join("ops.json")).unwrap();
        assert!(raw.contains("bypassesPlayerLimit"));
        assert!(
            apply_offline(
                dir.path(),
                &Moderation::Kick { reason: None },
                &name("Steve"),
                steve,
                0
            )
            .is_err()
        );
    }

    #[test]
    fn offline_uuid_matches_known_value() {
        // Known offline-mode UUID for "Notch".
        assert_eq!(
            offline_uuid("Notch"),
            "b50ad385-829d-3141-a216-7e7d7539ba7f"
        );
    }

    #[test]
    fn timestamps() {
        assert_eq!(minecraft_timestamp(0), "1970-01-01 00:00:00 +0000");
        assert_eq!(
            minecraft_timestamp(1_795_274_276),
            "2026-11-21 15:17:56 +0000"
        );
    }

    #[test]
    fn console_parsing_and_spoof_resistance() {
        let mut t = OnlineTracker::default();
        assert!(!t.observe("[15:00:00] [User Authenticator #1/INFO]: UUID of player Steve is 8667ba71-b85a-4004-af54-457a9734eed7", 1));
        assert!(t.observe("[15:00:01] [Server thread/INFO]: Steve joined the game", 2));
        assert_eq!(
            t.online()[0].uuid.as_deref(),
            Some("8667ba71-b85a-4004-af54-457a9734eed7")
        );
        // Chat cannot spoof join/leave.
        assert!(!t.observe(
            "[15:00:02] [Server thread/INFO]: <Steve> Bob joined the game",
            3
        ));
        assert!(!t.observe(
            "[15:00:02] [Server thread/INFO]: [Not Secure] <Steve> Steve left the game",
            3
        ));
        assert_eq!(t.online().len(), 1);
        assert!(t.observe(
            "[15:00:03] [Server thread/INFO]: System chat: Steve left the game",
            4
        ));
        assert!(t.online().is_empty());

        let line = "[15:00:04] [Server thread/INFO]: System chat: There are 2 of a max of 20 players online: Steve (8667ba71-b85a-4004-af54-457a9734eed7), Alex (ec561538-f3fd-461d-aff5-086b22154bce)";
        let (n, max, players) = parse_list(line).unwrap();
        assert_eq!((n, max, players.len()), (2, 20, 2));
        assert!(t.observe(line, 5));
        assert_eq!(t.online().len(), 2);
        assert_eq!(parse_list("[15:16:56] [Server thread/INFO]: System chat: There are 0 of a max of 20 players online: ").unwrap().2.len(), 0);
        // Paper's log format.
        let mut p = OnlineTracker::default();
        assert!(p.observe("[12:00:01 INFO]: Steve joined the game", 1));
        assert!(!p.observe("[12:00:02 INFO]: <Steve> Bob joined the game", 2));
        assert_eq!(p.online().len(), 1);
        assert!(p.observe("[12:00:03 INFO]: Steve left the game", 3));
    }
}
