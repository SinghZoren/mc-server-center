//! Validated relative paths.

use std::fmt;
use std::path::{Path, PathBuf};

use msm_core::{ErrorCode, MsmError};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum length of a single path component, in bytes (common file-system limit).
pub const MAX_COMPONENT_BYTES: usize = 255;
/// Maximum total length of a normalized relative path, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;
/// Maximum number of components.
pub const MAX_DEPTH: usize = 64;

/// Windows device names that refer to devices in any directory and with any extension.
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM0", "COM1", "COM2", "COM3", "COM4",
    "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3",
    "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

/// Characters that are invalid in Windows file names. Rejected everywhere so instances and
/// backups stay portable between Windows and Linux hosts. `:` also blocks drive letters and
/// NTFS alternate data streams (`file.txt:hidden`).
const FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

/// Why a path was rejected. Each variant names the rule that fired, for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// The path is empty (or only `.` / separators).
    #[error("path is empty")]
    Empty,
    /// The path starts with a separator (absolute, rooted or UNC path).
    #[error("path must be relative")]
    Absolute,
    /// A `..` component.
    #[error("path must not contain '..'")]
    ParentTraversal,
    /// A NUL or other control character.
    #[error("path contains a control character")]
    ControlCharacter,
    /// A character that is invalid on Windows, including `:` (drive letters, data streams).
    #[error("path contains forbidden character {0:?}")]
    ForbiddenCharacter(char),
    /// A Windows reserved device name such as `CON` or `nul.txt`.
    #[error("path component {0:?} is a reserved device name")]
    ReservedName(String),
    /// A component ending in `.` or space, which Windows silently strips (aliasing risk).
    #[error("path component {0:?} ends with a dot or space")]
    TrailingDotOrSpace(String),
    /// A component that looks like an 8.3 short name (`PROGRA~1`), which can alias another file.
    #[error("path component {0:?} looks like a Windows short name")]
    ShortNameAlias(String),
    /// A component or the whole path is too long, or nested too deeply.
    #[error("path is too long or too deep")]
    TooLong,
}

impl From<PathError> for MsmError {
    fn from(e: PathError) -> Self {
        MsmError::new(ErrorCode::PathRejected, e.to_string())
    }
}

/// A validated, normalized path relative to some root directory.
///
/// Invariants (checked at construction, so holders can rely on them):
/// * non-empty, separated by `/`, no leading separator;
/// * no `.`, `..` or empty components;
/// * no control or Windows-forbidden characters, reserved names, trailing dots/spaces or
///   short-name aliases;
/// * bounded length and depth.
///
/// Both `/` and `\` are accepted as separators on input; the stored form always uses `/`.
/// Joining a `RelPath` onto a root can therefore never leave that root lexically. Symlinks are
/// handled separately by [`crate::Root`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(String);

impl RelPath {
    /// Validates and normalizes `input`.
    ///
    /// # Errors
    /// Returns the [`PathError`] for the first rule that fails.
    pub fn parse(input: &str) -> Result<Self, PathError> {
        if input.starts_with('/') || input.starts_with('\\') {
            // Covers `/etc`, `\Windows`, `\\server\share`, `\\?\C:\`, `\\.\pipe`.
            return Err(PathError::Absolute);
        }

        let mut components: Vec<&str> = Vec::new();
        for component in input.split(['/', '\\']) {
            match component {
                "" | "." => continue,
                ".." => return Err(PathError::ParentTraversal),
                c => {
                    validate_component(c)?;
                    components.push(c);
                }
            }
        }

        if components.is_empty() {
            return Err(PathError::Empty);
        }
        if components.len() > MAX_DEPTH {
            return Err(PathError::TooLong);
        }
        let normalized = components.join("/");
        if normalized.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        Ok(Self(normalized))
    }

    /// The normalized form, using `/` separators.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Iterates over the components.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The final component (file or directory name).
    #[must_use]
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The parent path, or `None` for a single-component path.
    #[must_use]
    pub fn parent(&self) -> Option<RelPath> {
        self.0
            .rsplit_once('/')
            .map(|(parent, _)| RelPath(parent.to_owned()))
    }

    /// Appends another relative path.
    ///
    /// # Errors
    /// Returns [`PathError::TooLong`] if the result exceeds the length or depth limits.
    pub fn join(&self, other: &RelPath) -> Result<RelPath, PathError> {
        RelPath::parse(&format!("{}/{}", self.0, other.0))
    }

