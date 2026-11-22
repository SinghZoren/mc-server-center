//! Modrinth API v2 (ADR-008): search, project versions, files with SHA-512.
//! Shapes verified 2026-11-21 (`fixtures/upstream/modrinth/`). Modrinth asks for a unique
//! User-Agent (ours identifies the app) and allows 300 requests/minute per IP.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::http::{DownloadError, DownloadSpec, Downloader, Expected, HostPolicy};

const API: &str = "https://api.modrinth.com/v2";

/// Hosts for Modrinth metadata and files.
#[must_use]
pub fn policy() -> HostPolicy {
    HostPolicy::new(&["api.modrinth.com", "cdn.modrinth.com"])
}

/// A search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    /// Project ID.
    pub project_id: String,
    /// URL slug.
    pub slug: String,
    /// Title.
    pub title: String,
    /// Short description.
    pub description: String,
    /// Author name.
    #[serde(default)]
    pub author: String,
    /// Total downloads.
    #[serde(default)]
    pub downloads: u64,
    /// Icon URL (not fetched by the app; shown only if the UI chooses to).
    #[serde(default)]
    pub icon_url: Option<String>,
    /// Categories (include loaders).
    #[serde(default)]
    pub categories: Vec<String>,
    /// Environment values (new model; may be empty on old data).
    #[serde(default)]
    pub environment: Vec<String>,
    /// Legacy client/server side flags.
    #[serde(default)]
    pub server_side: Option<String>,
    /// License ID.
    #[serde(default)]
    pub license: Option<String>,
}

/// Search response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    /// Results.
    pub hits: Vec<Hit>,
    /// Total matches.
    pub total_hits: u64,
    /// Offset used.
    pub offset: u64,
}

/// Dependency of a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    /// Specific version, if pinned.
    #[serde(default)]
    pub version_id: Option<String>,
    /// Project, if known.
    #[serde(default)]
    pub project_id: Option<String>,
    /// `required`, `optional`, `incompatible`, `embedded`.
    pub dependency_type: String,
}

/// File hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hashes {
    /// SHA-512 hex.
    pub sha512: String,
    /// SHA-1 hex.
    #[serde(default)]
    pub sha1: Option<String>,
}

/// A downloadable file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionFile {
    /// URL.
    pub url: Url,
    /// File name.
    pub filename: String,
    /// Primary file of the version.
    #[serde(default)]
    pub primary: bool,
    /// Size in bytes.
    pub size: u64,
    /// Hashes.
    pub hashes: Hashes,
}

/// A project version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// Version ID.
    pub id: String,
    /// Project ID.
    pub project_id: String,
    /// Display name.
    pub name: String,
    /// Version string.
    pub version_number: String,
    /// `release`, `beta`, `alpha`.
    pub version_type: String,
    /// Loaders.
    #[serde(default)]
    pub loaders: Vec<String>,
    /// Game versions.
    #[serde(default)]
    pub game_versions: Vec<String>,
    /// Environment (new model).
    #[serde(default)]
    pub environment: Option<String>,
    /// Dependencies.
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    /// Files.
    pub files: Vec<VersionFile>,
    /// Publication date (RFC 3339).
    #[serde(default)]
    pub date_published: String,
}

impl Version {
    /// The primary file (or the first one).
    #[must_use]
    pub fn primary_file(&self) -> Option<&VersionFile> {
        self.files
            .iter()
            .find(|f| f.primary)
            .or_else(|| self.files.first())
    }

    /// Download spec for the primary file (SHA-512 verified).
    ///
    /// # Errors
    /// `Malformed` if there is no usable file; `HostNotAllowed` for foreign hosts.
    pub fn download(&self) -> Result<DownloadSpec, DownloadError> {
        let f = self
            .primary_file()
            .ok_or_else(|| DownloadError::Malformed(format!("version {} has no files", self.id)))?;
        if f.hashes.sha512.len() != 128 {
            return Err(DownloadError::Malformed("file has no valid SHA-512".into()));
        }
        policy().check(&f.url)?;
        Ok(DownloadSpec {
            url: f.url.clone(),
            expected: Expected::Sha512(f.hashes.sha512.clone()),
            max_bytes: f.size.max(1),
            policy: policy(),
        })
    }
}

/// Whether an environment value means the file is useful on a dedicated server (ADR-008).
/// `None` = unknown (allowed with a warning by callers).
#[must_use]
pub fn server_installable(environment: &str) -> Option<bool> {
    match environment {
        "client_and_server"
        | "server_only"
        | "dedicated_server_only"
        | "client_or_server"
        | "client_or_server_prefers_both"
        | "server_only_client_optional"
        | "client_only_server_optional" => Some(true),
        "client_only" | "singleplayer_only" => Some(false),
        _ => None,
    }
}

fn check_id(id: &str) -> Result<&str, DownloadError> {
    if !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(id)
    } else {
        Err(DownloadError::Malformed("invalid project id".into()))
    }
}

/// What kind of content to search for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// Fabric mods.
    FabricMod,
    /// Paper/Bukkit plugins.
    PaperPlugin,
}

impl ContentKind {
    /// Loaders accepted on the instance.
    #[must_use]
    pub fn loaders(self) -> &'static [&'static str] {
        match self {
            Self::FabricMod => &["fabric"],
            Self::PaperPlugin => &["paper", "spigot", "bukkit"],
        }
    }

    fn facets(self, mc_version: &str) -> String {
        let (ty, loaders) = match self {
            Self::FabricMod => ("mod", vec!["categories:fabric"]),
            Self::PaperPlugin => (
                "plugin",
                vec!["categories:paper", "categories:spigot", "categories:bukkit"],
            ),
        };
        serde_json::json!([
            [format!("project_type:{ty}")],
            loaders,
            [format!("versions:{mc_version}")]
        ])
        .to_string()
    }
}

