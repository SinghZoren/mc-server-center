//! Injection-proof console command templates (PLY-03, CMD-01, CMD-02; threat T22).
//!
//! Everything the UI sends to a server console through buttons, quick commands or macros is
//! built from a [`CommandTemplate`] plus typed [`ArgValue`]s, never by string concatenation.
//! The rules that make injection impossible:
//!
//! * the command name (first token) of every template line is a literal, never a parameter;
//! * no value may contain a line break or any other control character, so one template line
//!   always renders to exactly one console line;
//! * word-like values (player names, numbers, choices) cannot contain whitespace, so they cannot
//!   shift later arguments;
//! * free text may contain spaces only in the **last** slot of a line.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Maximum length of one rendered console line.
pub const MAX_LINE_LEN: usize = 1024;
/// Maximum number of lines in a macro.
pub const MAX_MACRO_LINES: usize = 32;
/// Maximum length of a free-text argument (Minecraft chat messages are capped at 256).
pub const MAX_TEXT_LEN: usize = 256;

/// A validated Minecraft Java player name: 1–16 characters of `A-Z a-z 0-9 _`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PlayerName(String);

impl PlayerName {
    /// Validates a player name.
    ///
    /// # Errors
    /// [`CommandError::InvalidPlayerName`] if it is empty, too long or has other characters.
    pub fn parse(s: &str) -> Result<Self, CommandError> {
        let ok = (1..=16).contains(&s.len())
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(CommandError::InvalidPlayerName(s.to_owned()))
        }
    }

    /// The name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PlayerName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        PlayerName::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for PlayerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The type of a template parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParamType {
    /// A player name.
    Player,
    /// An integer within an inclusive range.
    Int {
        /// Minimum allowed value.
        min: i64,
        /// Maximum allowed value.
        max: i64,
    },
    /// One of a fixed set of words (e.g. `survival`, `creative`).
    Choice {
        /// Allowed values; each must be a single word.
        options: Vec<String>,
    },
    /// `true` or `false`.
    Bool,
    /// A single word: letters, digits and `_ - . : / + #` (e.g. a game rule or item ID).
    Word,
    /// Free text with spaces. Only allowed in the last slot of a line.
    Text {
        /// Maximum length in characters.
        max_len: usize,
    },
}

/// A parameter declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    /// Identifier used in `{name}` placeholders: `[a-z][a-z0-9_]*`.
    pub name: String,
    /// Translation key or plain label for the UI.
    pub label: String,
    /// Value type.
    #[serde(flatten)]
    pub ty: ParamType,
    /// Optional slots may be omitted; their placeholder and the space before it are dropped.
    #[serde(default)]
    pub optional: bool,
}

/// A value supplied for a parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgValue {
    /// Integer value.
    Int(i64),
    /// Boolean value.
    Bool(bool),
    /// Any string-typed value (player, choice, word, text); validated against the param type.
    Str(String),
}

/// Why a template or its arguments were rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// Bad player name.
    #[error("invalid player name {0:?}")]
    InvalidPlayerName(String),
    /// The template itself is malformed.
    #[error("invalid template: {0}")]
    InvalidTemplate(String),
    /// A required argument is missing.
    #[error("missing value for {0:?}")]
    MissingArgument(String),
    /// An argument was supplied that the template does not declare.
    #[error("unexpected argument {0:?}")]
    UnexpectedArgument(String),
    /// An argument has the wrong type or is out of range.
    #[error("invalid value for {param:?}: {reason}")]
    InvalidArgument {
        /// Parameter name.
        param: String,
        /// Why it was rejected.
        reason: String,
    },
    /// The rendered line is too long.
    #[error("command is too long")]
    TooLong,
}

