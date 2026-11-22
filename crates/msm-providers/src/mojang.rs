//! Mojang's official version manifest and server downloads (Vanilla adapter metadata).
//!
//! Integrity chain: the manifest (TLS, allowlisted host) lists each version JSON with its SHA-1;
//! we verify the version JSON against it, and the version JSON lists the server jar's SHA-1 and
//! size, which the downloader verifies. Mojang publishes only SHA-1 (recorded as
//! [`crate::http::Assurance::UpstreamSha1`]).
//!
//! **Version ordering never uses semver.** Mojang moved from `1.21.11` to `26.1` in 2026; the
//! manifest lists versions newest-first, so ordering is by manifest position.

use serde::Deserialize;
use sha1::Digest as _;
use url::Url;

use crate::http::{DownloadError, DownloadSpec, Downloader, Expected, HostPolicy};

/// Official manifest URL.
pub const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

/// Hosts the Mojang adapter may contact.
#[must_use]
pub fn policy() -> HostPolicy {
    HostPolicy::new(&[
        "piston-meta.mojang.com",
        "piston-data.mojang.com",
        "launchermeta.mojang.com",
        "launcher.mojang.com",
    ])
}

const MAX_META_BYTES: u64 = 8 * 1024 * 1024;

/// Release channel of a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionKind {
    /// Full release.
    Release,
    /// Snapshot, pre-release or release candidate.
    Snapshot,
    /// Historic beta.
    OldBeta,
    /// Historic alpha.
    OldAlpha,
    /// Anything Mojang adds later.
    #[serde(other)]
    Other,
}

/// `latest` block of the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Latest {
    /// Latest release ID.
    pub release: String,
    /// Latest snapshot ID.
    pub snapshot: String,
}

/// One entry of the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestEntry {
    /// Version ID, e.g. `26.3` or `1.20.1`.
    pub id: String,
    /// Channel.
    #[serde(rename = "type")]
    pub kind: VersionKind,
    /// URL of the version JSON.
    pub url: Url,
    /// Release timestamp (RFC 3339).
    pub release_time: String,
    /// SHA-1 of the version JSON.
    pub sha1: String,
}

/// The version manifest, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VersionManifest {
    /// Latest IDs.
    pub latest: Latest,
    /// All versions, newest first.
    pub versions: Vec<ManifestEntry>,
}

impl VersionManifest {
    /// Finds a version by ID.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ManifestEntry> {
        self.versions.iter().find(|v| v.id == id)
    }

    /// Releases, newest first.
    pub fn releases(&self) -> impl Iterator<Item = &ManifestEntry> {
        self.versions
            .iter()
            .filter(|v| v.kind == VersionKind::Release)
    }

    /// Whether `id` is the same as or newer than `floor`, by manifest order. `None` if either
    /// version is unknown.
    #[must_use]
    pub fn is_at_least(&self, id: &str, floor: &str) -> Option<bool> {
        let pos = |v: &str| self.versions.iter().position(|e| e.id == v);
        Some(pos(id)? <= pos(floor)?)
    }
}

/// Java requirement declared by a version.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JavaVersion {
    /// Mojang runtime component name (informational).
    pub component: String,
    /// Required Java major version.
    pub major_version: u32,
}

/// A downloadable artifact.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Artifact {
    /// SHA-1 hex.
    pub sha1: String,
    /// Size in bytes.
    pub size: u64,
    /// URL.
    pub url: Url,
}

/// Downloads block.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Downloads {
    /// Dedicated server jar (absent for very old versions).
    pub server: Option<Artifact>,
}

/// The parts of a version JSON we use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionDetails {
    /// Version ID.
    pub id: String,
    /// Java requirement (absent on very old versions, which need Java 8).
    pub java_version: Option<JavaVersion>,
    /// Downloads.
    pub downloads: Downloads,
}

impl VersionDetails {
    /// Required Java major; versions without the field predate it and need Java 8.
    #[must_use]
    pub fn java_major(&self) -> u32 {
        self.java_version.as_ref().map_or(8, |j| j.major_version)
    }

    /// Download spec for the server jar.
    ///
    /// # Errors
    /// `Malformed` if this version has no server download.
    pub fn server_download(&self) -> Result<DownloadSpec, DownloadError> {
        let server = self.downloads.server.as_ref().ok_or_else(|| {
            DownloadError::Malformed(format!("version {} has no server download", self.id))
        })?;
        Ok(DownloadSpec {
            url: server.url.clone(),
            expected: Expected::Sha1(server.sha1.clone()),
            max_bytes: server.size,
            policy: policy(),
        })
    }
}

/// Checks bytes against a SHA-1 from the manifest.
///
/// # Errors
/// [`DownloadError::HashMismatch`].
pub fn verify_sha1(bytes: &[u8], expected: &str) -> Result<(), DownloadError> {
    let actual = hex::encode(sha1::Sha1::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(DownloadError::HashMismatch {
            expected: expected.to_owned(),
            actual,
        })
    }
}

/// Mojang metadata client.
#[derive(Debug, Clone)]
pub struct MojangClient {
    downloader: Downloader,
    manifest_url: Url,
}

impl MojangClient {
    /// Client using the official manifest URL.
    ///
    /// # Panics
    /// Never: the constant URL is valid.
    #[must_use]
    pub fn new(downloader: Downloader) -> Self {
        #[allow(clippy::expect_used)]
        let manifest_url = Url::parse(MANIFEST_URL).expect("valid constant URL");
        Self {
            downloader,
            manifest_url,
        }
    }

