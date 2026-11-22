//! Lossless reader/writer for Java `.properties` files such as `server.properties`.
//!
//! The editor must preserve comments, ordering, unknown keys and the original formatting of
//! every line it does not change (CFG-03). This codec therefore keeps each logical line's raw
//! text, including its own line terminator, and only re-serializes entries that were modified.
//!
//! Parsing follows `java.util.Properties.load`: `#`/`!` comments, `=`/`:`/whitespace
//! separators, backslash escapes (`\t \n \r \f \uXXXX`), and line continuations. When a key
//! appears more than once, the **last** occurrence wins, as in Java.
//!
//! Line terminators are `\n` and `\r\n`. Java also accepts a lone `\r`; Minecraft never writes
//! one, and we keep it as ordinary content so the round trip stays exact.

use std::fmt::Write as _;

/// One logical line of the file. Every `raw` includes its own terminator (if any), so
/// concatenating all raws reproduces the input exactly, even with mixed line endings.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line {
    /// Comment, blank line, or anything else preserved verbatim.
    Other(String),
    /// A key/value entry. `raw` is the original text (may span continuation lines) and is
    /// emitted unchanged unless the entry is modified.
    Entry {
        key: String,
        value: String,
        raw: String,
    },
}

/// A parsed properties document that round-trips byte-for-byte when unmodified.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PropertiesDoc {
    lines: Vec<Line>,
}

const WS: [char; 3] = [' ', '\t', '\x0c'];

fn split_terminator(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else {
        (line, "")
    }
}

impl PropertiesDoc {
    /// Parses `text`. Never fails: lines that cannot be understood are kept verbatim.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let physical: Vec<&str> = text.split_inclusive('\n').collect();
        let mut lines = Vec::new();
        let mut i = 0;
        while i < physical.len() {
            let (body, _) = split_terminator(physical[i]);
            let trimmed = body.trim_start_matches(WS);
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                lines.push(Line::Other(physical[i].to_owned()));
                i += 1;
                continue;
            }
            // A line ending in an odd number of backslashes continues on the next line.
            let mut raw = String::from(physical[i]);
            let mut logical = String::from(trimmed);
            while ends_with_odd_backslashes(&logical) && i + 1 < physical.len() {
                logical.pop();
                i += 1;
                raw.push_str(physical[i]);
                logical.push_str(split_terminator(physical[i]).0.trim_start_matches(WS));
            }
            i += 1;
            let (key, value) = split_entry(&logical);
            lines.push(Line::Entry { key, value, raw });
        }
        Self { lines }
    }

    /// Returns the effective value for `key` (last occurrence wins).
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.lines.iter().rev().find_map(|l| match l {
            Line::Entry { key: k, value, .. } if k == key => Some(value.as_str()),
            _ => None,
        })
    }

    /// Effective key/value pairs in file order (duplicates resolved to their last value).
    #[must_use]
    pub fn entries(&self) -> Vec<(&str, &str)> {
        let mut out: Vec<(&str, &str)> = Vec::new();
        for line in &self.lines {
            if let Line::Entry { key, value, .. } = line {
                if let Some(existing) = out.iter_mut().find(|(k, _)| *k == key) {
                    existing.1 = value;
                } else {
                    out.push((key, value));
                }
            }
        }
        out
    }

    fn raws(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(|l| match l {
            Line::Other(raw) | Line::Entry { raw, .. } => raw.as_str(),
        })
    }

    /// Newline style for new lines: whichever the file mostly uses, `\n` if none.
    fn preferred_newline(&self) -> &'static str {
        let crlf = self.raws().filter(|r| r.ends_with("\r\n")).count();
        let lf = self.raws().filter(|r| r.ends_with('\n')).count() - crlf;
        if crlf > lf { "\r\n" } else { "\n" }
    }

    /// Sets `key` to `value`. Updates the effective (last) occurrence in place, keeping its
    /// position and line ending; appends a new entry at the end if the key is absent. Setting
    /// the current value leaves the original text untouched.
    pub fn set(&mut self, key: &str, value: &str) {
        let newline = self.preferred_newline();
        let existing = self.lines.iter_mut().rev().find_map(|l| match l {
            Line::Entry {
                key: k,
                value: v,
                raw,
            } if k == key => Some((v, raw)),
            _ => None,
        });
        match existing {
            Some((v, _)) if v == value => {}
            Some((v, raw)) => {
                let terminator = split_terminator(raw.as_str()).1;
                *raw = format_entry(key, value) + terminator;
                *v = value.to_owned();
            }
            None => {
                // Terminate the previous last line before appending.
                if let Some(Line::Other(raw) | Line::Entry { raw, .. }) = self.lines.last_mut()
                    && !raw.ends_with('\n')
                {
                    raw.push_str(newline);
                }
                self.lines.push(Line::Entry {
                    key: key.to_owned(),
                    value: value.to_owned(),
                    raw: format_entry(key, value) + newline,
                });
            }
        }
    }

    /// Removes every occurrence of `key`. Returns whether anything was removed.
    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.lines.len();
        self.lines
            .retain(|l| !matches!(l, Line::Entry { key: k, .. } if k == key));
        before != self.lines.len()
    }

    /// Serializes the document. Unmodified documents are reproduced byte-for-byte.
    #[must_use]
    pub fn render(&self) -> String {
        self.raws().collect()
    }
}

