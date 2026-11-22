//! Eclipse Temurin JRE metadata from the Adoptium API (ADR-004).
//!
//! `GET https://api.adoptium.net/v3/assets/latest/{major}/hotspot?os&architecture&image_type=jre&vendor=eclipse`
//! returns `[{release_name, binary:{package:{name, link, checksum(sha256), size}}}]`; the link is a
//! GitHub release URL that redirects to `release-assets.githubusercontent.com`
//! (verified 2026-11-21).

use serde::Deserialize;
use url::Url;

use crate::http::{DownloadError, DownloadSpec, Downloader, Expected, HostPolicy};

/// Hosts used for Temurin metadata and binaries.
#[must_use]
pub fn policy() -> HostPolicy {
    HostPolicy::new(&[
        "api.adoptium.net",
        "github.com",
        "release-assets.githubusercontent.com",
        "objects.githubusercontent.com",
    ])
}

#[derive(Debug, Deserialize)]
struct Asset {
    release_name: String,
    binary: Binary,
}

#[derive(Debug, Deserialize)]
struct Binary {
    package: Package,
    image_type: String,
}

#[derive(Debug, Deserialize)]
struct Package {
    name: String,
    link: Url,
    checksum: String,
    size: u64,
}

/// Archive format of a JRE package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    /// `.zip` (Windows).
    Zip,
    /// `.tar.gz` (Linux).
    TarGz,
}

/// A downloadable JRE build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JreRelease {
    /// Java major version.
    pub major: u32,
    /// e.g. `jdk-25.0.5.1+2`.
    pub release_name: String,
    /// Package file name.
    pub file_name: String,
    /// Archive format.
    pub kind: ArchiveKind,
    /// Download spec (sha256-verified).
    pub download: DownloadSpec,
}

/// Adoptium OS name for this build.
#[must_use]
pub fn current_os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else {
        "linux"
    }
}

/// Adoptium architecture name for this build.
#[must_use]
pub fn current_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x64"
    }
}

fn parse(major: u32, assets: Vec<Asset>) -> Result<JreRelease, DownloadError> {
    let a = assets
        .into_iter()
        .find(|a| a.binary.image_type == "jre")
        .ok_or_else(|| {
            DownloadError::Malformed(format!("no Temurin JRE {major} for this platform"))
        })?;
    let p = a.binary.package;
    if p.checksum.len() != 64 || !p.checksum.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(DownloadError::Malformed(
            "Adoptium returned an invalid checksum".into(),
        ));
    }
    let kind = if p.name.ends_with(".zip") {
        ArchiveKind::Zip
    } else if p.name.ends_with(".tar.gz") {
        ArchiveKind::TarGz
    } else {
        return Err(DownloadError::Malformed(format!(
            "unexpected package {}",
            p.name
        )));
    };
    policy().check(&p.link)?;
    Ok(JreRelease {
        major,
        release_name: a.release_name,
        file_name: p.name,
        kind,
        download: DownloadSpec {
            url: p.link,
            expected: Expected::Sha256(p.checksum),
            max_bytes: p.size,
            policy: policy(),
        },
    })
}

/// Finds the latest Temurin JRE for a major version on this OS/architecture.
///
/// # Errors
/// Network, policy or parse errors; `Malformed` if no build exists.
pub async fn latest_jre(downloader: &Downloader, major: u32) -> Result<JreRelease, DownloadError> {
    let url = Url::parse(&format!(
        "https://api.adoptium.net/v3/assets/latest/{major}/hotspot?os={}&architecture={}&image_type=jre&vendor=eclipse",
        current_os(),
        current_arch()
    ))
    .map_err(|e| DownloadError::Malformed(e.to_string()))?;
    let assets: Vec<Asset> = downloader.get_json(&url, &policy(), 1024 * 1024).await?;
    parse(major, assets)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[{"binary":{"architecture":"x64","image_type":"jre","os":"windows","package":{"checksum":"3260a1b9899f30ac80d737daafc053042c525249f52d066101fbf81d78d73f25","link":"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5.1%2B2/OpenJDK25U-jre_x64_windows_hotspot_25.0.5.1_2.zip","name":"OpenJDK25U-jre_x64_windows_hotspot_25.0.5.1_2.zip","size":58599439}},"release_name":"jdk-25.0.5.1+2","vendor":"eclipse"}]"#;

    #[test]
    fn parses_real_response_shape() {
        let assets: Vec<Asset> = serde_json::from_str(SAMPLE).unwrap();
        let r = parse(25, assets).unwrap();
        assert_eq!(r.kind, ArchiveKind::Zip);
        assert_eq!(r.release_name, "jdk-25.0.5.1+2");
        assert!(matches!(r.download.expected, Expected::Sha256(ref h) if h.len() == 64));
        assert_eq!(r.download.max_bytes, 58_599_439);
    }

    #[test]
    fn rejects_bad_checksums_hosts_and_empty_results() {
        let bad_sum = SAMPLE.replace("3260a1b9", "zzzz");
        assert!(parse(25, serde_json::from_str(&bad_sum).unwrap()).is_err());
        let bad_host = SAMPLE.replace("https://github.com", "https://evil.example");
        assert!(matches!(
            parse(25, serde_json::from_str(&bad_host).unwrap()),
            Err(DownloadError::HostNotAllowed(_))
        ));
        assert!(parse(25, vec![]).is_err());
    }
}