impl From<CommandError> for crate::MsmError {
    fn from(e: CommandError) -> Self {
        crate::MsmError::new(crate::ErrorCode::InvalidArgument, e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(String),
    Slot(String),
}

/// A validated, multi-line command template, e.g. `kick {player} {reason}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTemplate {
    params: Vec<Param>,
    lines: Vec<Vec<Token>>,
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/' | '+' | '#' | '~' | '@')
}

impl CommandTemplate {
    /// Builds a template from parameter declarations and template lines. A leading `/` on a
    /// line is removed (console commands do not need it).
    ///
    /// # Errors
    /// [`CommandError::InvalidTemplate`] if any safety rule is violated.
    pub fn new(params: Vec<Param>, lines: &[&str]) -> Result<Self, CommandError> {
        let bad = |m: String| Err(CommandError::InvalidTemplate(m));
        if lines.is_empty() || lines.len() > MAX_MACRO_LINES {
            return bad(format!("needs 1..={MAX_MACRO_LINES} lines"));
        }
        for (i, p) in params.iter().enumerate() {
            let valid_name = p
                .name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase())
                && p.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !valid_name {
                return bad(format!("bad parameter name {:?}", p.name));
            }
            if params[..i].iter().any(|q| q.name == p.name) {
                return bad(format!("duplicate parameter {:?}", p.name));
            }
            match &p.ty {
                ParamType::Int { min, max } if min > max => {
                    return bad(format!("{}: min > max", p.name));
                }
                ParamType::Choice { options } => {
                    if options.is_empty()
                        || options
                            .iter()
                            .any(|o| o.is_empty() || !o.chars().all(is_word_char))
                    {
                        return bad(format!("{}: choices must be non-empty words", p.name));
                    }
                }
                ParamType::Text { max_len } if *max_len == 0 || *max_len > MAX_TEXT_LEN => {
                    return bad(format!(
                        "{}: text max_len must be 1..={MAX_TEXT_LEN}",
                        p.name
                    ));
                }
                _ => {}
            }
        }

        let mut parsed_lines = Vec::new();
        for line in lines {
            let line = line.trim();
            let line = line.strip_prefix('/').unwrap_or(line);
            if line.chars().any(char::is_control) {
                return bad("template contains a control character".into());
            }
            let tokens = tokenize(line)?;
            match tokens.first() {
                Some(Token::Literal(head)) if head.chars().all(is_word_char) => {}
                _ => {
                    return bad(format!(
                        "line {line:?} must start with a literal command name"
                    ));
                }
            }
            for (idx, t) in tokens.iter().enumerate() {
                if let Token::Slot(name) = t {
                    let Some(param) = params.iter().find(|p| &p.name == name) else {
                        return bad(format!("unknown placeholder {{{name}}}"));
                    };
                    if matches!(param.ty, ParamType::Text { .. }) && idx != tokens.len() - 1 {
                        return bad(format!("text parameter {{{name}}} must be the last token"));
                    }
                    if param.optional && idx != tokens.len() - 1 {
                        return bad(format!(
                            "optional parameter {{{name}}} must be the last token"
                        ));
                    }
                }
            }
            parsed_lines.push(tokens);
        }
        Ok(Self {
            params,
            lines: parsed_lines,
        })
    }

    /// Declared parameters, for building UI forms.
    #[must_use]
    pub fn params(&self) -> &[Param] {
        &self.params
    }

    /// Renders one console line per template line.
    ///
    /// # Errors
    /// [`CommandError`] if an argument is missing, unexpected, of the wrong type or invalid.
    pub fn render(&self, args: &BTreeMap<String, ArgValue>) -> Result<Vec<String>, CommandError> {
        for name in args.keys() {
            if !self.params.iter().any(|p| &p.name == name) {
                return Err(CommandError::UnexpectedArgument(name.clone()));
            }
        }
        let mut values: BTreeMap<&str, Option<String>> = BTreeMap::new();
        for p in &self.params {
            let rendered = match args.get(&p.name) {
                None if p.optional => None,
                None => return Err(CommandError::MissingArgument(p.name.clone())),
                // Empty optional text is treated as omitted.
                Some(ArgValue::Str(s)) if p.optional && s.trim().is_empty() => None,
                Some(v) => Some(validate(p, v)?),
            };
            values.insert(p.name.as_str(), rendered);
        }

        let mut out = Vec::with_capacity(self.lines.len());
        for tokens in &self.lines {
            let mut line = String::new();
            for t in tokens {
                let piece = match t {
                    Token::Literal(l) => Some(l.as_str()),
                    Token::Slot(name) => values.get(name.as_str()).and_then(|v| v.as_deref()),
                };
                // Separating spaces are part of the literal tokens, so pieces simply concatenate.
                if let Some(piece) = piece {
                    line.push_str(piece);
                }
            }
            let line = line.trim_end().to_owned();
            if line.len() > MAX_LINE_LEN {
                return Err(CommandError::TooLong);
            }
            debug_assert!(!line.chars().any(char::is_control));
            out.push(line);
        }
        Ok(out)
    }
}

