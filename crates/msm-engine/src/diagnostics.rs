//! Deterministic troubleshooting (DIA-01, DIA-02): pure rules over console output and the exit
//! code. Same input, same findings; no network or AI. Each finding has a stable code, the
//! evidence line that triggered it, and a plain-language next step.

use serde::Serialize;

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The server cannot run until this is fixed.
    Error,
    /// Something is wrong but the server may run.
    Warning,
}

/// A suggested fix the UI can offer as a button.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Repair {
    /// Install a specific Java major.
    InstallJava {
        /// Major version.
        major: u32,
    },
    /// Accept the EULA (opens the EULA confirmation).
    AcceptEula,
    /// Change memory in settings.
    AdjustMemory,
    /// Change the port.
    ChangePort,
    /// Restore a backup.
    RestoreBackup,
}

/// A diagnosis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// Stable code, e.g. `MSM-JAVA-001`.
    pub code: &'static str,
    /// Severity.
    pub severity: Severity,
    /// Short headline.
    pub title: String,
    /// What to do.
    pub advice: String,
    /// The console line that matched (already plain text).
    pub evidence: Option<String>,
    /// One-click repair, if any.
    pub repair: Option<Repair>,
}

fn class_version_to_java(v: u32) -> Option<u32> {
    // Class file version 52 = Java 8, so major = version - 44.
    v.checked_sub(44).filter(|m| (8..=40).contains(m))
}

