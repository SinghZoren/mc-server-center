//! Mods and plugins (MOD-01..03, ADR-008): dependency planning, install and removal.
//!
//! * Planning picks the newest **release** compatible with the server's loader and Minecraft
//!   version (falling back to beta, never alpha), follows `required` dependencies, lists
//!   `optional` ones, flags `incompatible` ones that are installed or planned, and refuses
//!   client-only content (MOD-02).
//! * Installed content is recorded in `server/.msm-content.json`, so the record travels with
//!   backups and restores.
//! * Files land in `mods/` (Fabric) or `plugins/` (Paper) via atomic writes; file names must be
//!   a single safe path component ending in `.jar`.

use std::collections::{BTreeSet, VecDeque};
use std::path::Path;

use msm_core::{ErrorCode, MsmError};
use msm_fsguard::{RelPath, write_atomic};
use msm_providers::http::{DownloadError, DownloadSpec};
use msm_providers::modrinth::{self, Version};
use serde::{Deserialize, Serialize};

/// Registry file inside `server/`.
pub const REGISTRY_FILE: &str = ".msm-content.json";
const MAX_PLAN_ITEMS: usize = 40;

/// Where version metadata comes from (Modrinth in production, fixtures in tests).
pub trait VersionSource {
    /// Compatible versions of a project, newest first.
    fn versions(
        &self,
        project: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Version>, DownloadError>> + Send;
    /// One version by ID.
    fn version(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<Version, DownloadError>> + Send;
    /// Human-readable project title.
    fn title(&self, project: &str) -> impl std::future::Future<Output = String> + Send;
}

/// One installed project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledItem {
    /// Modrinth project ID.
    pub project_id: String,
    /// Installed version ID.
    pub version_id: String,
    /// Version string.
    pub version_number: String,
    /// Title.
    pub title: String,
    /// File path relative to `server/`.
    pub file: String,
    /// SHA-512 of the file.
    pub sha512: String,
    /// Projects that required this one (empty if the user asked for it).
    #[serde(default)]
    pub required_by: Vec<String>,
}

/// The registry file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    /// Installed items.
    pub items: Vec<InstalledItem>,
}

impl Registry {
    /// Loads the registry (empty if missing or unreadable).
    #[must_use]
    pub fn load(server_dir: &Path) -> Self {
        std::fs::read(server_dir.join(REGISTRY_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self, server_dir: &Path) -> Result<(), MsmError> {
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
        write_atomic(&server_dir.join(REGISTRY_FILE), &json)
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))
    }

    fn has(&self, project: &str) -> bool {
        self.items.iter().any(|i| i.project_id == project)
    }
}

/// A file to install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanItem {
    /// Project ID.
    pub project_id: String,
    /// Title.
    pub title: String,
    /// Version ID.
    pub version_id: String,
    /// Version string.
    pub version_number: String,
    /// `release` or `beta`.
    pub version_type: String,
    /// File name.
    pub filename: String,
    /// Size in bytes.
    pub size: u64,
    /// Project that needs it (`None` = requested by the user).
    pub required_by: Option<String>,
    /// Environment value, if declared.
    pub environment: Option<String>,
    /// Verified download.
    #[serde(skip)]
    pub download: Option<DownloadSpec>,
    /// SHA-512.
    pub sha512: String,
}

/// The preview shown before installing (MOD-02).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// Files that will be added.
    pub install: Vec<PlanItem>,
    /// Required projects already installed.
    pub already_installed: Vec<String>,
    /// Optional dependencies the user may add.
    pub optional: Vec<String>,
    /// Problems that block installation.
    pub conflicts: Vec<String>,
    /// Notes (e.g. unknown environment).
    pub warnings: Vec<String>,
}

fn pick(versions: &[Version]) -> Option<&Version> {
    versions
        .iter()
        .find(|v| v.version_type == "release")
        .or_else(|| versions.iter().find(|v| v.version_type == "beta"))
}

