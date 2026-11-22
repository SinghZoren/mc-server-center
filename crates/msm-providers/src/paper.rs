//! PaperMC via the Fill v3 downloads service (ADR-005). STABLE builds only, sha256-verified.
//! Endpoints and shapes verified 2026-11-21 (`fixtures/upstream/paper/`).

use serde::Deserialize;
use std::collections::BTreeMap;
use url::Url;

use crate::http::{DownloadError, DownloadSpec, Downloader, Expected, HostPolicy};

const API: &str = "https://fill.papermc.io/v3/projects/paper";

/// Hosts for Paper metadata and jars.
#[must_use]
pub fn policy() -> HostPolicy {
    HostPolicy::new(&["fill.papermc.io", "fill-data.papermc.io"])
}

#[derive(Debug, Deserialize)]
struct Project {
    versions: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Build {
    id: u64,
    channel: String,
    downloads: BTreeMap<String, Download>,
}

#[derive(Debug, Deserialize)]
struct Download {
    name: String,
    checksums: Checksums,
    size: u64,
    url: Url,
}

#[derive(Debug, Deserialize)]
struct Checksums {
    sha256: String,
}

/// A resolved Paper build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaperBuild {
    /// Minecraft version.
    pub mc_version: String,
    /// Build number.
    pub build: u64,
    /// Jar file name.
    pub file_name: String,
    /// Verified download.
    pub download: DownloadSpec,
}

/// Minecraft versions Paper supports (release versions only; `-pre`/`-rc` excluded).
fn parse_versions(project: Project) -> Vec<String> {
    project
        .versions
        .into_values()
        .flatten()
        .filter(|v| !v.contains("-pre") && !v.contains("-rc") && !v.contains("snapshot"))
        .collect()
}

fn pick_stable(mc_version: &str, builds: Vec<Build>) -> Result<PaperBuild, DownloadError> {
    let build = builds
        .into_iter()
        .filter(|b| b.channel == "STABLE")
        .max_by_key(|b| b.id)
        .ok_or_else(|| {
            DownloadError::Malformed(format!("no stable Paper build for {mc_version}"))
        })?;
    let id = build.id;
    let d = build
        .downloads
        .into_iter()
        .find_map(|(k, v)| (k == "server:default").then_some(v))
        .ok_or_else(|| DownloadError::Malformed("Paper build has no server download".into()))?;
    if d.checksums.sha256.len() != 64 {
        return Err(DownloadError::Malformed(
            "Paper build has no valid sha256".into(),
        ));
    }
    policy().check(&d.url)?;
    Ok(PaperBuild {
        mc_version: mc_version.to_owned(),
        build: id,
        file_name: d.name,
        download: DownloadSpec {
            url: d.url,
            expected: Expected::Sha256(d.checksums.sha256),
            max_bytes: d.size,
            policy: policy(),
        },
    })
}

/// Lists Minecraft versions with Paper builds.
///
/// # Errors
/// Network, policy or parse errors.
pub async fn versions(dl: &Downloader) -> Result<Vec<String>, DownloadError> {
    let url = Url::parse(API).map_err(|e| DownloadError::Malformed(e.to_string()))?;
    let project: Project = dl.get_json(&url, &policy(), 4 * 1024 * 1024).await?;
    Ok(parse_versions(project))
}

/// Resolves the newest STABLE build for a Minecraft version.
///
/// # Errors
/// Network, policy or parse errors; `Malformed` if no stable build exists.
pub async fn latest_stable(dl: &Downloader, mc_version: &str) -> Result<PaperBuild, DownloadError> {
    if !mc_version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(DownloadError::Malformed("invalid version".into()));
    }
    let url = Url::parse(&format!("{API}/versions/{mc_version}/builds"))
        .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    let builds: Vec<Build> = dl.get_json(&url, &policy(), 16 * 1024 * 1024).await?;
    pick_stable(mc_version, builds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/upstream/paper")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn parses_real_project_and_builds() {
        let versions = parse_versions(serde_json::from_str(&fixture("project.json")).unwrap());
        assert!(versions.contains(&"26.3".to_string()));
        assert!(versions.contains(&"1.21.1".to_string()));
        assert!(!versions.iter().any(|v| v.contains("-rc")));

        let builds: Vec<Build> = serde_json::from_str(&fixture("builds-1.21.1.json")).unwrap();
        let b = pick_stable("1.21.1", builds).unwrap();
        assert_eq!(b.build, 133);
        assert_eq!(b.download.url.host_str(), Some("fill-data.papermc.io"));
        assert!(matches!(b.download.expected, Expected::Sha256(ref h) if h.len() == 64));
    }

    #[test]
    fn ignores_non_stable_and_rejects_bad_hosts() {
        let builds: Vec<Build> =
            serde_json::from_str(&fixture("builds-1.21.1.json").replace("\"STABLE\"", "\"ALPHA\""))
                .unwrap();
        assert!(pick_stable("1.21.1", builds).is_err());
        let evil: Vec<Build> = serde_json::from_str(
            &fixture("builds-1.21.1.json")
                .replace("https://fill-data.papermc.io", "https://evil.example"),
        )
        .unwrap();
        assert!(matches!(
            pick_stable("1.21.1", evil),
            Err(DownloadError::HostNotAllowed(_))
        ));
    }
}