/// Splits a template line into literals and `{slot}` placeholders. Literal text keeps its
/// spaces; `{{`/`}}` are not supported (no Minecraft command we template needs braces in a
/// literal; JSON-text commands are intentionally out of scope).
fn tokenize(line: &str) -> Result<Vec<Token>, CommandError> {
    let mut tokens = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        match rest.find('{') {
            Some(0) => {
                let end = rest.find('}').ok_or_else(|| {
                    CommandError::InvalidTemplate(format!("unclosed placeholder in {line:?}"))
                })?;
                let name = &rest[1..end];
                tokens.push(Token::Slot(name.to_owned()));
                rest = &rest[end + 1..];
                // A slot must be followed by a space or end of line, so values cannot glue onto
                // literals in ways that change their meaning.
                if !(rest.is_empty() || rest.starts_with(' ')) {
                    return Err(CommandError::InvalidTemplate(format!(
                        "placeholder {{{name}}} must be followed by a space or end of line"
                    )));
                }
            }
            Some(pos) => {
                let literal = &rest[..pos];
                if !literal.ends_with(' ') {
                    return Err(CommandError::InvalidTemplate(format!(
                        "placeholder must be preceded by a space in {line:?}"
                    )));
                }
                tokens.push(Token::Literal(literal.to_owned()));
                rest = &rest[pos..];
            }
            None => {
                if rest.contains('}') {
                    return Err(CommandError::InvalidTemplate(format!(
                        "stray '}}' in {line:?}"
                    )));
                }
                tokens.push(Token::Literal(rest.to_owned()));
                rest = "";
            }
        }
    }
    // The command head must be the first word of the first literal.
    if let Some(Token::Literal(first)) = tokens.first()
        && let Some((head, tail)) = first.split_once(' ')
    {
        let (head, tail) = (head.to_owned(), format!(" {tail}"));
        tokens.splice(0..1, [Token::Literal(head), Token::Literal(tail)]);
    }
    Ok(tokens)
}