/// Plans installing `project` and its required dependencies.
///
/// # Errors
/// Upstream errors, or `InvalidArgument` if the requested project has no compatible version or
/// is client-only.
pub async fn plan<S: VersionSource>(
    src: &S,
    project: &str,
    installed: &Registry,
) -> Result<Plan, MsmError> {
    let mut out = Plan::default();
    if installed.has(project) {
        out.already_installed.push(project.to_owned());
        return Ok(out);
    }
    let mut queue: VecDeque<(String, Option<String>)> =
        VecDeque::from([(project.to_owned(), None)]);
    let mut seen = BTreeSet::new();
    let mut incompatible: Vec<(String, String)> = Vec::new();
    while let Some((pid, parent)) = queue.pop_front() {
        if !seen.insert(pid.clone()) {
            continue;
        }
        if installed.has(&pid) {
            out.already_installed.push(pid);
            continue;
        }
        if out.install.len() >= MAX_PLAN_ITEMS {
            return Err(MsmError::new(
                ErrorCode::InvalidArgument,
                "too many dependencies to install at once",
            ));
        }
        let versions = src.versions(&pid).await?;
        let title = src.title(&pid).await;
        let Some(v) = pick(&versions) else {
            let msg =
                format!("{title} has no release for this server's Minecraft version and loader");
            if parent.is_none() {
                return Err(MsmError::new(ErrorCode::InvalidArgument, msg));
            }
            out.conflicts
                .push(format!("{msg} (needed by {})", parent.unwrap_or_default()));
            continue;
        };
        match v.environment.as_deref().map(modrinth::server_installable) {
            Some(Some(false)) => {
                let msg = format!(
                    "{title} only works on the player's computer (client-only), not on a server"
                );
                if parent.is_none() {
                    return Err(MsmError::new(ErrorCode::InvalidArgument, msg));
                }
                out.warnings.push(format!("{msg}; skipped"));
                continue;
            }
            Some(None) | None => out
                .warnings
                .push(format!("{title} doesn't say whether it works on servers")),
            Some(Some(true)) => {}
        }
        let file = v.primary_file().ok_or_else(|| {
            MsmError::new(ErrorCode::UpstreamError, format!("{title} has no file"))
        })?;
        let download = v.download()?;
        for d in &v.dependencies {
            let dep_project = match (&d.project_id, &d.version_id) {
                (Some(p), _) => p.clone(),
                (None, Some(vid)) => src.version(vid).await?.project_id,
                (None, None) => continue,
            };
            match d.dependency_type.as_str() {
                "required" => queue.push_back((dep_project, Some(title.clone()))),
                "optional" => {
                    if !installed.has(&dep_project) {
                        out.optional.push(dep_project);
                    }
                }
                "incompatible" => incompatible.push((dep_project, title.clone())),
                _ => {}
            }
        }
        out.install.push(PlanItem {
            project_id: pid,
            title,
            version_id: v.id.clone(),
            version_number: v.version_number.clone(),
            version_type: v.version_type.clone(),
            filename: file.filename.clone(),
            size: file.size,
            required_by: parent,
            environment: v.environment.clone(),
            download: Some(download),
            sha512: file.hashes.sha512.clone(),
        });
    }
    for (bad, by) in incompatible {
        if installed.has(&bad) || out.install.iter().any(|i| i.project_id == bad) {
            let name = installed
                .items
                .iter()
                .find(|i| i.project_id == bad)
                .map_or(bad.clone(), |i| i.title.clone());
            out.conflicts
                .push(format!("{by} can't be used together with {name}"));
        }
    }
    out.optional.sort();
    out.optional.dedup();
    Ok(out)
}

fn safe_jar_name(name: &str) -> Result<RelPath, MsmError> {
    let rel = RelPath::parse(name)?;
    if rel.components().count() != 1 || !name.to_ascii_lowercase().ends_with(".jar") {
        return Err(MsmError::new(
            ErrorCode::PathRejected,
            format!("unexpected file name {name:?}"),
        ));
    }
    Ok(rel)
}