fn ends_with_odd_backslashes(s: &str) -> bool {
    s.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
}

/// Splits a logical line (leading whitespace already removed) into unescaped key and value.
fn split_entry(logical: &str) -> (String, String) {
    let chars: Vec<char> = logical.chars().collect();
    let mut key_end = chars.len();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '=' | ':' | ' ' | '\t' | '\x0c' => {
                key_end = i;
                break;
            }
            _ => i += 1,
        }
    }
    let key_end = key_end.min(chars.len());
    // Skip whitespace, then at most one '=' or ':', then whitespace again.
    let mut j = key_end;
    while j < chars.len() && WS.contains(&chars[j]) {
        j += 1;
    }
    if j < chars.len() && matches!(chars[j], '=' | ':') {
        j += 1;
        while j < chars.len() && WS.contains(&chars[j]) {
            j += 1;
        }
    }
    let key: String = chars[..key_end].iter().collect();
    let value: String = chars[j.min(chars.len())..].iter().collect();
    (unescape(&key), unescape(&value))
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('f') => out.push('\x0c'),
            Some('u') => {
                let hex: String = chars.clone().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(decoded) if hex.len() == 4 => {
                        out.push(decoded);
                        for _ in 0..4 {
                            chars.next();
                        }
                    }
                    // Java throws on malformed \u; we keep the text rather than fail.
                    _ => out.push('u'),
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn escape(s: &str, is_key: bool, out: &mut String) {
    for (idx, c) in s.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\x0c' => out.push_str("\\f"),
            '=' | ':' | '#' | '!' => {
                out.push('\\');
                out.push(c);
            }
            ' ' if is_key || idx == 0 => out.push_str("\\ "),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// Formats a new or modified entry the way Java's `Properties.store` does (`key=value`).
fn format_entry(key: &str, value: &str) -> String {
    let mut out = String::new();
    escape(key, true, &mut out);
    out.push('=');
    escape(value, false, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const SAMPLE: &str = "#Minecraft server properties\n\
#Fri Nov 21 13:00:00 UTC 2026\n\
accepts-transfers=false\n\
allow-flight=false\n\
difficulty=easy\n\
motd=A Minecraft Server\n\
\n\
! custom comment\n\
level-seed=\n\
server-port=25565\n\
rcon.password=\n\
custom.mod-key = hello world\n";

    #[test]
    fn unmodified_documents_round_trip_exactly() {
        let samples = [
            SAMPLE,
            "",
            "a=1",
            "a=1\r\nb=2\r\n",
            "a=1\nb=2\r\nc=3",
            "  # indented comment\n\tkey\tvalue\n",
            "multi=line one \\\n    line two\nnext=1\n",
            "weird line without separator\n",
        ];
        for s in samples {
            assert_eq!(PropertiesDoc::parse(s).render(), s, "sample {s:?}");
        }
    }

    #[test]
    fn reads_java_separator_and_escape_rules() {
        let doc = PropertiesDoc::parse(
            "a=1\nb:2\nc 3\nd = 4\ne\t=\t5\nf\n\
             g=multi \\\n   part\n\
             h=tab\\tnew\\nline\n\
             i=\\u00e9t\\u00E9\n\
             j\\ k=space in key\n\
             l=colon\\:and\\=equals\n",
        );
        assert_eq!(doc.get("a"), Some("1"));
        assert_eq!(doc.get("b"), Some("2"));
        assert_eq!(doc.get("c"), Some("3"));
        assert_eq!(doc.get("d"), Some("4"));
        assert_eq!(doc.get("e"), Some("5"));
        assert_eq!(doc.get("f"), Some(""));
        assert_eq!(doc.get("g"), Some("multi part"));
        assert_eq!(doc.get("h"), Some("tab\tnew\nline"));
        assert_eq!(doc.get("i"), Some("été"));
        assert_eq!(doc.get("j k"), Some("space in key"));
        assert_eq!(doc.get("l"), Some("colon:and=equals"));
    }

    #[test]
    fn last_duplicate_wins_and_set_updates_it() {
        let mut doc = PropertiesDoc::parse("a=1\nb=2\na=3\n");
        assert_eq!(doc.get("a"), Some("3"));
        assert_eq!(doc.entries(), vec![("a", "3"), ("b", "2")]);
        doc.set("a", "4");
        assert_eq!(doc.render(), "a=1\nb=2\na=4\n");
    }

    #[test]
    fn set_preserves_everything_else() {
        let mut doc = PropertiesDoc::parse(SAMPLE);
        doc.set("difficulty", "hard");
        doc.set("motd", "Friends only: no griefing!");
        doc.set("brand-new", "yes");
        let out = doc.render();
        let expected = SAMPLE
            .replace("difficulty=easy", "difficulty=hard")
            .replace(
                "motd=A Minecraft Server",
                "motd=Friends only\\: no griefing\\!",
            )
            + "brand-new=yes\n";
        assert_eq!(out, expected);
        let reparsed = PropertiesDoc::parse(&out);
        assert_eq!(reparsed.get("motd"), Some("Friends only: no griefing!"));
        assert_eq!(reparsed.get("custom.mod-key"), Some("hello world"));
    }

    #[test]
    fn setting_the_same_value_keeps_original_formatting() {
        let mut doc = PropertiesDoc::parse("custom.mod-key = hello world\n");
        doc.set("custom.mod-key", "hello world");
        assert_eq!(doc.render(), "custom.mod-key = hello world\n");
    }

    #[test]
    fn newline_style_is_preserved() {
        let mut doc = PropertiesDoc::parse("a=1\r\n");
        doc.set("b", "2");
        assert_eq!(doc.render(), "a=1\r\nb=2\r\n");
        let mut doc = PropertiesDoc::parse("a=1");
        doc.set("b", "2");
        assert_eq!(doc.render(), "a=1\nb=2\n");
        let mut doc = PropertiesDoc::parse("a=1\r\nb=2");
        doc.set("b", "3");
        assert_eq!(doc.render(), "a=1\r\nb=3");
    }

    #[test]
    fn remove_deletes_all_occurrences() {
        let mut doc = PropertiesDoc::parse("a=1\nb=2\na=3\n");
        assert!(doc.remove("a"));
        assert!(!doc.remove("a"));
        assert_eq!(doc.render(), "b=2\n");
    }

    #[test]
    fn empty_document_gets_entries() {
        let mut doc = PropertiesDoc::parse("");
        doc.set("eula", "true");
        assert_eq!(doc.render(), "eula=true\n");
    }

    proptest! {
        /// Any key/value set through the API reads back identically after a render/parse cycle,
        /// and other entries are unaffected.
        #[test]
        fn set_round_trips(key in "[a-zA-Z0-9.\\-_ :=#!\\\\é]{1,20}", value in "\\PC{0,40}") {
            let mut doc = PropertiesDoc::parse(SAMPLE);
            doc.set(&key, &value);
            let reparsed = PropertiesDoc::parse(&doc.render());
            prop_assert_eq!(reparsed.get(&key), Some(value.as_str()));
            if key != "server-port" {
                prop_assert_eq!(reparsed.get("server-port"), Some("25565"));
            }
        }

        /// Arbitrary input never panics and always round-trips when unmodified.
        #[test]
        fn arbitrary_text_round_trips(text in "[ -~\\n\\r\\t\\\\é]{0,200}") {
            prop_assert_eq!(PropertiesDoc::parse(&text).render(), text);
        }
    }
}