    /// Converts to a native relative [`PathBuf`] (with the platform separator).
    #[must_use]
    pub fn to_native(&self) -> PathBuf {
        self.components().collect()
    }

    /// Joins onto `root` lexically. Prefer [`crate::Root::resolve`] for real file access,
    /// which also refuses symlinks.
    #[must_use]
    pub fn join_onto(&self, root: &Path) -> PathBuf {
        root.join(self.to_native())
    }
}

fn validate_component(c: &str) -> Result<(), PathError> {
    if c.len() > MAX_COMPONENT_BYTES {
        return Err(PathError::TooLong);
    }
    if c.chars().any(char::is_control) {
        return Err(PathError::ControlCharacter);
    }
    if let Some(ch) = c.chars().find(|ch| FORBIDDEN_CHARS.contains(ch)) {
        return Err(PathError::ForbiddenCharacter(ch));
    }
    if c.ends_with('.') || c.ends_with(' ') {
        return Err(PathError::TrailingDotOrSpace(c.to_owned()));
    }
    // Windows treats `NUL`, `nul.txt`, `NUL .txt` and `nul.tar.gz` all as the device.
    let stem = c.split('.').next().unwrap_or(c).trim_end_matches(' ');
    if RESERVED_NAMES.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return Err(PathError::ReservedName(c.to_owned()));
    }
    if looks_like_short_name(c) {
        return Err(PathError::ShortNameAlias(c.to_owned()));
    }
    Ok(())
}

