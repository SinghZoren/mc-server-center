//! Java runtime discovery and probing (JAVA-01, JAVA-03).
//!
//! A runtime is only trusted after we have *run* it and read its own system properties: we never
//! infer the version from a folder name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Facts reported by a Java executable about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaInfo {
    /// Path to the `java` executable that was probed (canonical).
    pub executable: PathBuf,
    /// Feature (major) version, e.g. 21.
    pub major: u32,
    /// Full runtime version string, e.g. `21.0.5+11-LTS`.
    pub runtime_version: String,
    /// Vendor, e.g. `Eclipse Adoptium`.
    pub vendor: String,
    /// `os.arch`, e.g. `amd64`, `x86`, `aarch64`.
    pub arch: String,
    /// Whether the JVM is 64-bit.
    pub is_64bit: bool,
}

/// Why a candidate could not be used.
#[derive(Debug, thiserror::Error)]
pub enum JavaError {
    /// The executable could not be started.
    #[error("could not run {path:?}: {source}")]
    Spawn {
        /// Executable.
        path: PathBuf,
        /// Cause.
        #[source]
        source: std::io::Error,
    },
    /// It ran, but did not report the expected properties.
    #[error("{0:?} did not report its version")]
    Unrecognized(PathBuf),
    /// It did not answer in time.
    #[error("{0:?} timed out")]
    Timeout(PathBuf),
}

/// Parses the `-XshowSettings:properties` block (`    key = value` lines).
#[must_use]
pub fn parse_properties(output: &str) -> BTreeMap<String, String> {
    let mut props = BTreeMap::new();
    for line in output.lines() {
        if !line.starts_with("    ") || line.starts_with("        ") {
            continue; // continuation lines of multi-value properties are more indented
        }
        if let Some((k, v)) = line.trim().split_once(" = ") {
            props.insert(k.trim().to_owned(), v.trim().to_owned());
        }
    }
    props
}

/// Derives the major version from `java.specification.version` (`1.8` → 8, `21` → 21).
#[must_use]
pub fn major_from_spec(spec: &str) -> Option<u32> {
    let s = spec.strip_prefix("1.").unwrap_or(spec);
    s.split('.').next()?.parse().ok()
}

fn info_from_props(executable: PathBuf, props: &BTreeMap<String, String>) -> Option<JavaInfo> {
    let major = major_from_spec(props.get("java.specification.version")?)?;
    let arch = props.get("os.arch").cloned().unwrap_or_default();
    let is_64bit = props
        .get("sun.arch.data.model")
        .map_or_else(|| arch.contains("64"), |m| m == "64");
    Some(JavaInfo {
        executable,
        major,
        runtime_version: props
            .get("java.runtime.version")
            .or_else(|| props.get("java.version"))
            .cloned()
            .unwrap_or_default(),
        vendor: props.get("java.vendor").cloned().unwrap_or_default(),
        arch,
        is_64bit,
    })
}

/// Runs `java -XshowSettings:properties -version` and parses the result.
///
/// # Errors
/// See [`JavaError`].
pub async fn probe(executable: &Path) -> Result<JavaInfo, JavaError> {
    let path = std::fs::canonicalize(executable).unwrap_or_else(|_| executable.to_owned());
    let mut cmd = tokio::process::Command::new(&path);
    cmd.args(["-XshowSettings:properties", "-version"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(crate::process::CREATE_NO_WINDOW);
    let child = cmd.spawn().map_err(|source| JavaError::Spawn {
        path: path.clone(),
        source,
    })?;
    let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .map_err(|_| JavaError::Timeout(path.clone()))?
        .map_err(|source| JavaError::Spawn {
            path: path.clone(),
            source,
        })?;
    // Settings are printed to stderr.
    let text = String::from_utf8_lossy(&output.stderr);
    let props = parse_properties(&text);
    info_from_props(path.clone(), &props).ok_or(JavaError::Unrecognized(path))
}

fn exe_name() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

/// Lists plausible Java executables on this machine (not yet probed): `JAVA_HOME`, `PATH`, and
/// common install locations. `extra_roots` are directories containing JDK/JRE folders (e.g. our
/// managed runtimes directory).
#[must_use]
pub fn candidates(extra_roots: &[PathBuf]) -> Vec<PathBuf> {
    let exe = exe_name();
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if p.is_file() && !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        push(PathBuf::from(home).join("bin").join(exe));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            push(dir.join(exe));
        }
    }
    let mut roots: Vec<PathBuf> = extra_roots.to_vec();
    if cfg!(windows) {
        for base in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(pf) = std::env::var_os(base) {
                let pf = PathBuf::from(pf);
                for vendor in [
                    "Java",
                    "Eclipse Adoptium",
                    "Eclipse Foundation",
                    "Microsoft",
                    "Zulu",
                    "Amazon Corretto",
                    "BellSoft",
                ] {
                    roots.push(pf.join(vendor));
                }
            }
        }
    } else {
        roots.push(PathBuf::from("/usr/lib/jvm"));
        roots.push(PathBuf::from("/opt/java"));
    }
    for root in roots {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                push(entry.path().join("bin").join(exe));
            }
        }
    }
    out
}

