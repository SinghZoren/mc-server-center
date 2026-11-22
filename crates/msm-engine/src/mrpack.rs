//! Modrinth modpacks (`.mrpack`, PACK-01..04, ADR-008).
//!
//! Validation happens **before anything is written**: index format, game, dependencies (only
//! Minecraft + Fabric are supported today), every file path (must be a safe relative path),
//! every download URL (HTTPS on an allowlisted host), mandatory SHA-512, sizes, and server
//! environment (`env.server == "unsupported"` files are skipped). Overrides are extracted with
//! the safe extractor (links rejected), `overrides/` first, then `server-overrides/` on top.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use msm_core::{ErrorCode, MsmError};
use msm_fsguard::RelPath;
use msm_fsguard::archive::{self, Limits, LinkPolicy};
use msm_providers::http::{DownloadSpec, Expected, HostPolicy};
use serde::{Deserialize, Serialize};
use url::Url;

/// Index file name inside the archive.
pub const INDEX: &str = "modrinth.index.json";
/// Pack lock written into the server folder (PACK-03).
pub const LOCK_FILE: &str = ".msm-modpack.json";
/// Largest single file we accept from a pack.
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// Hosts pack files may be downloaded from (Modrinth's own whitelist plus GitHub's asset CDN
/// that github.com release links redirect to).
#[must_use]
pub fn download_policy() -> HostPolicy {
    HostPolicy::new(&[
        "cdn.modrinth.com",
        "github.com",
        "raw.githubusercontent.com",
        "gitlab.com",
        "objects.githubusercontent.com",
        "release-assets.githubusercontent.com",
    ])
}

/// Per-side requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Env {
    /// Client side.
    #[serde(default)]
    pub client: Option<String>,
    /// Server side.
    #[serde(default)]
    pub server: Option<String>,
}

/// File hashes from the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHashes {
    /// SHA-1 (required by the spec, informational here).
    #[serde(default)]
    pub sha1: Option<String>,
    /// SHA-512 (required; used for verification).
    #[serde(default)]
    pub sha512: Option<String>,
}

/// A file entry in the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackFile {
    /// Destination path.
    pub path: String,
    /// Hashes.
    pub hashes: FileHashes,
    /// Environment.
    #[serde(default)]
    pub env: Option<Env>,
    /// Download mirrors.
    pub downloads: Vec<String>,
    /// Size in bytes.
    pub file_size: u64,
}

/// `modrinth.index.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackIndex {
    /// Must be 1.
    pub format_version: u32,
    /// Must be `minecraft`.
    pub game: String,
    /// Pack version.
    pub version_id: String,
    /// Pack name.
    pub name: String,
    /// Summary.
    #[serde(default)]
    pub summary: Option<String>,
    /// Files to download.
    pub files: Vec<PackFile>,
    /// Required game/loader versions.
    pub dependencies: BTreeMap<String, String>,
}

/// A file to fetch into the server folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// Destination relative to `server/`.
    pub path: RelPath,
    /// Verified download.
    pub download: DownloadSpec,
}

/// Validated, installable plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackPlan {
    /// Pack name.
    pub name: String,
    /// Pack version.
    pub version_id: String,
    /// Minecraft version.
    pub mc_version: String,
    /// Fabric loader version.
    pub fabric_loader: String,
    /// Files to download.
    pub files: Vec<PlannedFile>,
    /// Client-only files skipped.
    pub skipped_client_only: Vec<String>,
    /// Total download size.
    pub total_bytes: u64,
}