/// Analyses the most recent console lines (oldest first) and the exit code.
#[must_use]
pub fn analyze(lines: &[String], exit_code: Option<i32>, java_major: u32) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    let find = |pred: &dyn Fn(&str) -> bool| lines.iter().rev().find(|l| pred(l)).cloned();
    let mut push = |f: Finding| {
        if !out.iter().any(|o| o.code == f.code) {
            out.push(f);
        }
    };

    if let Some(line) = find(&|l| {
        l.contains("UnsupportedClassVersionError")
            || l.contains("compiled by a more recent version of the Java Runtime")
    }) {
        let needed = line
            .split("class file version ")
            .nth(1)
            .and_then(|s| s.split(['.', ')', ' ']).next())
            .and_then(|v| v.parse::<u32>().ok())
            .and_then(class_version_to_java);
        push(Finding {
            code: "MSM-JAVA-001",
            severity: Severity::Error,
            title: "The server needs a newer Java".into(),
            advice: match needed {
                Some(m) => format!(
                    "This server (or one of its mods) needs Java {m}, but it was started with Java {java_major}. Install Java {m} and start again."
                ),
                None => format!(
                    "Something in this server needs a newer Java than {java_major}. Check the mods' requirements."
                ),
            },
            evidence: Some(line),
            repair: needed.map(|major| Repair::InstallJava { major }),
        });
    }
    if let Some(line) =
        find(&|l| l.contains("FAILED TO BIND TO PORT") || l.contains("Address already in use"))
    {
        push(Finding {
            code: "MSM-PORT-001",
            severity: Severity::Error,
            title: "The port is already in use".into(),
            advice: "Another program (often another Minecraft server) is using this port. Stop it, or change this server's port.".into(),
            evidence: Some(line),
            repair: Some(Repair::ChangePort),
        });
    }
    if let Some(line) = find(&|l| {
        l.contains("Could not reserve enough space")
            || l.contains("Invalid maximum heap size")
            || l.contains("Invalid initial heap size")
    }) {
        push(Finding {
            code: "MSM-MEM-002",
            severity: Severity::Error,
            title: "Too much memory requested".into(),
            advice: "Java couldn't get the memory this server asks for. Lower the server's memory, or close other programs.".into(),
            evidence: Some(line),
            repair: Some(Repair::AdjustMemory),
        });
    } else if let Some(line) = find(&|l| l.contains("java.lang.OutOfMemoryError")) {
        push(Finding {
            code: "MSM-MEM-001",
            severity: Severity::Error,
            title: "The server ran out of memory".into(),
            advice: "Give the server more memory (modpacks often need 4–8 GB), or lower the view distance.".into(),
            evidence: Some(line),
            repair: Some(Repair::AdjustMemory),
        });
    }
    if let Some(line) = find(&|l| l.contains("You need to agree to the EULA")) {
        push(Finding {
            code: "MSM-EULA-001",
            severity: Severity::Error,
            title: "The Minecraft EULA hasn't been accepted".into(),
            advice: "Accept the Minecraft EULA, then start the server again.".into(),
            evidence: Some(line),
            repair: Some(Repair::AcceptEula),
        });
    }
    if let Some(line) = find(&|l| {
        l.contains("Incompatible mods found")
            || l.contains("Missing or unsupported mandatory dependencies")
            || l.contains("requires any version of")
            || l.contains("is incompatible with")
            || l.contains("which is missing!")
    }) {
        push(Finding {
            code: "MSM-MOD-001",
            severity: Severity::Error,
            title: "A mod is missing something it needs".into(),
            advice: "One of the installed mods needs another mod, or a different version. The console shows which; add the missing mod or remove the one that needs it.".into(),
            evidence: Some(line),
            repair: None,
        });
    }
    if let Some(line) = find(&|l| {
        l.contains("Failed to load level")
            || l.contains("Exception reading") && l.contains("level.dat")
            || l.contains("Failed to load datapacks")
    }) {
        push(Finding {
            code: "MSM-WORLD-001",
            severity: Severity::Error,
            title: "The world couldn't be loaded".into(),
            advice: "The world files may be damaged or from a newer version. Restore a backup, or check the console for details.".into(),
            evidence: Some(line),
            repair: Some(Repair::RestoreBackup),
        });
    }
    if let Some(line) = find(&|l| {
        l.contains("Unable to access jarfile")
            || l.contains("Error: Could not find or load main class")
    }) {
        push(Finding {
            code: "MSM-FILES-001",
            severity: Severity::Error,
            title: "Server files are missing".into(),
            advice: "The server's program file is missing or damaged. Restore a backup, or create the server again.".into(),
            evidence: Some(line),
            repair: Some(Repair::RestoreBackup),
        });
    }
    if out.is_empty() && exit_code.is_some_and(|c| c != 0) {
        let last_error =
            find(&|l| l.contains("ERROR") || l.contains("Exception") || l.contains("FATAL"));
        out.push(Finding {
            code: "MSM-CRASH-000",
            severity: Severity::Error,
            title: "The server stopped unexpectedly".into(),
            advice: "No known cause was recognised. The highlighted line below and the console's last messages usually say why.".into(),
            evidence: last_error,
            repair: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn java_version_mismatch_names_the_needed_major() {
        let f = analyze(
            &lines(&[
                "Error: LinkageError occurred while loading main class net.minecraft.bundler.Main",
                "java.lang.UnsupportedClassVersionError: net/minecraft/bundler/Main has been compiled by a more recent version of the Java Runtime (class file version 69.0), this version of the Java Runtime only recognizes class file versions up to 61.0",
            ]),
            Some(1),
            17,
        );
        assert_eq!(f[0].code, "MSM-JAVA-001");
        assert_eq!(f[0].repair, Some(Repair::InstallJava { major: 25 }));
        assert!(f[0].advice.contains("Java 25"));
    }

    #[test]
    fn recognises_common_failures() {
        let cases = [
            (
                "[12:00:00] [Server thread/WARN]: **** FAILED TO BIND TO PORT!",
                "MSM-PORT-001",
            ),
            (
                "Error occurred during initialization of VM: Could not reserve enough space for 16777216KB object heap",
                "MSM-MEM-002",
            ),
            ("java.lang.OutOfMemoryError: Java heap space", "MSM-MEM-001"),
            (
                "[12:00:00] [ServerMain/INFO]: You need to agree to the EULA in order to run the server. Go to eula.txt for more info.",
                "MSM-EULA-001",
            ),
            ("Incompatible mods found!", "MSM-MOD-001"),
            (
                "Error: Unable to access jarfile server.jar",
                "MSM-FILES-001",
            ),
        ];
        for (line, code) in cases {
            let f = analyze(&lines(&[line]), Some(1), 21);
            assert_eq!(f.first().map(|x| x.code), Some(code), "{line}");
        }
    }

    #[test]
    fn unknown_crash_gets_a_generic_finding_and_clean_exit_gets_none() {
        let f = analyze(
            &lines(&[
                "[12:00:00] [Server thread/ERROR]: Encountered an unexpected exception",
                "x",
            ]),
            Some(1),
            21,
        );
        assert_eq!(f[0].code, "MSM-CRASH-000");
        assert!(
            f[0].evidence
                .as_deref()
                .unwrap_or("")
                .contains("unexpected exception")
        );
        assert!(analyze(&lines(&["Stopping server"]), Some(0), 21).is_empty());
    }

    #[test]
    fn findings_are_deterministic_and_deduplicated() {
        let input = lines(&[
            "java.lang.OutOfMemoryError: a",
            "java.lang.OutOfMemoryError: b",
        ]);
        let a = analyze(&input, Some(1), 21);
        assert_eq!(a, analyze(&input, Some(1), 21));
        assert_eq!(a.len(), 1);
        assert_eq!(
            a[0].evidence.as_deref(),
            Some("java.lang.OutOfMemoryError: b")
        );
    }
}