/// Searches Modrinth for content compatible with an instance.
///
/// # Errors
/// Network, policy or parse errors.
pub async fn search(
    dl: &Downloader,
    kind: ContentKind,
    mc_version: &str,
    query: &str,
    offset: u32,
) -> Result<SearchResult, DownloadError> {
    let mut url = Url::parse(&format!("{API}/search"))
        .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    let query: String = query
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect();
    url.query_pairs_mut()
        .append_pair("query", &query)
        .append_pair("facets", &kind.facets(mc_version))
        .append_pair("limit", "20")
        .append_pair("offset", &offset.min(10_000).to_string())
        .append_pair(
            "index",
            if query.is_empty() {
                "downloads"
            } else {
                "relevance"
            },
        );
    dl.get_json(&url, &policy(), 2 * 1024 * 1024).await
}

/// Lists versions of a project compatible with the given loaders and game version (newest first).
///
/// # Errors
/// Network, policy or parse errors.
pub async fn versions(
    dl: &Downloader,
    project: &str,
    loaders: &[&str],
    mc_version: &str,
) -> Result<Vec<Version>, DownloadError> {
    let mut url = Url::parse(&format!("{API}/project/{}/version", check_id(project)?))
        .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair(
            "loaders",
            &serde_json::to_string(loaders).unwrap_or_default(),
        )
        .append_pair(
            "game_versions",
            &serde_json::json!([mc_version]).to_string(),
        )
        .append_pair("include_changelog", "false");
    dl.get_json(&url, &policy(), 8 * 1024 * 1024).await
}

/// Fetches one version by ID.
///
/// # Errors
/// Network, policy or parse errors.
pub async fn version(dl: &Downloader, id: &str) -> Result<Version, DownloadError> {
    let url = Url::parse(&format!("{API}/version/{}", check_id(id)?))
        .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    dl.get_json(&url, &policy(), 2 * 1024 * 1024).await
}

/// Minimal project info.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// ID.
    pub id: String,
    /// Slug.
    pub slug: String,
    /// Title.
    pub title: String,
    /// Legacy server-side flag.
    #[serde(default)]
    pub server_side: Option<String>,
}

/// Fetches project info.
///
/// # Errors
/// Network, policy or parse errors.
pub async fn project(dl: &Downloader, id: &str) -> Result<Project, DownloadError> {
    let url = Url::parse(&format!("{API}/project/{}", check_id(id)?))
        .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    dl.get_json(&url, &policy(), 2 * 1024 * 1024).await
}

/// Searches Fabric modpacks (any Minecraft version at or above what the caller filters later).
///
/// # Errors
/// Network, policy or parse errors.
pub async fn search_modpacks(
    dl: &Downloader,
    query: &str,
    offset: u32,
) -> Result<SearchResult, DownloadError> {
    let mut url = Url::parse(&format!("{API}/search"))
        .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    let query: String = query
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect();
    url.query_pairs_mut()
        .append_pair("query", &query)
        .append_pair(
            "facets",
            r#"[["project_type:modpack"],["categories:fabric"]]"#,
        )
        .append_pair("limit", "20")
        .append_pair("offset", &offset.min(10_000).to_string())
        .append_pair(
            "index",
            if query.is_empty() {
                "downloads"
            } else {
                "relevance"
            },
        );
    dl.get_json(&url, &policy(), 2 * 1024 * 1024).await
}

/// Lists Fabric versions of a modpack project (newest first), any Minecraft version.
///
/// # Errors
/// Network, policy or parse errors.
pub async fn pack_versions(dl: &Downloader, project: &str) -> Result<Vec<Version>, DownloadError> {
    let mut url = Url::parse(&format!("{API}/project/{}/version", check_id(project)?))
        .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("loaders", r#"["fabric"]"#)
        .append_pair("include_changelog", "false");
    dl.get_json(&url, &policy(), 8 * 1024 * 1024).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/upstream/modrinth")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn parses_real_search_and_versions() {
        let r: SearchResult = serde_json::from_str(&fixture("search.json")).unwrap();
        assert_eq!(r.hits[0].slug, "lithium");
        assert!(!r.hits[0].environment.is_empty());
        let v: Vec<Version> =
            serde_json::from_str(&fixture("versions-fabric-api-26.3.json")).unwrap();
        let spec = v[0].download().unwrap();
        assert_eq!(spec.url.host_str(), Some("cdn.modrinth.com"));
        assert!(matches!(spec.expected, Expected::Sha512(ref h) if h.len() == 128));
        assert_eq!(
            v[0].environment.as_deref(),
            Some("client_or_server_prefers_both")
        );
    }

    #[test]
    fn environment_mapping() {
        assert_eq!(server_installable("server_only"), Some(true));
        assert_eq!(
            server_installable("client_or_server_prefers_both"),
            Some(true)
        );
        assert_eq!(server_installable("client_only"), Some(false));
        assert_eq!(server_installable("singleplayer_only"), Some(false));
        assert_eq!(server_installable("unknown"), None);
    }

    #[test]
    fn facets_and_ids() {
        assert!(
            ContentKind::FabricMod
                .facets("26.3")
                .contains("project_type:mod")
        );
        assert!(
            ContentKind::PaperPlugin
                .facets("26.3")
                .contains("categories:paper")
        );
        assert!(check_id("P7dR8mSH").is_ok());
        assert!(check_id("../x").is_err());
        let v: Vec<Version> = serde_json::from_str(
            &fixture("versions-fabric-api-26.3.json")
                .replace("https://cdn.modrinth.com", "https://evil.example"),
        )
        .unwrap();
        assert!(matches!(
            v[0].download(),
            Err(DownloadError::HostNotAllowed(_))
        ));
    }
}