/// Writes downloaded files into `server/<target_dir>/` and records them. `files` pairs each plan
/// item with the path of its verified, cached download.
///
/// # Errors
/// Path or I/O errors. Files already written stay; the caller restores the safety snapshot.
pub fn apply(
    server_dir: &Path,
    target_dir: &str,
    plan: &Plan,
    files: &[(usize, std::path::PathBuf)],
) -> Result<Registry, MsmError> {
    let dir = server_dir.join(target_dir);
    std::fs::create_dir_all(&dir).map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
    let mut reg = Registry::load(server_dir);
    for (idx, blob) in files {
        let item = &plan.install[*idx];
        let name = safe_jar_name(&item.filename)?;
        let bytes =
            std::fs::read(blob).map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
        write_atomic(&name.join_onto(&dir), &bytes)
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
        reg.items.retain(|i| i.project_id != item.project_id);
        reg.items.push(InstalledItem {
            project_id: item.project_id.clone(),
            version_id: item.version_id.clone(),
            version_number: item.version_number.clone(),
            title: item.title.clone(),
            file: format!("{target_dir}/{}", name.as_str()),
            sha512: item.sha512.clone(),
            required_by: item.required_by.iter().cloned().collect(),
        });
    }
    reg.items
        .sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    reg.save(server_dir)?;
    Ok(reg)
}

