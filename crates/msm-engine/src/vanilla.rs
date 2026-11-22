//! Vanilla server installation (SRV-01..05 for the Vanilla adapter) and EULA acceptance.
//!
//! Install is staged: everything is assembled under `tmp/stage/<id>/` and only renamed into
//! `instances/<id>/` once complete, so a failed or cancelled install never leaves a half-built
//! instance (SRV-04). The EULA is **never** written here; see [`accept_eula`] (SRV-05).

use std::path::{Path, PathBuf};

use msm_core::ids::InstanceId;
use msm_core::mc::properties::PropertiesDoc;
use msm_fsguard::{RelPath, Root, swap_dir, write_atomic};
use msm_providers::http::{Assurance, DownloadError, Downloader, Progress};
use msm_providers::mojang::{MojangClient, VersionKind};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::process::{DEFAULT_READY_PATTERN, LaunchTarget};

/// Oldest Minecraft version offered by default (Log4Shell-era versions excluded; ADR-005).
pub const MIN_SUPPORTED_VERSION: &str = "1.18.2";
/// Manifest format version.
pub const MANIFEST_FORMAT: u32 = 1;

/// Where a downloaded file came from and how it was verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// What it is, e.g. `server-jar`.
    pub artifact: String,
    /// Final URL after redirects.
    pub url: String,
    /// Integrity level.
    pub assurance: Assurance,
    /// SHA-256 of what we stored.
    pub sha256: String,
    /// When it was fetched (Unix ms).
    pub fetched_at_ms: i64,
}

/// `instances/<id>/instance.toml` — everything needed to run the instance even if the
/// database is lost (ADR-006).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceManifest {
    /// Manifest format.
    pub format: u32,
    /// Instance ID.
    pub id: InstanceId,
    /// Display name.
    pub name: String,
    /// `vanilla`, `paper`, …
    pub software: String,
    /// Minecraft version.
    pub mc_version: String,
    /// Loader/build details (e.g. `paper build 133`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Required Java major.
    pub java_major: u32,
    /// Server port.
    pub port: u16,
    /// Max heap in MiB.
    pub memory_mb: u32,
    /// Main jar or arg files, relative to `server/`.
    pub target: LaunchTarget,
    /// Extra server arguments.
    pub server_args: Vec<String>,
    /// Ready-line regex.
    pub ready_pattern: String,
    /// Download provenance.
    pub provenance: Vec<Provenance>,
}

impl InstanceManifest {
    /// Reads `instance.toml` from an instance directory.
    ///
    /// # Errors
    /// I/O or parse errors.
    pub fn load(instance_dir: &Path) -> Result<Self, InstallError> {
        let text = std::fs::read_to_string(instance_dir.join("instance.toml"))?;
        toml::from_str(&text).map_err(|e| InstallError::Manifest(e.to_string()))
    }

    /// JVM arguments derived from settings.
    #[must_use]
    pub fn jvm_args(&self) -> Vec<String> {
        vec![
            format!("-Xms{}M", self.memory_mb.min(1024)),
            format!("-Xmx{}M", self.memory_mb),
        ]
    }
}

/// Which server software to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Software {
    /// Official Mojang server.
    #[default]
    Vanilla,
    /// PaperMC (performance, Bukkit/Paper plugins).
    Paper,
    /// Fabric loader (Fabric mods).
    Fabric,
}

impl Software {
    /// Stable string used in manifests and the API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vanilla => "vanilla",
            Self::Paper => "paper",
            Self::Fabric => "fabric",
        }
    }
}

/// What the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VanillaRequest {
    /// Server software.
    pub software: Software,
    /// Display name.
    pub name: String,
    /// Minecraft version ID (`26.3`, `1.20.1`, …).
    pub mc_version: String,
    /// Port.
    pub port: u16,
    /// Max heap in MiB.
    pub memory_mb: u32,
    /// Allow versions older than [`MIN_SUPPORTED_VERSION`] or non-release versions (advanced).
    pub allow_unsupported: bool,
    /// Exact Fabric loader version (modpacks pin one); newest stable when `None`.
    pub fabric_loader: Option<String>,
}