/// Probes every candidate and returns the working runtimes, de-duplicated by canonical path.
pub async fn discover(extra_roots: &[PathBuf]) -> Vec<JavaInfo> {
    let mut found: Vec<JavaInfo> = Vec::new();
    for c in candidates(extra_roots) {
        if let Ok(info) = probe(&c).await
            && !found.iter().any(|f| f.executable == info.executable)
        {
            found.push(info);
        }
    }
    found
}

/// Picks the best runtime for a requirement: exactly the required major, 64-bit, preferring
/// managed runtimes (paths under one of `managed_roots`). Newer majors are **not** chosen
/// automatically (modded servers often break on them); the user can override explicitly.
#[must_use]
pub fn select<'a>(
    runtimes: &'a [JavaInfo],
    required_major: u32,
    managed_roots: &[PathBuf],
) -> Option<&'a JavaInfo> {
    let is_managed = |j: &JavaInfo| managed_roots.iter().any(|r| j.executable.starts_with(r));
    runtimes
        .iter()
        .filter(|j| j.major == required_major && j.is_64bit)
        .max_by_key(|j| (is_managed(j), j.runtime_version.clone()))
}

/// Probes only runtimes installed directly under `roots` (`<root>/<name>/bin/java`).
pub async fn discover_in(roots: &[PathBuf]) -> Vec<JavaInfo> {
    let mut found = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let exe = entry.path().join("bin").join(exe_name());
            if exe.is_file()
                && let Ok(info) = probe(&exe).await
            {
                found.push(info);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Property settings:
    file.encoding = UTF-8
    java.home = C:\\Program Files\\Java\\jdk-25
    java.library.path = C:\\a
        C:\\b
    java.runtime.version = 25+37-LTS-3491
    java.specification.version = 25
    java.vendor = Oracle Corporation
    os.arch = amd64
    sun.arch.data.model = 64

java version \"25\" 2025-09-16 LTS
";

    #[test]
    fn parses_settings_output() {
        let props = parse_properties(SAMPLE);
        assert_eq!(
            props.get("java.specification.version").map(String::as_str),
            Some("25")
        );
        assert_eq!(
            props.get("java.library.path").map(String::as_str),
            Some("C:\\a")
        );
        let info = info_from_props(PathBuf::from("java"), &props).unwrap();
        assert_eq!(info.major, 25);
        assert!(info.is_64bit);
        assert_eq!(info.vendor, "Oracle Corporation");
    }

    #[test]
    fn spec_version_formats() {
        assert_eq!(major_from_spec("1.8"), Some(8));
        assert_eq!(major_from_spec("17"), Some(17));
        assert_eq!(major_from_spec("21.0"), Some(21));
        assert_eq!(major_from_spec("x"), None);
    }

    #[test]
    fn selection_requires_exact_major_and_prefers_managed() {
        let mk = |path: &str, major, is_64bit| JavaInfo {
            executable: PathBuf::from(path),
            major,
            runtime_version: "v".into(),
            vendor: String::new(),
            arch: String::new(),
            is_64bit,
        };
        let all = vec![
            mk("/sys/jdk21/bin/java", 21, true),
            mk("/managed/temurin-21/bin/java", 21, true),
            mk("/sys/jdk25/bin/java", 25, true),
            mk("/sys/jre17-32/bin/java", 17, false),
        ];
        let managed = vec![PathBuf::from("/managed")];
        assert_eq!(
            select(&all, 21, &managed).unwrap().executable,
            PathBuf::from("/managed/temurin-21/bin/java")
        );
        assert!(
            select(&all, 17, &managed).is_none(),
            "32-bit is never selected"
        );
        assert!(
            select(&all, 22, &managed).is_none(),
            "no automatic newer major"
        );
    }

    /// Runs against whatever Java is installed; skipped silently when there is none.
    #[tokio::test]
    async fn probes_installed_java_if_present() {
        let found = discover(&[]).await;
        for j in &found {
            assert!(j.major >= 8, "{j:?}");
        }
    }
}
