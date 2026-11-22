//! Fabric via meta.fabricmc.net (ADR-005). Fabric publishes **no hashes**, so the server
//! launcher is trust-on-first-use: TLS to an allowlisted host, and we record its SHA-256 in the
//! instance provenance. The vanilla server jar it wraps is fetched from Mojang separately (SHA-1
//! verified) and placed next to it, so the launcher does not download it itself.
//! Shapes verified 2026-11-21 (`fixtures/upstream/fabric/`).

use serde::Deserialize;
use url::Url;

use crate::http::{DownloadError, DownloadSpec, Downloader, Expected, HostPolicy};

const META: &str = "https://meta.fabricmc.net/v2";

/// Hosts for Fabric metadata, launcher and libraries.
#[must_use]
pub fn policy() -> HostPolicy {
    HostPolicy::new(&["meta.fabricmc.net", "maven.fabricmc.net"])
}

#[derive(Debug, Clone, Deserialize)]
struct GameVersion {
    version: String,
    stable: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct LoaderEntry {
    loader: Component,
}

#[derive(Debug, Clone, Deserialize)]
struct Component {
    version: String,
    stable: bool,
}

/// Resolved Fabric components for a game version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricBuild {
    /// Minecraft version.
    pub mc_version: String,
    /// Loader version.
    pub loader: String,
    /// Installer version used to produce the launcher.
    pub installer: String,
    /// Server launcher download (no upstream hash).
    pub launcher: DownloadSpec,
}

fn safe(v: &str) -> Result<&str, DownloadError> {
    if !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
    {
        Ok(v)
    } else {
        Err(DownloadError::Malformed(format!(
            "unexpected version string {v:?}"
        )))
    }
}

fn first_stable(
    list: impl IntoIterator<Item = Component>,
    what: &str,
) -> Result<String, DownloadError> {
    list.into_iter()
        .find(|c| c.stable)
        .map(|c| c.version)
        .ok_or_else(|| DownloadError::Malformed(format!("no stable Fabric {what}")))
}

fn launcher_spec(mc: &str, loader: &str, installer: &str) -> Result<DownloadSpec, DownloadError> {
    let url = Url::parse(&format!(
        "{META}/versions/loader/{}/{}/{}/server/jar",
        safe(mc)?,
        safe(loader)?,
        safe(installer)?
    ))
    .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    Ok(DownloadSpec {
        url,
        expected: Expected::None,
        max_bytes: 8 * 1024 * 1024,
        policy: policy(),
    })
}

/// Stable Minecraft versions Fabric supports.
///
/// # Errors
/// Network, policy or parse errors.
pub async fn versions(dl: &Downloader) -> Result<Vec<String>, DownloadError> {
    let url = Url::parse(&format!("{META}/versions/game"))
        .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    let games: Vec<GameVersion> = dl.get_json(&url, &policy(), 4 * 1024 * 1024).await?;
    Ok(games
        .into_iter()
        .filter(|g| g.stable)
        .map(|g| g.version)
        .collect())
}

/// Resolves the newest stable loader and installer for a Minecraft version.
///
/// # Errors
/// Network, policy or parse errors.
pub async fn resolve(dl: &Downloader, mc_version: &str) -> Result<FabricBuild, DownloadError> {
    resolve_with(dl, mc_version, None).await
}

/// Like [`resolve`], but uses a specific loader version when given (modpacks pin one). The
/// pinned version must be one Fabric lists for this Minecraft version.
///
/// # Errors
/// Network, policy or parse errors; `Malformed` if the pinned loader is not available.
pub async fn resolve_with(
    dl: &Downloader,
    mc_version: &str,
    pinned_loader: Option<&str>,
) -> Result<FabricBuild, DownloadError> {
    let mc = safe(mc_version)?;
    let loaders_url = Url::parse(&format!("{META}/versions/loader/{mc}"))
        .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    let loaders: Vec<LoaderEntry> = dl
        .get_json(&loaders_url, &policy(), 8 * 1024 * 1024)
        .await?;
    let loader = match pinned_loader {
        Some(want) => {
            let want = safe(want)?;
            loaders
                .iter()
                .find(|l| l.loader.version == want)
                .map(|l| l.loader.version.clone())
                .ok_or_else(|| {
                    DownloadError::Malformed(format!(
                        "Fabric loader {want} is not available for {mc}"
                    ))
                })?
        }
        None => first_stable(loaders.into_iter().map(|l| l.loader), "loader")?,
    };
    let inst_url = Url::parse(&format!("{META}/versions/installer"))
        .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    let installers: Vec<Component> = dl.get_json(&inst_url, &policy(), 1024 * 1024).await?;
    let installer = first_stable(installers, "installer")?;
    let launcher = launcher_spec(mc, &loader, &installer)?;
    Ok(FabricBuild {
        mc_version: mc.to_owned(),
        loader,
        installer,
        launcher,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/upstream/fabric")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn parses_real_fixtures() {
        let games: Vec<GameVersion> = serde_json::from_str(&fixture("game.json")).unwrap();
        assert!(games.iter().any(|g| g.stable && g.version == "26.3"));
        assert!(games.iter().any(|g| !g.stable));
        let loaders: Vec<LoaderEntry> = serde_json::from_str(&fixture("loader-26.3.json")).unwrap();
        assert_eq!(
            first_stable(loaders.into_iter().map(|l| l.loader), "loader").unwrap(),
            "0.20.1"
        );
        let installers: Vec<Component> = serde_json::from_str(&fixture("installer.json")).unwrap();
        assert_eq!(first_stable(installers, "installer").unwrap(), "1.1.2");
    }

    #[test]
    fn launcher_url_is_built_safely() {
        let spec = launcher_spec("26.3", "0.20.1", "1.1.2").unwrap();
        assert_eq!(
            spec.url.as_str(),
            "https://meta.fabricmc.net/v2/versions/loader/26.3/0.20.1/1.1.2/server/jar"
        );
        assert_eq!(spec.expected, Expected::None);
        assert!(launcher_spec("26.3/../../x", "0.20.1", "1.1.2").is_err());
        assert!(launcher_spec("26.3", "0.20.1?x=1", "1.1.2").is_err());
    }
}