/// Removes an installed project's file and record. Returns the titles of installed items that
/// were installed because they needed it (they may stop working).
///
/// # Errors
/// `NotFound` if not installed; path or I/O errors.
pub fn remove(server_dir: &Path, project: &str) -> Result<Vec<String>, MsmError> {
    let mut reg = Registry::load(server_dir);
    let item = reg
        .items
        .iter()
        .find(|i| i.project_id == project)
        .cloned()
        .ok_or_else(|| MsmError::new(ErrorCode::NotFound, "that mod or plugin is not installed"))?;
    let rel = RelPath::parse(&item.file)?;
    let path = rel.join_onto(server_dir);
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
    }
    reg.items.retain(|i| i.project_id != project);
    // Items that needed this one and are still installed may stop working.
    let dependents: Vec<String> = item
        .required_by
        .iter()
        .filter(|t| reg.items.iter().any(|i| &i.title == *t))
        .cloned()
        .collect();
    reg.save(server_dir)?;
    Ok(dependents)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use msm_providers::modrinth::{Dependency, Hashes, VersionFile};

    use super::*;

    fn ver(project: &str, id: &str, ty: &str, env: Option<&str>, deps: &[(&str, &str)]) -> Version {
        Version {
            id: id.into(),
            project_id: project.into(),
            name: id.into(),
            version_number: format!("{id}-num"),
            version_type: ty.into(),
            loaders: vec!["fabric".into()],
            game_versions: vec!["26.3".into()],
            environment: env.map(Into::into),
            dependencies: deps
                .iter()
                .map(|(p, t)| Dependency {
                    version_id: None,
                    project_id: Some((*p).into()),
                    dependency_type: (*t).into(),
                })
                .collect(),
            files: vec![VersionFile {
                url: url::Url::parse(&format!("https://cdn.modrinth.com/data/{project}/{id}.jar"))
                    .unwrap(),
                filename: format!("{project}-{id}.jar"),
                primary: true,
                size: 10,
                hashes: Hashes {
                    sha512: "a".repeat(128),
                    sha1: None,
                },
            }],
            date_published: String::new(),
        }
    }

    struct Fake(HashMap<&'static str, Vec<Version>>);

    impl VersionSource for Fake {
        async fn versions(&self, project: &str) -> Result<Vec<Version>, DownloadError> {
            Ok(self.0.get(project).cloned().unwrap_or_default())
        }
        async fn version(&self, _id: &str) -> Result<Version, DownloadError> {
            Err(DownloadError::Status(404))
        }
        async fn title(&self, project: &str) -> String {
            project.to_uppercase()
        }
    }

    fn src() -> Fake {
        Fake(HashMap::from([
            (
                "mod",
                vec![
                    ver("mod", "v2a", "alpha", Some("server_only"), &[]),
                    ver(
                        "mod",
                        "v1",
                        "release",
                        Some("client_and_server"),
                        &[
                            ("api", "required"),
                            ("extra", "optional"),
                            ("bad", "incompatible"),
                        ],
                    ),
                ],
            ),
            (
                "api",
                vec![ver(
                    "api",
                    "a1",
                    "release",
                    Some("client_or_server_prefers_both"),
                    &[],
                )],
            ),
            (
                "extra",
                vec![ver("extra", "e1", "release", Some("server_only"), &[])],
            ),
            (
                "bad",
                vec![ver("bad", "b1", "release", Some("server_only"), &[])],
            ),
            (
                "hud",
                vec![ver("hud", "h1", "release", Some("client_only"), &[])],
            ),
            (
                "needs-hud",
                vec![ver(
                    "needs-hud",
                    "n1",
                    "release",
                    Some("server_only"),
                    &[("hud", "required")],
                )],
            ),
            (
                "beta-only",
                vec![ver("beta-only", "x1", "beta", Some("server_only"), &[])],
            ),
        ]))
    }

    #[tokio::test]
    async fn plans_release_with_required_dependency() {
        let p = plan(&src(), "mod", &Registry::default()).await.unwrap();
        let ids: Vec<_> = p
            .install
            .iter()
            .map(|i| (i.project_id.as_str(), i.version_id.as_str()))
            .collect();
        assert_eq!(
            ids,
            [("mod", "v1"), ("api", "a1")],
            "alpha skipped, dependency added"
        );
        assert_eq!(p.install[1].required_by.as_deref(), Some("MOD"));
        assert_eq!(p.optional, ["extra"]);
        assert!(p.conflicts.is_empty());
    }

    #[tokio::test]
    async fn already_installed_and_incompatible() {
        let mut reg = Registry::default();
        for (pid, title) in [("api", "API"), ("bad", "BAD")] {
            reg.items.push(InstalledItem {
                project_id: pid.into(),
                version_id: "x".into(),
                version_number: "x".into(),
                title: title.into(),
                file: format!("mods/{pid}.jar"),
                sha512: String::new(),
                required_by: vec![],
            });
        }
        let p = plan(&src(), "mod", &reg).await.unwrap();
        assert_eq!(p.install.len(), 1);
        assert_eq!(p.already_installed, ["api"]);
        assert_eq!(p.conflicts, ["MOD can't be used together with BAD"]);
    }

    #[tokio::test]
    async fn client_only_content_is_never_installed() {
        assert!(plan(&src(), "hud", &Registry::default()).await.is_err());
        let p = plan(&src(), "needs-hud", &Registry::default())
            .await
            .unwrap();
        assert_eq!(p.install.len(), 1);
        assert!(p.warnings.iter().any(|w| w.contains("client-only")));
    }

    #[tokio::test]
    async fn beta_fallback_and_missing_versions() {
        let p = plan(&src(), "beta-only", &Registry::default())
            .await
            .unwrap();
        assert_eq!(p.install[0].version_type, "beta");
        assert!(plan(&src(), "nothing", &Registry::default()).await.is_err());
    }

    #[test]
    fn apply_and_remove() {
        let d = tempfile::tempdir().unwrap();
        let blob = d.path().join("blob");
        std::fs::write(&blob, b"jarbytes").unwrap();
        let item = |pid: &str, by: Option<&str>| PlanItem {
            project_id: pid.into(),
            title: pid.to_uppercase(),
            version_id: "v".into(),
            version_number: "1".into(),
            version_type: "release".into(),
            filename: format!("{pid}.jar"),
            size: 8,
            required_by: by.map(Into::into),
            environment: None,
            download: None,
            sha512: "a".repeat(128),
        };
        let p = Plan {
            install: vec![item("mod", None), item("api", Some("MOD"))],
            ..Plan::default()
        };
        let server = d.path().join("server");
        std::fs::create_dir(&server).unwrap();
        let reg = apply(&server, "mods", &p, &[(0, blob.clone()), (1, blob.clone())]).unwrap();
        assert_eq!(reg.items.len(), 2);
        assert_eq!(
            std::fs::read(server.join("mods/mod.jar")).unwrap(),
            b"jarbytes"
        );
        assert_eq!(
            remove(&server, "api").unwrap(),
            vec!["MOD".to_string()],
            "MOD needed API"
        );
        assert!(!server.join("mods/api.jar").exists());
        assert_eq!(remove(&server, "mod").unwrap(), Vec::<String>::new());
        assert!(remove(&server, "mod").is_err());

        let evil = Plan {
            install: vec![PlanItem {
                filename: "../../evil.jar".into(),
                ..item("x", None)
            }],
            ..Plan::default()
        };
        assert!(apply(&server, "mods", &evil, &[(0, blob)]).is_err());
    }
}