fn validate(p: &Param, v: &ArgValue) -> Result<String, CommandError> {
    let invalid = |reason: &str| CommandError::InvalidArgument {
        param: p.name.clone(),
        reason: reason.to_owned(),
    };
    match (&p.ty, v) {
        (ParamType::Player, ArgValue::Str(s)) => Ok(PlayerName::parse(s)?.0),
        (ParamType::Int { min, max }, ArgValue::Int(n)) => {
            if (min..=max).contains(&n) {
                Ok(n.to_string())
            } else {
                Err(invalid(&format!("must be between {min} and {max}")))
            }
        }
        (ParamType::Bool, ArgValue::Bool(b)) => Ok(b.to_string()),
        (ParamType::Choice { options }, ArgValue::Str(s)) => {
            if options.iter().any(|o| o == s) {
                Ok(s.clone())
            } else {
                Err(invalid("not one of the allowed choices"))
            }
        }
        (ParamType::Word, ArgValue::Str(s)) => {
            if !s.is_empty() && s.len() <= 128 && s.chars().all(is_word_char) {
                Ok(s.clone())
            } else {
                Err(invalid("must be a single word"))
            }
        }
        (ParamType::Text { max_len }, ArgValue::Str(s)) => {
            if s.chars()
                .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
            {
                Err(invalid(
                    "must not contain line breaks or control characters",
                ))
            } else if s.chars().count() > *max_len {
                Err(invalid(&format!("must be at most {max_len} characters")))
            } else if s.trim().is_empty() {
                Err(invalid("must not be empty"))
            } else {
                Ok(s.trim().to_owned())
            }
        }
        _ => Err(invalid("wrong type")),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn p(name: &str, ty: ParamType) -> Param {
        Param {
            name: name.into(),
            label: name.into(),
            ty,
            optional: false,
        }
    }

    fn args(pairs: &[(&str, ArgValue)]) -> BTreeMap<String, ArgValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    fn kick() -> CommandTemplate {
        let mut reason = p("reason", ParamType::Text { max_len: 100 });
        reason.optional = true;
        CommandTemplate::new(
            vec![p("player", ParamType::Player), reason],
            &["kick {player} {reason}"],
        )
        .unwrap()
    }

    #[test]
    fn player_names() {
        for ok in ["Steve", "a", "x_X_x", "ABCDEFGHIJKLMNOP"] {
            assert!(PlayerName::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "ABCDEFGHIJKLMNOPQ",
            "has space",
            "semi;colon",
            "new\nline",
            "é",
            "a-b",
        ] {
            assert!(PlayerName::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn renders_kick_with_and_without_reason() {
        let t = kick();
        let line = t
            .render(&args(&[
                ("player", ArgValue::Str("Steve".into())),
                ("reason", ArgValue::Str("Please read the rules".into())),
            ]))
            .unwrap();
        assert_eq!(line, vec!["kick Steve Please read the rules"]);
        let line = t
            .render(&args(&[("player", ArgValue::Str("Steve".into()))]))
            .unwrap();
        assert_eq!(line, vec!["kick Steve"]);
    }

    #[test]
    fn injection_attempts_are_rejected() {
        let t = kick();
        let attempts = [
            ("player", "Steve\nop Mallory"),
            ("player", "Steve op"),
            ("player", "@a"),
            ("reason", "bye\nop Mallory"),
            ("reason", "bye\rop Mallory"),
            ("reason", "bye\u{2028}op Mallory"),
        ];
        for (param, value) in attempts {
            let mut a = args(&[("player", ArgValue::Str("Steve".into()))]);
            a.insert(param.into(), ArgValue::Str(value.into()));
            assert!(t.render(&a).is_err(), "{param}={value:?}");
        }
    }

    #[test]
    fn typed_values_are_checked() {
        let t = CommandTemplate::new(
            vec![
                p("ticks", ParamType::Int { min: 0, max: 24000 }),
                p(
                    "mode",
                    ParamType::Choice {
                        options: vec!["survival".into(), "creative".into()],
                    },
                ),
                p("on", ParamType::Bool),
            ],
            &[
                "time set {ticks}",
                "defaultgamemode {mode}",
                "gamerule keepInventory {on}",
            ],
        )
        .unwrap();
        let ok = t
            .render(&args(&[
                ("ticks", ArgValue::Int(1000)),
                ("mode", ArgValue::Str("creative".into())),
                ("on", ArgValue::Bool(true)),
            ]))
            .unwrap();
        assert_eq!(
            ok,
            vec![
                "time set 1000",
                "defaultgamemode creative",
                "gamerule keepInventory true"
            ]
        );
        let out_of_range = args(&[
            ("ticks", ArgValue::Int(99999)),
            ("mode", ArgValue::Str("creative".into())),
            ("on", ArgValue::Bool(true)),
        ]);
        assert!(t.render(&out_of_range).is_err());
        let bad_choice = args(&[
            ("ticks", ArgValue::Int(1)),
            ("mode", ArgValue::Str("spectator".into())),
            ("on", ArgValue::Bool(true)),
        ]);
        assert!(t.render(&bad_choice).is_err());
        let wrong_type = args(&[
            ("ticks", ArgValue::Str("1".into())),
            ("mode", ArgValue::Str("creative".into())),
            ("on", ArgValue::Bool(true)),
        ]);
        assert!(t.render(&wrong_type).is_err());
    }

    #[test]
    fn missing_and_unexpected_arguments() {
        let t = kick();
        assert_eq!(
            t.render(&args(&[])),
            Err(CommandError::MissingArgument("player".into()))
        );
        let extra = args(&[
            ("player", ArgValue::Str("a".into())),
            ("zzz", ArgValue::Bool(true)),
        ]);
        assert_eq!(
            t.render(&extra),
            Err(CommandError::UnexpectedArgument("zzz".into()))
        );
    }

    #[test]
    fn unsafe_templates_are_rejected() {
        let player = || vec![p("player", ParamType::Player)];
        let text = || vec![p("msg", ParamType::Text { max_len: 50 })];
        let cases: Vec<(Vec<Param>, &str)> = vec![
            (player(), "{player} is the command"),
            (text(), "say {msg} and more"),
            (player(), "op {player}suffix"),
            (player(), "op prefix{player}"),
            (player(), "op {unknown}"),
            (player(), "op {player"),
            (player(), "op player}"),
            (player(), "op\n{player}"),
            (player(), ""),
        ];
        for (params, line) in cases {
            assert!(CommandTemplate::new(params, &[line]).is_err(), "{line:?}");
        }
        assert!(CommandTemplate::new(vec![p("Bad", ParamType::Bool)], &["x {Bad}"]).is_err());
        assert!(CommandTemplate::new(vec![], &["/say hi"]).is_ok());
    }

    #[test]
    fn leading_slash_is_stripped() {
        let t = CommandTemplate::new(vec![], &["/weather clear"]).unwrap();
        assert_eq!(t.render(&BTreeMap::new()).unwrap(), vec!["weather clear"]);
    }

    proptest! {
        /// Whatever the user types, each template line yields exactly one line, with no control
        /// characters, starting with the template's literal command name.
        #[test]
        fn rendering_cannot_inject(player in "\\PC{0,20}", reason in "\\PC{0,120}") {
            let t = kick();
            let a = args(&[("player", ArgValue::Str(player)), ("reason", ArgValue::Str(reason))]);
            if let Ok(lines) = t.render(&a) {
                prop_assert_eq!(lines.len(), 1);
                let line = &lines[0];
                prop_assert!(!line.chars().any(char::is_control));
                prop_assert!(line.starts_with("kick "));
                // The player slot is exactly one validated word.
                let player_tok = line.split(' ').nth(1).unwrap_or("");
                prop_assert!(PlayerName::parse(player_tok).is_ok());
            }
        }
    }
}