/// Reads the index from an `.mrpack` (size-capped).
///
/// # Errors
/// `UpstreamError` if the archive or index is missing or malformed.
pub fn read_index(mrpack: &Path) -> Result<PackIndex, MsmError> {
    let bad = |m: String| MsmError::new(ErrorCode::UpstreamError, m);
    let mut zip =
        zip::ZipArchive::new(std::fs::File::open(mrpack).map_err(|e| bad(e.to_string()))?)
            .map_err(|e| bad(format!("not a valid modpack archive: {e}")))?;
    let entry = zip
        .by_name(INDEX)
        .map_err(|_| bad("the modpack has no modrinth.index.json".into()))?;
    if entry.size() > 32 * 1024 * 1024 {
        return Err(bad("the modpack index is unreasonably large".into()));
    }
    let mut text = String::new();
    entry
        .take(32 * 1024 * 1024)
        .read_to_string(&mut text)
        .map_err(|e| bad(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| bad(format!("the modpack index is invalid: {e}")))
}

/// Checks everything and produces a plan, or the list of reasons the pack can't be installed
/// (PACK-04). Nothing is downloaded or written here.
///
/// # Errors
/// A list of human-readable limitations.
pub fn plan(index: &PackIndex) -> Result<PackPlan, Vec<String>> {
    let mut problems = Vec::new();
    if index.format_version != 1 {
        problems.push(format!(
            "pack format {} is not supported (only version 1)",
            index.format_version
        ));
    }
    if index.game != "minecraft" {
        problems.push(format!("this pack is for {:?}, not Minecraft", index.game));
    }
    let mc = index.dependencies.get("minecraft").cloned();
    if mc.is_none() {
        problems.push("the pack doesn't say which Minecraft version it needs".into());
    }
    let mut loader = None;
    for (k, v) in &index.dependencies {
        match k.as_str() {
            "minecraft" => {}
            "fabric-loader" => loader = Some(v.clone()),
            "forge" | "neoforge" => problems.push(format!("this pack needs {k} {v}; Forge/NeoForge packs aren't supported yet (Fabric packs are)")),
            "quilt-loader" => problems.push("this pack needs Quilt, which isn't supported".into()),
            other => problems.push(format!("this pack needs {other} {v}, which isn't supported")),
        }
    }
    if loader.is_none() && !problems.iter().any(|p| p.contains("needs")) {
        problems.push("the pack doesn't use a mod loader this app supports (Fabric)".into());
    }

    let policy = download_policy();
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut total = 0u64;
    let mut seen = std::collections::HashSet::new();
    for f in &index.files {
        let path = match RelPath::parse(&f.path) {
            Ok(p) => p,
            Err(e) => {
                problems.push(format!("unsafe file path {:?} ({e})", f.path));
                continue;
            }
        };
        if !seen.insert(path.as_str().to_lowercase()) {
            problems.push(format!("the file {} is listed twice", f.path));
            continue;
        }
        if f.env.as_ref().and_then(|e| e.server.as_deref()) == Some("unsupported") {
            skipped.push(f.path.clone());
            continue;
        }
        let Some(sha512) = f
            .hashes
            .sha512
            .clone()
            .filter(|h| h.len() == 128 && h.chars().all(|c| c.is_ascii_hexdigit()))
        else {
            problems.push(format!("{} has no valid SHA-512 hash", f.path));
            continue;
        };
        if f.file_size > MAX_FILE_BYTES {
            problems.push(format!(
                "{} is too large ({} MB)",
                f.path,
                f.file_size / (1024 * 1024)
            ));
            continue;
        }
        let url = f
            .downloads
            .iter()
            .filter_map(|d| Url::parse(d).ok())
            .find(|u| policy.check(u).is_ok());
        let Some(url) = url else {
            problems.push(format!(
                "{} can only be downloaded from a site this app doesn't trust ({})",
                f.path,
                f.downloads.join(", ")
            ));
            continue;
        };
        total += f.file_size;
        files.push(PlannedFile {
            path,
            download: DownloadSpec {
                url,
                expected: Expected::Sha512(sha512),
                max_bytes: f.file_size.max(1),
                policy: policy.clone(),
            },
        });
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(PackPlan {
        name: index.name.clone(),
        version_id: index.version_id.clone(),
        mc_version: mc.unwrap_or_default(),
        fabric_loader: loader.unwrap_or_default(),
        files,
        skipped_client_only: skipped,
        total_bytes: total,
    })
}

/// Copies `overrides/` then `server-overrides/` from the pack into `server_dir`, after a safe
/// extraction into `scratch` (an empty directory).
///
/// # Errors
/// Extraction (unsafe paths, links, limits) or I/O errors.
pub fn apply_overrides(
    mrpack: &Path,
    scratch: &Path,
    server_dir: &Path,
) -> Result<usize, MsmError> {
    archive::extract_zip(
        mrpack,
        scratch,
        Limits {
            max_total_bytes: 2 * 1024 * 1024 * 1024,
            ..Limits::default()
        },
        LinkPolicy::Reject,
    )?;
    let mut copied = 0;
    for layer in ["overrides", "server-overrides"] {
        let root = scratch.join(layer);
        if root.is_dir() {
            copied += copy_tree(&root, server_dir)?;
        }
    }
    Ok(copied)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<usize, MsmError> {
    let io = |e: std::io::Error| MsmError::new(ErrorCode::Internal, e.to_string());
    std::fs::create_dir_all(dst).map_err(io)?;
    let mut n = 0;
    for e in std::fs::read_dir(src).map_err(io)?.flatten() {
        let meta = std::fs::symlink_metadata(e.path()).map_err(io)?;
        let target = dst.join(e.file_name());
        if meta.is_dir() {
            n += copy_tree(&e.path(), &target)?;
        } else if meta.is_file() {
            std::fs::copy(e.path(), &target).map_err(io)?;
            n += 1;
        }
    }
    Ok(n)
}

/// What the lock file records (PACK-03).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackLock {
    /// Modrinth project ID of the pack.
    pub project_id: String,
    /// Modrinth version ID of the pack.
    pub version_id: String,
    /// Pack name.
    pub name: String,
    /// Pack version string.
    pub pack_version: String,
    /// Minecraft version.
    pub mc_version: String,
    /// Fabric loader.
    pub fabric_loader: String,
    /// SHA-512 of the `.mrpack`.
    pub mrpack_sha512: String,
    /// Installed files and their hashes.
    pub files: Vec<(String, String)>,
    /// Skipped client-only files.
    pub skipped_client_only: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(files: &str, deps: &str) -> PackIndex {
        serde_json::from_str(&format!(
            r#"{{"formatVersion":1,"game":"minecraft","versionId":"1.0","name":"Test Pack","files":[{files}],"dependencies":{deps}}}"#
        ))
        .unwrap()
    }

    fn file(path: &str, url: &str, server: &str) -> String {
        format!(
            r#"{{"path":"{path}","hashes":{{"sha1":"{}","sha512":"{}"}},"env":{{"client":"required","server":"{server}"}},"downloads":["{url}"],"fileSize":100}}"#,
            "a".repeat(40),
            "b".repeat(128)
        )
    }

    const FABRIC: &str = r#"{"minecraft":"26.3","fabric-loader":"0.20.1"}"#;

    #[test]
    fn plans_a_valid_fabric_pack() {
        let files = [
            file(
                "mods/a.jar",
                "https://cdn.modrinth.com/data/x/a.jar",
                "required",
            ),
            file(
                "mods/shaders.jar",
                "https://cdn.modrinth.com/data/x/s.jar",
                "unsupported",
            ),
        ]
        .join(",");
        let p = plan(&index(&files, FABRIC)).unwrap();
        assert_eq!(p.mc_version, "26.3");
        assert_eq!(p.fabric_loader, "0.20.1");
        assert_eq!(p.files.len(), 1);
        assert_eq!(p.skipped_client_only, ["mods/shaders.jar"]);
    }

    #[test]
    fn rejects_unsafe_and_unsupported_packs_with_reasons() {
        let cases = [
            (
                file(
                    "../../evil.jar",
                    "https://cdn.modrinth.com/x.jar",
                    "required",
                ),
                FABRIC,
                "unsafe file path",
            ),
            (
                file("C:/evil.jar", "https://cdn.modrinth.com/x.jar", "required"),
                FABRIC,
                "unsafe file path",
            ),
            (
                file("mods/a.jar", "https://evil.example/a.jar", "required"),
                FABRIC,
                "doesn't trust",
            ),
            (
                file("mods/a.jar", "http://cdn.modrinth.com/a.jar", "required"),
                FABRIC,
                "doesn't trust",
            ),
            (
                file("mods/a.jar", "https://cdn.modrinth.com/a.jar", "required"),
                r#"{"minecraft":"1.20.1","forge":"47.4.28"}"#,
                "Forge/NeoForge",
            ),
            (
                file("mods/a.jar", "https://cdn.modrinth.com/a.jar", "required"),
                r#"{"minecraft":"1.20.1","quilt-loader":"0.1"}"#,
                "Quilt",
            ),
        ];
        for (f, deps, needle) in cases {
            let problems = plan(&index(&f, deps)).unwrap_err();
            assert!(
                problems.iter().any(|p| p.contains(needle)),
                "{needle}: {problems:?}"
            );
        }
        let dup = [
            file("mods/A.jar", "https://cdn.modrinth.com/a.jar", "required"),
            file("mods/a.jar", "https://cdn.modrinth.com/a.jar", "required"),
        ]
        .join(",");
        assert!(
            plan(&index(&dup, FABRIC))
                .unwrap_err()
                .iter()
                .any(|p| p.contains("twice"))
        );
        let nohash = r#"{"path":"mods/a.jar","hashes":{"sha1":"x"},"downloads":["https://cdn.modrinth.com/a.jar"],"fileSize":1}"#;
        assert!(
            plan(&index(nohash, FABRIC))
                .unwrap_err()
                .iter()
                .any(|p| p.contains("SHA-512"))
        );
    }

    #[test]
    fn overrides_are_layered_and_safely_extracted() {
        use std::io::Write;
        let d = tempfile::tempdir().unwrap();
        let pack = d.path().join("p.mrpack");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&pack).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        for (name, body) in [
            (INDEX, "{}"),
            ("overrides/config/a.toml", "from overrides"),
            ("overrides/config/b.toml", "only overrides"),
            ("server-overrides/config/a.toml", "from server-overrides"),
            ("client-overrides/options.txt", "client"),
        ] {
            z.start_file(name, o).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap();
        let scratch = d.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        let server = d.path().join("server");
        let n = apply_overrides(&pack, &scratch, &server).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            std::fs::read_to_string(server.join("config/a.toml")).unwrap(),
            "from server-overrides"
        );
        assert_eq!(
            std::fs::read_to_string(server.join("config/b.toml")).unwrap(),
            "only overrides"
        );
        assert!(
            !server.join("options.txt").exists(),
            "client overrides ignored"
        );
    }
}