/// Detects 8.3-style names such as `PROGRA~1` or `MODS~12.JAR`: a `~` followed by a digit.
/// Such names can alias a differently named file on Windows and bypass allowlists.
fn looks_like_short_name(c: &str) -> bool {
    let bytes = c.as_bytes();
    bytes
        .windows(2)
        .any(|w| w[0] == b'~' && w[1].is_ascii_digit())
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for RelPath {
    type Err = PathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for RelPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Deserialization validates, so a `RelPath` received over the API is always safe.
impl<'de> Deserialize<'de> for RelPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        RelPath::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn accepts_and_normalizes_ordinary_paths() {
        let cases = [
            ("server.properties", "server.properties"),
            ("world/region/r.0.0.mca", "world/region/r.0.0.mca"),
            ("mods\\fabric-api.jar", "mods/fabric-api.jar"),
            ("./config//sub/./file.toml", "config/sub/file.toml"),
            ("config/", "config"),
            (".hidden/file", ".hidden/file"),
            ("logs/latest.log", "logs/latest.log"),
            ("name with spaces/file.txt", "name with spaces/file.txt"),
            ("ünïcödé/世界.dat", "ünïcödé/世界.dat"),
            ("con_tents/console.txt", "con_tents/console.txt"),
            ("nullable.txt", "nullable.txt"),
            ("a~b/c", "a~b/c"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                RelPath::parse(input).unwrap().as_str(),
                expected,
                "input {input:?}"
            );
        }
    }

    #[test]
    fn rejects_dangerous_paths() {
        use PathError as E;
        type Check = fn(&PathError) -> bool;
        let cases: &[(&str, Check)] = &[
            ("", |e| *e == E::Empty),
            (".", |e| *e == E::Empty),
            ("./", |e| *e == E::Empty),
            ("../etc/passwd", |e| *e == E::ParentTraversal),
            ("world/../../escape", |e| *e == E::ParentTraversal),
            ("world\\..\\..\\escape", |e| *e == E::ParentTraversal),
            ("/etc/passwd", |e| *e == E::Absolute),
            ("\\Windows\\System32", |e| *e == E::Absolute),
            ("\\\\server\\share\\x", |e| *e == E::Absolute),
            ("\\\\?\\C:\\Windows", |e| *e == E::Absolute),
            ("//?/C:/Windows", |e| *e == E::Absolute),
            ("C:\\Windows", |e| matches!(e, E::ForbiddenCharacter(':'))),
            ("C:relative", |e| matches!(e, E::ForbiddenCharacter(':'))),
            ("file.txt:secret", |e| {
                matches!(e, E::ForbiddenCharacter(':'))
            }),
            ("file.txt::$DATA", |e| {
                matches!(e, E::ForbiddenCharacter(':'))
            }),
            ("a\0b", |e| *e == E::ControlCharacter),
            ("line\nbreak", |e| *e == E::ControlCharacter),
            ("CON", |e| matches!(e, E::ReservedName(_))),
            ("con", |e| matches!(e, E::ReservedName(_))),
            ("nul.txt", |e| matches!(e, E::ReservedName(_))),
            ("dir/NUL .txt", |e| matches!(e, E::ReservedName(_))),
            ("LPT1.tar.gz", |e| matches!(e, E::ReservedName(_))),
            ("COM¹", |e| matches!(e, E::ReservedName(_))),
            ("conin$", |e| matches!(e, E::ReservedName(_))),
            ("trailing.", |e| matches!(e, E::TrailingDotOrSpace(_))),
            ("trailing ", |e| matches!(e, E::TrailingDotOrSpace(_))),
            ("dir./file", |e| matches!(e, E::TrailingDotOrSpace(_))),
            ("PROGRA~1/x", |e| matches!(e, E::ShortNameAlias(_))),
            ("mods/FABRIC~1.JAR", |e| matches!(e, E::ShortNameAlias(_))),
            ("what?.txt", |e| matches!(e, E::ForbiddenCharacter('?'))),
            ("a|b", |e| matches!(e, E::ForbiddenCharacter('|'))),
            ("a*b", |e| matches!(e, E::ForbiddenCharacter('*'))),
            ("a<b>", |e| matches!(e, E::ForbiddenCharacter('<'))),
            ("quote\"d", |e| matches!(e, E::ForbiddenCharacter('"'))),
        ];
        for (input, check) in cases {
            let err = RelPath::parse(input).expect_err(input);
            assert!(check(&err), "input {input:?} gave unexpected error {err:?}");
        }
    }

    #[test]
    fn rejects_overlong_paths() {
        let long_component = "a".repeat(MAX_COMPONENT_BYTES + 1);
        assert_eq!(RelPath::parse(&long_component), Err(PathError::TooLong));
        let deep = vec!["d"; MAX_DEPTH + 1].join("/");
        assert_eq!(RelPath::parse(&deep), Err(PathError::TooLong));
        let wide = vec!["a".repeat(200); 6].join("/");
        assert_eq!(RelPath::parse(&wide), Err(PathError::TooLong));
    }

    #[test]
    fn helpers_work() {
        let p = RelPath::parse("world/region/r.0.0.mca").unwrap();
        assert_eq!(p.file_name(), "r.0.0.mca");
        assert_eq!(p.parent().unwrap().as_str(), "world/region");
        assert_eq!(RelPath::parse("a").unwrap().parent(), None);
        let joined = p
            .parent()
            .unwrap()
            .join(&RelPath::parse("r.1.0.mca").unwrap())
            .unwrap();
        assert_eq!(joined.as_str(), "world/region/r.1.0.mca");
        assert_eq!(p.components().count(), 3);
    }

    #[test]
    fn serde_validates_on_deserialize() {
        let ok: RelPath = serde_json::from_str("\"mods/a.jar\"").unwrap();
        assert_eq!(ok.as_str(), "mods/a.jar");
        assert!(serde_json::from_str::<RelPath>("\"../../etc/passwd\"").is_err());
        assert_eq!(serde_json::to_string(&ok).unwrap(), "\"mods/a.jar\"");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]

        /// Whatever the input, an accepted path is canonical and lexically confined.
        #[test]
        fn accepted_paths_are_confined(input in r"[a-zA-Z0-9 ._~:/\\\-<>|?*\x00\n]{0,40}") {
            if let Ok(rel) = RelPath::parse(&input) {
                let root = std::path::Path::new("root");
                let joined = rel.join_onto(root);
                prop_assert!(joined.starts_with(root));
                for comp in joined.components().skip(1) {
                    prop_assert!(matches!(comp, std::path::Component::Normal(_)), "{comp:?}");
                }
                prop_assert_eq!(RelPath::parse(rel.as_str()), Ok(rel.clone()));
            }
        }

        /// Any input containing a `..` component is rejected.
        #[test]
        fn parent_components_always_rejected(prefix in "[a-z]{0,8}", suffix in "[a-z]{0,8}", sep in r"[/\\]") {
            let input = if prefix.is_empty() { format!("..{sep}{suffix}") } else { format!("{prefix}{sep}..{sep}{suffix}") };
            prop_assert_eq!(RelPath::parse(&input), Err(PathError::ParentTraversal));
        }

        /// Arbitrary unicode never panics.
        #[test]
        fn never_panics(input in "\\PC{0,64}") {
            let _ = RelPath::parse(&input);
        }
    }
}