    /// Fetches the version manifest.
    ///
    /// # Errors
    /// Network, policy or parse errors.
    pub async fn manifest(&self) -> Result<VersionManifest, DownloadError> {
        self.downloader
            .get_json(&self.manifest_url, &policy(), MAX_META_BYTES)
            .await
    }

    /// Fetches a version JSON and verifies it against the manifest's SHA-1.
    ///
    /// # Errors
    /// Network, policy, hash or parse errors.
    pub async fn version(&self, entry: &ManifestEntry) -> Result<VersionDetails, DownloadError> {
        let bytes = self
            .downloader
            .get_bytes(&entry.url, &policy(), MAX_META_BYTES)
            .await?;
        verify_sha1(&bytes, &entry.sha1)?;
        serde_json::from_slice(&bytes).map_err(|e| DownloadError::Malformed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/upstream/mojang")
            .join(name);
        std::fs::read_to_string(path).unwrap()
    }

    fn manifest() -> VersionManifest {
        serde_json::from_str(&fixture("version_manifest_v2.json")).unwrap()
    }

    #[test]
    fn parses_real_manifest_and_orders_without_semver() {
        let m = manifest();
        assert_eq!(m.latest.release, "26.3");
        assert_eq!(m.get("26.3").unwrap().kind, VersionKind::Release);
        assert_eq!(
            m.get("26.4-snapshot-10").unwrap().kind,
            VersionKind::Snapshot
        );
        assert!(m.releases().all(|v| v.kind == VersionKind::Release));
        // 26.x is newer than 1.21.11 even though "1" > "2" fails naive string compare and
        // semver would treat "26.3" oddly.
        assert_eq!(m.is_at_least("26.3", "1.21.11"), Some(true));
        assert_eq!(m.is_at_least("1.20.1", "1.18.2"), Some(true));
        assert_eq!(m.is_at_least("1.16.5", "1.18.2"), Some(false));
        assert_eq!(m.is_at_least("1.18.2", "1.18.2"), Some(true));
        assert_eq!(m.is_at_least("9.9", "1.18.2"), None);
    }

    #[test]
    fn parses_version_details_and_java_requirements() {
        let v26: VersionDetails = serde_json::from_str(&fixture("version-26.3.json")).unwrap();
        assert_eq!(v26.java_major(), 25);
        let spec = v26.server_download().unwrap();
        assert_eq!(spec.url.host_str(), Some("piston-data.mojang.com"));
        assert!(matches!(spec.expected, Expected::Sha1(ref h) if h.len() == 40));
        assert!(policy().check(&spec.url).is_ok());

        let v120: VersionDetails = serde_json::from_str(&fixture("version-1.20.1.json")).unwrap();
        assert_eq!(v120.java_major(), 17);
    }

    #[test]
    fn missing_java_field_means_java_8_and_missing_server_is_an_error() {
        let old: VersionDetails = serde_json::from_str(r#"{"id":"1.2.5","downloads":{}}"#).unwrap();
        assert_eq!(old.java_major(), 8);
        assert!(matches!(
            old.server_download(),
            Err(DownloadError::Malformed(_))
        ));
    }

    #[test]
    fn unknown_kinds_and_fields_are_tolerated() {
        let m: VersionManifest = serde_json::from_str(
            r#"{"latest":{"release":"x","snapshot":"y"},"versions":[{"id":"x","type":"experimental","url":"https://piston-meta.mojang.com/x.json","time":"t","releaseTime":"t","sha1":"00","complianceLevel":1,"newField":true}]}"#,
        )
        .unwrap();
        assert_eq!(m.versions[0].kind, VersionKind::Other);
    }

    #[test]
    fn malformed_manifest_is_an_error() {
        assert!(serde_json::from_str::<VersionManifest>(r#"{"latest":{}}"#).is_err());
    }

    #[test]
    fn version_json_hash_is_checked() {
        assert!(verify_sha1(b"abc", "a9993e364706816aba3e25717850c26c9cd0d89d").is_ok());
        assert!(matches!(
            verify_sha1(b"abd", "a9993e364706816aba3e25717850c26c9cd0d89d"),
            Err(DownloadError::HashMismatch { .. })
        ));
    }
}

/// Hosts used for player profile lookups.
#[must_use]
pub fn profile_policy() -> HostPolicy {
    HostPolicy::new(&["api.mojang.com"])
}

#[derive(Deserialize)]
struct Profile {
    id: String,
}

/// Looks up the UUID (32 hex digits, no dashes) of a Java Edition account by name via
/// `https://api.mojang.com/users/profiles/minecraft/<name>` (verified 2026-11-21). Returns
/// `Ok(None)` when no such account exists. The caller must pass an already validated player
/// name (`[A-Za-z0-9_]{1,16}`).
///
/// # Errors
/// Network, policy or parse errors.
pub async fn profile_id(
    downloader: &Downloader,
    name: &str,
) -> Result<Option<String>, DownloadError> {
    if !(1..=16).contains(&name.len())
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(DownloadError::Malformed("invalid player name".into()));
    }
    let url = Url::parse(&format!(
        "https://api.mojang.com/users/profiles/minecraft/{name}"
    ))
    .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    match downloader
        .get_bytes(&url, &profile_policy(), 64 * 1024)
        .await
    {
        Ok(bytes) => {
            let p: Profile = serde_json::from_slice(&bytes)
                .map_err(|e| DownloadError::Malformed(e.to_string()))?;
            Ok(Some(p.id))
        }
        Err(DownloadError::Status(404 | 204)) => Ok(None),
        Err(e) => Err(e),
    }
}
