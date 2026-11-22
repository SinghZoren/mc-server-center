//! Built-in quick commands and user macros (CMD-01, CMD-02).
//!
//! Every action is a [`CommandTemplate`], so arguments are typed and cannot inject extra
//! commands. Actions that differ between Minecraft versions are gated; the game-rule rename was
//! verified on real servers on 2026-11-21: 1.21.10 accepts `gamerule keepInventory`, 1.21.11 and
//! 26.3 only accept `gamerule keep_inventory`.

use std::collections::BTreeMap;

use msm_core::mc::command::{ArgValue, CommandError, CommandTemplate, Param, ParamType};
use serde::{Deserialize, Serialize};

/// A command action shown on the Quick Commands page or saved as a macro.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionDef {
    /// Stable ID (`time_day`, or a macro ULID).
    pub id: String,
    /// Label (translation key for built-ins, user text for macros).
    pub label: String,
    /// Grouping for the UI.
    pub category: String,
    /// Typed parameters.
    pub params: Vec<Param>,
    /// Command lines (templates).
    pub lines: Vec<String>,
}

impl ActionDef {
    /// Compiles the template, validating it.
    ///
    /// # Errors
    /// [`CommandError::InvalidTemplate`] if unsafe or malformed.
    pub fn template(&self) -> Result<CommandTemplate, CommandError> {
        let lines: Vec<&str> = self.lines.iter().map(String::as_str).collect();
        CommandTemplate::new(self.params.clone(), &lines)
    }

    /// Renders console lines for the given arguments.
    ///
    /// # Errors
    /// Template or argument validation errors.
    pub fn render(&self, args: &BTreeMap<String, ArgValue>) -> Result<Vec<String>, CommandError> {
        self.template()?.render(args)
    }
}

fn param(name: &str, ty: ParamType) -> Param {
    Param {
        name: name.into(),
        label: format!("param.{name}"),
        ty,
        optional: false,
    }
}

fn choice(name: &str, options: &[&str]) -> Param {
    param(
        name,
        ParamType::Choice {
            options: options.iter().map(|s| (*s).to_owned()).collect(),
        },
    )
}

fn action(id: &str, category: &str, params: Vec<Param>, lines: &[&str]) -> ActionDef {
    ActionDef {
        id: id.into(),
        label: format!("quick.{id}"),
        category: category.into(),
        params,
        lines: lines.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// Whether a Minecraft version uses the snake_case game-rule names introduced in 1.21.11.
/// Handles both `1.x.y` and year-based `26.x` IDs; unknown formats default to the new names.
#[must_use]
pub fn uses_snake_case_gamerules(mc_version: &str) -> bool {
    let core = mc_version.split(['-', ' ']).next().unwrap_or(mc_version);
    let parts: Vec<u32> = core.split('.').map_while(|p| p.parse().ok()).collect();
    match parts.as_slice() {
        [1, minor, rest @ ..] => {
            let patch = rest.first().copied().unwrap_or(0);
            *minor > 21 || (*minor == 21 && patch >= 11)
        }
        [major, ..] => *major >= 2,
        [] => true,
    }
}

/// Built-in actions applicable to `mc_version`.
#[must_use]
pub fn builtin_actions(mc_version: &str) -> Vec<ActionDef> {
    let keep_inventory = if uses_snake_case_gamerules(mc_version) {
        "gamerule keep_inventory {enabled}"
    } else {
        "gamerule keepInventory {enabled}"
    };
    vec![
        action("time_day", "world", vec![], &["time set day"]),
        action("time_noon", "world", vec![], &["time set noon"]),
        action("time_night", "world", vec![], &["time set night"]),
        action("weather_clear", "world", vec![], &["weather clear"]),
        action("weather_rain", "world", vec![], &["weather rain"]),
        action("weather_thunder", "world", vec![], &["weather thunder"]),
        action(
            "difficulty",
            "world",
            vec![choice("level", &["peaceful", "easy", "normal", "hard"])],
            &["difficulty {level}"],
        ),
        action(
            "keep_inventory",
            "rules",
            vec![param("enabled", ParamType::Bool)],
            &[keep_inventory],
        ),
        action(
            "gamemode",
            "players",
            vec![
                choice("mode", &["survival", "creative", "adventure", "spectator"]),
                param("player", ParamType::Player),
            ],
            &["gamemode {mode} {player}"],
        ),
        action(
            "broadcast",
            "chat",
            vec![param("message", ParamType::Text { max_len: 256 })],
            &["say {message}"],
        ),
        action("save_world", "server", vec![], &["save-all"]),
    ]
}

/// Validates a user macro before saving: safe template, sensible size, non-empty name.
///
/// # Errors
/// [`CommandError::InvalidTemplate`].
pub fn validate_macro(def: &ActionDef) -> Result<(), CommandError> {
    let name = def.label.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(CommandError::InvalidTemplate(
            "macro name must be 1-64 printable characters".into(),
        ));
    }
    def.template().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, ArgValue)]) -> BTreeMap<String, ArgValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn gamerule_naming_boundary_matches_real_servers() {
        for v in ["1.18.2", "1.20.1", "1.21.1", "1.21.10"] {
            assert!(!uses_snake_case_gamerules(v), "{v}");
        }
        for v in ["1.21.11", "26.1", "26.3", "26.4-snapshot-10", "27.1"] {
            assert!(uses_snake_case_gamerules(v), "{v}");
        }
    }

    #[test]
    fn all_builtins_are_valid_templates_for_both_eras() {
        for v in ["1.20.1", "26.3"] {
            for a in builtin_actions(v) {
                assert!(a.template().is_ok(), "{} on {v}", a.id);
            }
        }
    }

    #[test]
    fn renders_version_specific_and_typed_actions() {
        let find = |v: &str, id: &str| builtin_actions(v).into_iter().find(|a| a.id == id).unwrap();
        let on = args(&[("enabled", ArgValue::Bool(true))]);
        assert_eq!(
            find("26.3", "keep_inventory").render(&on).unwrap(),
            ["gamerule keep_inventory true"]
        );
        assert_eq!(
            find("1.20.1", "keep_inventory").render(&on).unwrap(),
            ["gamerule keepInventory true"]
        );
        let gm = args(&[
            ("mode", ArgValue::Str("creative".into())),
            ("player", ArgValue::Str("Steve".into())),
        ]);
        assert_eq!(
            find("26.3", "gamemode").render(&gm).unwrap(),
            ["gamemode creative Steve"]
        );
        let bad = args(&[
            ("mode", ArgValue::Str("god".into())),
            ("player", ArgValue::Str("Steve".into())),
        ]);
        assert!(find("26.3", "gamemode").render(&bad).is_err());
    }

    #[test]
    fn macros_are_validated() {
        let ok = ActionDef {
            id: "m1".into(),
            label: "Start event".into(),
            category: "macro".into(),
            params: vec![param("player", ParamType::Player)],
            lines: vec![
                "time set day".into(),
                "weather clear".into(),
                "tp {player} 0 100 0".into(),
            ],
        };
        assert!(validate_macro(&ok).is_ok());
        assert_eq!(
            ok.render(&args(&[("player", ArgValue::Str("Alex".into()))]))
                .unwrap(),
            ["time set day", "weather clear", "tp Alex 0 100 0"]
        );
        let unsafe_macro = ActionDef {
            lines: vec!["{player} test".into()],
            ..ok.clone()
        };
        assert!(validate_macro(&unsafe_macro).is_err());
        let nameless = ActionDef {
            label: " ".into(),
            ..ok
        };
        assert!(validate_macro(&nameless).is_err());
    }
}