/// Install failure. The staging directory is always cleaned up.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// Version not found or not offered.
    #[error("version {0} is not available")]
    UnknownVersion(String),
    /// Version exists but is outside the supported range.
    #[error("version {version} is not supported ({reason})")]
    Unsupported {
        /// Version.
        version: String,
        /// Why.
        reason: String,
    },
    /// Invalid request field.
    #[error("invalid request: {0}")]
    Invalid(String),
    /// Upstream/download error.
    #[error(transparent)]
    Download(#[from] DownloadError),
    /// Manifest could not be read or written.
    #[error("instance manifest error: {0}")]
    Manifest(String),
    /// Filesystem error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Any other classified error (e.g. not enough disk space).
    #[error(transparent)]
    Other(#[from] msm_core::MsmError),
}

impl From<InstallError> for msm_core::MsmError {
    fn from(e: InstallError) -> Self {
        use msm_core::ErrorCode as C;
        match e {
            InstallError::Download(d) => d.into(),
            InstallError::Other(e) => e,
            InstallError::UnknownVersion(_) => msm_core::MsmError::new(C::NotFound, e.to_string()),
            InstallError::Unsupported { .. } | InstallError::Invalid(_) => {
                msm_core::MsmError::new(C::InvalidArgument, e.to_string())
            }
            InstallError::Manifest(_) | InstallError::Io(_) => {
                msm_core::MsmError::new(C::Internal, e.to_string())
            }
        }
    }
}

/// Directories the installer works in.
#[derive(Debug, Clone)]
pub struct InstallDirs {
    /// `tmp/stage` (same volume as `instances`).
    pub stage_root: PathBuf,
    /// `instances`.
    pub instances_root: PathBuf,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn validate(req: &VanillaRequest) -> Result<(), InstallError> {
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(InstallError::Invalid(
            "name must be 1-64 printable characters".into(),
        ));
    }
    if req.port < 1024 {
        return Err(InstallError::Invalid(
            "port must be between 1024 and 65535".into(),
        ));
    }
    if !(512..=262_144).contains(&req.memory_mb) {
        return Err(InstallError::Invalid(
            "memory must be between 512 MiB and 256 GiB".into(),
        ));
    }
    Ok(())
}

/// Installs a Vanilla server and returns its manifest. The instance appears atomically at
/// `instances/<id>/` only when everything succeeded.
///
/// # Errors
/// See [`InstallError`]. Nothing is left behind on failure or cancellation.
pub async fn install(
    downloader: &Downloader,
    mojang: &MojangClient,
    dirs: &InstallDirs,
    req: &VanillaRequest,
    progress: Progress<'_>,
    cancel: &CancellationToken,
) -> Result<InstanceManifest, InstallError> {
    validate(req)?;
    // Server jars, libraries and a first world need a few hundred MB.
    tokio::fs::create_dir_all(&dirs.stage_root).await?;
    crate::backups::ensure_space(&dirs.stage_root, 400 * 1024 * 1024)?;
    let manifest = mojang.manifest().await?;
    let entry = manifest
        .get(&req.mc_version)
        .ok_or_else(|| InstallError::UnknownVersion(req.mc_version.clone()))?;
    if !req.allow_unsupported {
        if entry.kind != VersionKind::Release {
            return Err(InstallError::Unsupported {
                version: req.mc_version.clone(),
                reason: "only full releases are offered by default".into(),
            });
        }
        if manifest.is_at_least(&req.mc_version, MIN_SUPPORTED_VERSION) != Some(true) {
            return Err(InstallError::Unsupported {
                version: req.mc_version.clone(),
                reason: format!("versions older than {MIN_SUPPORTED_VERSION} are not supported"),
            });
        }
    }
    // Mojang's version metadata gives the Java requirement for every software type.
    let details = mojang.version(entry).await?;

    // Collect (blob, file name in server/, provenance label), the launch jar and extra files.
    let mut files: Vec<(msm_providers::http::Blob, &'static str, &'static str)> = Vec::new();
    let mut extra: Vec<(&'static str, String)> = Vec::new();
    let (target, build) = match req.software {
        Software::Vanilla => {
            let blob = downloader
                .fetch(&details.server_download()?, progress, cancel)
                .await?;
            files.push((blob, "server.jar", "server-jar"));
            ("server.jar", None)
        }
        Software::Paper => {
            let paper = msm_providers::paper::latest_stable(downloader, &req.mc_version).await?;
            let blob = downloader.fetch(&paper.download, progress, cancel).await?;
            files.push((blob, "paper.jar", "paper-jar"));
            ("paper.jar", Some(format!("paper build {}", paper.build)))
        }
        Software::Fabric => {
            // Vanilla jar from Mojang (SHA-1 verified) placed where the launcher expects it, so
            // the launcher never downloads it itself.
            let vanilla = downloader
                .fetch(&details.server_download()?, progress, cancel)
                .await?;
            files.push((vanilla, "server.jar", "server-jar"));
            let fabric = msm_providers::fabric::resolve_with(
                downloader,
                &req.mc_version,
                req.fabric_loader.as_deref(),
            )
            .await?;
            let launcher = downloader.fetch(&fabric.launcher, progress, cancel).await?;
            files.push((launcher, "fabric-server-launch.jar", "fabric-launcher"));
            extra.push((
                "fabric-server-launcher.properties",
                "serverJar=server.jar\n".into(),
            ));
            (
                "fabric-server-launch.jar",
                Some(format!(
                    "fabric loader {} (installer {})",
                    fabric.loader, fabric.installer
                )),
            )
        }
    };

    let id = InstanceId::new();
    let stage = dirs.stage_root.join(id.to_string());
    let plan = Assembly {
        files,
        extra,
        target,
        build,
    };
    let result = assemble(&stage, id, req, details.java_major(), plan, dirs).await;
    if result.is_err() {
        let _ = tokio::fs::remove_dir_all(&stage).await;
    }
    result
}

struct Assembly {
    files: Vec<(msm_providers::http::Blob, &'static str, &'static str)>,
    extra: Vec<(&'static str, String)>,
    target: &'static str,
    build: Option<String>,
}

async fn assemble(
    stage: &Path,
    id: InstanceId,
    req: &VanillaRequest,
    java_major: u32,
    plan: Assembly,
    dirs: &InstallDirs,
) -> Result<InstanceManifest, InstallError> {
    let server_dir = stage.join("server");
    tokio::fs::create_dir_all(&server_dir).await?;
    let mut provenance = Vec::new();
    for (blob, name, artifact) in &plan.files {
        tokio::fs::copy(&blob.path, server_dir.join(name)).await?;
        provenance.push(Provenance {
            artifact: (*artifact).into(),
            url: blob.final_url.to_string(),
            assurance: blob.assurance,
            sha256: blob.sha256.clone(),
            fetched_at_ms: now_ms(),
        });
    }
    for (name, body) in &plan.extra {
        write_atomic(&server_dir.join(name), body.as_bytes())?;
    }

    // Pre-seed only the port; Minecraft fills in every other default on first start and keeps
    // existing values.
    let mut props = PropertiesDoc::default();
    props.set("server-port", &req.port.to_string());
    write_atomic(
        &server_dir.join("server.properties"),
        props.render().as_bytes(),
    )?;

    let manifest = InstanceManifest {
        format: MANIFEST_FORMAT,
        id,
        name: req.name.trim().to_owned(),
        software: req.software.as_str().into(),
        mc_version: req.mc_version.clone(),
        build: plan.build,
        java_major,
        port: req.port,
        memory_mb: req.memory_mb,
        target: LaunchTarget::Jar {
            path: plan.target.into(),
        },
        server_args: vec!["nogui".into()],
        ready_pattern: DEFAULT_READY_PATTERN.into(),
        provenance,
    };
    let text =
        toml::to_string_pretty(&manifest).map_err(|e| InstallError::Manifest(e.to_string()))?;
    write_atomic(&stage.join("instance.toml"), text.as_bytes())?;

    tokio::fs::create_dir_all(&dirs.instances_root).await?;
    let target = dirs.instances_root.join(id.to_string());
    let previous = dirs.instances_root.join(format!(".{id}.previous"));
    swap_dir(stage, &target, &previous)?;
    Ok(manifest)
}

/// Writes `eula=true` for an instance. Must only be called after the user explicitly accepted
/// the Minecraft EULA in the UI; the caller records who accepted and when in the audit trail.
///
/// # Errors
/// Path or I/O errors.
pub fn accept_eula(instance_dir: &Path) -> Result<(), msm_core::MsmError> {
    let root = Root::open(instance_dir.join("server"))?;
    let rel = RelPath::parse("eula.txt")?;
    let body = "# Accepted by the server owner in MC Server Center.\n\
                # https://aka.ms/MinecraftEULA\n\
                eula=true\n";
    root.write_atomic(&rel, body.as_bytes())?;
    Ok(())
}

/// Whether the EULA has been accepted for an instance (reads `server/eula.txt`).
#[must_use]
pub fn eula_accepted(instance_dir: &Path) -> bool {
    std::fs::read_to_string(instance_dir.join("server").join("eula.txt"))
        .map(|t| {
            PropertiesDoc::parse(&t)
                .get("eula")
                .is_some_and(|v| v.trim() == "true")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_validation() {
        let ok = VanillaRequest {
            software: Software::Vanilla,
            name: "Friends".into(),
            mc_version: "26.3".into(),
            port: 25565,
            memory_mb: 2048,
            allow_unsupported: false,
            fabric_loader: None,
        };
        assert!(validate(&ok).is_ok());
        assert!(
            validate(&VanillaRequest {
                name: " ".into(),
                ..ok.clone()
            })
            .is_err()
        );
        assert!(
            validate(&VanillaRequest {
                name: "a\nb".into(),
                ..ok.clone()
            })
            .is_err()
        );
        assert!(
            validate(&VanillaRequest {
                port: 80,
                ..ok.clone()
            })
            .is_err()
        );
        assert!(
            validate(&VanillaRequest {
                memory_mb: 100,
                ..ok
            })
            .is_err()
        );
    }

    #[test]
    fn eula_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("server")).unwrap();
        assert!(!eula_accepted(dir.path()));
        std::fs::write(dir.path().join("server/eula.txt"), "eula=false\n").unwrap();
        assert!(!eula_accepted(dir.path()));
        accept_eula(dir.path()).unwrap();
        assert!(eula_accepted(dir.path()));
    }

    #[test]
    fn manifest_round_trips_through_toml() {
        let m = InstanceManifest {
            format: 1,
            id: InstanceId::new(),
            name: "n".into(),
            software: "vanilla".into(),
            mc_version: "26.3".into(),
            build: None,
            java_major: 25,
            port: 25565,
            memory_mb: 2048,
            target: LaunchTarget::Jar {
                path: "server.jar".into(),
            },
            server_args: vec!["nogui".into()],
            ready_pattern: DEFAULT_READY_PATTERN.into(),
            provenance: vec![Provenance {
                artifact: "server-jar".into(),
                url: "https://piston-data.mojang.com/x".into(),
                assurance: Assurance::UpstreamSha1,
                sha256: "ab".into(),
                fetched_at_ms: 1,
            }],
        };
        let text = toml::to_string_pretty(&m).unwrap();
        assert_eq!(toml::from_str::<InstanceManifest>(&text).unwrap(), m);
        assert_eq!(m.jvm_args(), ["-Xms1024M", "-Xmx2048M"]);
    }
}
