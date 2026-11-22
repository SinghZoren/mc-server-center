//! Backups and restore (BAK-01..04, ADR-007).
//!
//! * Format: a ZIP (opens natively on Windows) containing the server files plus
//!   `msm-backup.json` (written last) listing every file with its size and SHA-256. The same
//!   manifest is stored next to the archive as `<id>.json` for quick listing.
//! * Symlinks/junctions inside the server folder are never followed; they are listed as skipped.
//! * Restore extracts into a fresh staging directory with the safe extractor (links rejected),
//!   verifies every hash against the manifest, then swaps directories. The previous `server/`
//!   folder is **kept** as `server.before-restore-<time>/`, so a restore never destroys data.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use msm_core::{ErrorCode, MsmError};
use msm_fsguard::archive::{self, Limits, LinkPolicy};
use msm_fsguard::{RelPath, swap_dir, write_atomic};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Name of the manifest entry inside the archive.
pub const MANIFEST_ENTRY: &str = "msm-backup.json";
/// Manifest format version.
pub const FORMAT_VERSION: u32 = 1;

/// What a backup contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Everything in `server/` except logs and caches.
    Full,
    /// Worlds plus settings and player lists.
    World,
    /// Mods, plugins and their configs (safety snapshot before content changes).
    Content,
}

/// One file in a backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to `server/`.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256.
    pub sha256: String,
}

/// Backup manifest (also stored as a sidecar).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    /// Format version.
    pub format_version: u32,
    /// Backup ID.
    pub id: String,
    /// Instance ID.
    pub instance: String,
    /// Minecraft version at backup time.
    pub mc_version: String,
    /// Unix ms.
    pub created_at_ms: i64,
    /// `manual`, `safety:<operation>`, …
    pub reason: String,
    /// Scope.
    pub scope: Scope,
    /// Whether the server's saving was paused and flushed (running servers).
    pub quiesced: bool,
    /// Files.
    pub files: Vec<FileEntry>,
    /// Links that were not followed.
    pub skipped: Vec<String>,
    /// Total bytes of all files.
    pub total_bytes: u64,
}

const EXCLUDED_DIRS: &[&str] = &[
    "logs",
    "cache",
    "crash-reports",
    "debug",
    ".fabric",
    "libraries",
    "versions",
];

fn included(rel: &str, scope: Scope) -> bool {
    let first = rel.split('/').next().unwrap_or(rel);
    if rel.ends_with("session.lock") {
        return false; // held open by a running server; recreated on start
    }
    match scope {
        Scope::Full => !EXCLUDED_DIRS.contains(&first),
        Scope::Content => {
            matches!(first, "mods" | "plugins" | "config") || rel == crate::content::REGISTRY_FILE
        }
        Scope::World => {
            first.starts_with("world")
                || matches!(
                    rel,
                    "server.properties"
                        | "ops.json"
                        | "whitelist.json"
                        | "banned-players.json"
                        | "banned-ips.json"
                        | "usercache.json"
                )
        }
    }
}

fn is_link(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return meta.file_attributes() & 0x400 != 0;
    }
    #[allow(unreachable_code)]
    false
}

fn walk(
    root: &Path,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(String, PathBuf, u64)>,
    skipped: &mut Vec<String>,
) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.flatten().collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let meta = fs::symlink_metadata(e.path())?;
        if is_link(&meta) {
            skipped.push(rel);
        } else if meta.is_dir() {
            walk(root, &e.path(), &rel, out, skipped)?;
        } else if meta.is_file() {
            out.push((rel, e.path(), meta.len()));
        }
    }
    let _ = root;
    Ok(())
}

/// Sizes the files a backup would contain (for the disk-space estimate).
///
/// # Errors
/// I/O errors walking the directory.
pub fn estimate(server_dir: &Path, scope: Scope) -> io::Result<u64> {
    let (mut files, mut skipped) = (Vec::new(), Vec::new());
    walk(server_dir, server_dir, "", &mut files, &mut skipped)?;
    Ok(files
        .iter()
        .filter(|(r, _, _)| included(r, scope))
        .map(|(_, _, s)| *s)
        .sum())
}

/// Space to always leave free on the disk (BAK-04).
pub const DISK_RESERVE_BYTES: u64 = 512 * 1024 * 1024;

/// Fails with `INSUFFICIENT_DISK_SPACE` (showing both numbers) unless `needed` bytes plus a
/// reserve are free on the volume holding `dir`.
///
/// # Errors
/// [`ErrorCode::InsufficientDiskSpace`].
pub fn ensure_space(dir: &Path, needed: u64) -> Result<(), MsmError> {
    let probe = dir.ancestors().find(|p| p.exists()).unwrap_or(dir);
    let Ok(available) = fs4::available_space(probe) else {
        return Ok(());
    };
    let required = needed.saturating_add(DISK_RESERVE_BYTES);
    if available < required {
        let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
        return Err(MsmError::new(
            ErrorCode::InsufficientDiskSpace,
            format!(
                "needs about {:.1} GB free (including a safety margin) but only {:.1} GB is available",
                gib(required),
                gib(available)
            ),
        ));
    }
    Ok(())
}

/// What to back up.
#[derive(Debug, Clone)]
pub struct BackupRequest<'a> {
    /// Backup ID (ULID).
    pub id: &'a str,
    /// Instance ID.
    pub instance: &'a str,
    /// Minecraft version.
    pub mc_version: &'a str,
    /// Reason.
    pub reason: &'a str,
    /// Scope.
    pub scope: Scope,
    /// Whether the caller paused saving first.
    pub quiesced: bool,
}

/// Creates `<dest_dir>/<id>.zip` and `<dest_dir>/<id>.json` from `server_dir`. Blocking; run it
/// on a blocking thread. On failure the partial archive is removed.
///
/// # Errors
/// I/O errors (including a full disk).
pub fn create(
    server_dir: &Path,
    dest_dir: &Path,
    req: &BackupRequest<'_>,
) -> Result<BackupManifest, MsmError> {
    let io_err = |e: io::Error| {
        MsmError::new(
            if e.raw_os_error() == Some(112) || e.raw_os_error() == Some(28) {
                ErrorCode::InsufficientDiskSpace
            } else {
                ErrorCode::Internal
            },
            e.to_string(),
        )
    };
    fs::create_dir_all(dest_dir).map_err(io_err)?;
    // Uncompressed size is an upper bound for the archive.
    ensure_space(dest_dir, estimate(server_dir, req.scope).unwrap_or(0))?;
    let zip_path = dest_dir.join(format!("{}.zip", req.id));
    let tmp_path = dest_dir.join(format!(".{}.zip.partial", req.id));
    let result = (|| -> Result<BackupManifest, MsmError> {
        let (mut all, mut skipped) = (Vec::new(), Vec::new());
        walk(server_dir, server_dir, "", &mut all, &mut skipped).map_err(io_err)?;
        let mut zip = zip::ZipWriter::new(File::create(&tmp_path).map_err(io_err)?);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .large_file(true);
        let mut files = Vec::new();
        let mut buf = vec![0u8; 256 * 1024];
        for (rel, path, _) in all.iter().filter(|(r, _, _)| included(r, req.scope)) {
            zip.start_file(rel.as_str(), opts)
                .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
            let mut f = File::open(path).map_err(io_err)?;
            let mut hasher = Sha256::new();
            let mut size = 0u64;
            loop {
                let n = f.read(&mut buf).map_err(io_err)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                zip.write_all(&buf[..n]).map_err(io_err)?;
                size += n as u64;
            }
            files.push(FileEntry {
                path: rel.clone(),
                size,
                sha256: hex::encode(hasher.finalize()),
            });
        }
        let total_bytes = files.iter().map(|f| f.size).sum();
        let manifest = BackupManifest {
            format_version: FORMAT_VERSION,
            id: req.id.to_owned(),
            instance: req.instance.to_owned(),
            mc_version: req.mc_version.to_owned(),
            created_at_ms: now_ms(),
            reason: req.reason.to_owned(),
            scope: req.scope,
            quiesced: req.quiesced,
            files,
            skipped,
            total_bytes,
        };
        let json = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
        zip.start_file(MANIFEST_ENTRY, opts)
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
        zip.write_all(&json).map_err(io_err)?;
        let file = zip
            .finish()
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
        file.sync_all().map_err(io_err)?;
        drop(file);
        msm_fsguard::rename_with_retry(&tmp_path, &zip_path).map_err(io_err)?;
        write_atomic(&dest_dir.join(format!("{}.json", req.id)), &json).map_err(io_err)?;
        Ok(manifest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

/// Reads the sidecar manifests of all backups in a directory, newest first.
#[must_use]
pub fn list(dest_dir: &Path) -> Vec<BackupManifest> {
    let mut out: Vec<BackupManifest> = fs::read_dir(dest_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter(|e| e.path().with_extension("zip").is_file())
        .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
        .collect();
    out.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms));
    out
}

/// Result of verifying an archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verification {
    /// All files present and matching.
    pub ok: bool,
    /// Problems found.
    pub problems: Vec<String>,
}

/// Re-reads every file in the archive and checks it against the embedded manifest. Blocking.
///
/// # Errors
/// I/O errors opening the archive.
pub fn verify(zip_path: &Path) -> Result<(BackupManifest, Verification), MsmError> {
    let invalid = |m: String| MsmError::new(ErrorCode::UpstreamError, m);
    let mut z = zip::ZipArchive::new(
        File::open(zip_path).map_err(|e| MsmError::new(ErrorCode::NotFound, e.to_string()))?,
    )
    .map_err(|e| invalid(format!("not a valid backup archive: {e}")))?;
    let manifest: BackupManifest = {
        let mut entry = z
            .by_name(MANIFEST_ENTRY)
            .map_err(|_| invalid("backup has no manifest".into()))?;
        let mut s = Vec::new();
        entry
            .read_to_end(&mut s)
            .map_err(|e| invalid(e.to_string()))?;
        serde_json::from_slice(&s).map_err(|e| invalid(format!("manifest is unreadable: {e}")))?
    };
    let mut problems = Vec::new();
    let mut buf = vec![0u8; 256 * 1024];
    for f in &manifest.files {
        match z.by_name(&f.path) {
            Err(_) => problems.push(format!("missing: {}", f.path)),
            Ok(mut entry) => {
                let mut h = Sha256::new();
                let mut size = 0u64;
                loop {
                    match entry.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            h.update(&buf[..n]);
                            size += n as u64;
                        }
                        Err(e) => {
                            problems.push(format!("unreadable: {} ({e})", f.path));
                            break;
                        }
                    }
                }
                if size != f.size || hex::encode(h.finalize()) != f.sha256 {
                    problems.push(format!("changed or corrupt: {}", f.path));
                }
            }
        }
    }
    Ok((
        manifest,
        Verification {
            ok: problems.is_empty(),
            problems,
        },
    ))
}

/// Restores a backup over `instance_dir/server` (the server must be stopped). Returns the path
/// where the previous `server/` folder was kept. Blocking.
///
/// # Errors
/// Verification failures (nothing changes), extraction errors, or I/O errors. On any error the
/// current server folder is left as it was.
pub fn restore(
    zip_path: &Path,
    instance_dir: &Path,
    stage_root: &Path,
) -> Result<PathBuf, MsmError> {
    let (manifest, check) = verify(zip_path)?;
    if !check.ok {
        return Err(MsmError::new(
            ErrorCode::DownloadHashMismatch,
            format!("backup failed verification: {}", check.problems.join("; ")),
        ));
    }
    // Unpacked files go to the staging area first (same volume as the instance).
    ensure_space(stage_root, manifest.total_bytes)?;
    let stage = stage_root.join(format!("restore-{}", manifest.id));
    if stage.exists() {
        fs::remove_dir_all(&stage)
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
    }
    fs::create_dir_all(&stage).map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
    let result = (|| -> Result<PathBuf, MsmError> {
        let staged_server = stage.join("server");
        fs::create_dir(&staged_server)
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
        archive::extract_zip(
            zip_path,
            &staged_server,
            Limits {
                max_total_bytes: u64::MAX / 2,
                ..Limits::default()
            },
            LinkPolicy::Reject,
        )?;
        // The manifest entry is not part of the server.
        let _ = fs::remove_file(staged_server.join(MANIFEST_ENTRY));
        // Re-check what landed on disk.
        for f in &manifest.files {
            let rel = RelPath::parse(&f.path)?;
            let bytes = fs::read(rel.join_onto(&staged_server))
                .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
            if hex::encode(Sha256::digest(&bytes)) != f.sha256 {
                return Err(MsmError::new(
                    ErrorCode::DownloadHashMismatch,
                    format!("{} changed during restore", f.path),
                ));
            }
        }
        // A world-only backup restores on top of a copy of the current non-world files.
        let current = instance_dir.join("server");
        if manifest.scope != Scope::Full && current.is_dir() {
            copy_missing(&current, &staged_server, "", manifest.scope)?;
        }
        let kept = instance_dir.join(format!("server.before-restore-{}", now_ms()));
        swap_dir(&staged_server, &current, &kept).map_err(|e| {
            MsmError::new(
                ErrorCode::Internal,
                format!(
                    "could not swap folders (is the server or a file explorer using them?): {e}"
                ),
            )
        })?;
        Ok(kept)
    })();
    let _ = fs::remove_dir_all(&stage);
    result
}

/// For scoped restores: copies files from the current server (`src`) that the backup's scope
/// does not cover, so only the backed-up part is rolled back.
fn copy_missing(src: &Path, dst: &Path, prefix: &str, scope: Scope) -> Result<(), MsmError> {
    let ioe = |e: io::Error| MsmError::new(ErrorCode::Internal, e.to_string());
    for e in fs::read_dir(src).map_err(ioe)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if included(&rel, scope)
            || (prefix.is_empty() && scope == Scope::World && name.starts_with("world"))
        {
            continue;
        }
        let meta = fs::symlink_metadata(e.path()).map_err(ioe)?;
        let target = dst.join(&name);
        if is_link(&meta) {
            continue;
        } else if meta.is_dir() {
            fs::create_dir_all(&target).map_err(ioe)?;
            copy_missing(&e.path(), &target, &rel, scope)?;
        } else if !target.exists() {
            fs::copy(e.path(), &target).map_err(ioe)?;
        }
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("server");
        fs::create_dir_all(s.join("world/region")).unwrap();
        fs::create_dir_all(s.join("logs")).unwrap();
        fs::create_dir_all(s.join("mods")).unwrap();
        fs::write(s.join("world/level.dat"), b"level-v1").unwrap();
        fs::write(s.join("world/region/r.0.0.mca"), vec![7u8; 100_000]).unwrap();
        fs::write(s.join("world/session.lock"), b"lock").unwrap();
        fs::write(s.join("server.properties"), b"motd=hi\n").unwrap();
        fs::write(s.join("mods/a.jar"), b"jar").unwrap();
        fs::write(s.join("logs/latest.log"), b"log").unwrap();
        d
    }

    fn req(id: &str, scope: Scope) -> BackupRequest<'_> {
        BackupRequest {
            id,
            instance: "inst",
            mc_version: "26.3",
            reason: "manual",
            scope,
            quiesced: false,
        }
    }

    #[test]
    fn full_backup_round_trip_keeps_previous_folder() {
        let d = server();
        let inst = d.path();
        let backups = inst.join("backups");
        let m = create(&inst.join("server"), &backups, &req("b1", Scope::Full)).unwrap();
        let paths: Vec<_> = m.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"world/level.dat"));
        assert!(paths.contains(&"mods/a.jar"));
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with("logs/") || p.ends_with("session.lock"))
        );
        assert_eq!(list(&backups).len(), 1);
        assert!(verify(&backups.join("b1.zip")).unwrap().1.ok);

        // Damage the world, then restore.
        fs::write(inst.join("server/world/level.dat"), b"GRIEFED").unwrap();
        let kept = restore(&backups.join("b1.zip"), inst, &inst.join("stage")).unwrap();
        assert_eq!(
            fs::read(inst.join("server/world/level.dat")).unwrap(),
            b"level-v1"
        );
        assert_eq!(
            fs::read(kept.join("world/level.dat")).unwrap(),
            b"GRIEFED",
            "previous state kept"
        );
        assert!(
            !inst.join("stage").read_dir().unwrap().any(|_| true),
            "stage cleaned"
        );
    }

    #[test]
    fn world_backup_keeps_other_files_on_restore() {
        let d = server();
        let inst = d.path();
        create(
            &inst.join("server"),
            &inst.join("backups"),
            &req("w1", Scope::World),
        )
        .unwrap();
        fs::write(inst.join("server/mods/new.jar"), b"added after backup").unwrap();
        fs::remove_dir_all(inst.join("server/world")).unwrap();
        restore(&inst.join("backups/w1.zip"), inst, &inst.join("stage")).unwrap();
        assert_eq!(
            fs::read(inst.join("server/world/level.dat")).unwrap(),
            b"level-v1"
        );
        assert_eq!(
            fs::read(inst.join("server/mods/new.jar")).unwrap(),
            b"added after backup"
        );
    }

    #[test]
    fn content_snapshot_rolls_back_only_mods() {
        let d = server();
        let inst = d.path();
        create(
            &inst.join("server"),
            &inst.join("backups"),
            &req("s1", Scope::Content),
        )
        .unwrap();
        fs::write(inst.join("server/mods/new.jar"), b"new mod").unwrap();
        fs::write(inst.join("server/world/level.dat"), b"played since").unwrap();
        restore(&inst.join("backups/s1.zip"), inst, &inst.join("stage")).unwrap();
        assert!(
            !inst.join("server/mods/new.jar").exists(),
            "new mod rolled back"
        );
        assert!(inst.join("server/mods/a.jar").exists());
        assert_eq!(
            fs::read(inst.join("server/world/level.dat")).unwrap(),
            b"played since",
            "world kept"
        );
    }

    #[test]
    fn corrupt_backup_is_detected_and_not_restored() {
        let d = server();
        let inst = d.path();
        let backups = inst.join("backups");
        create(&inst.join("server"), &backups, &req("c1", Scope::Full)).unwrap();
        // Rewrite the archive with a tampered file but the original manifest.
        let src = backups.join("c1.zip");
        let mut z = zip::ZipArchive::new(File::open(&src).unwrap()).unwrap();
        let bad = backups.join("bad.zip");
        let mut w = zip::ZipWriter::new(File::create(&bad).unwrap());
        for i in 0..z.len() {
            let mut e = z.by_index(i).unwrap();
            let name = e.name().to_owned();
            let mut data = Vec::new();
            e.read_to_end(&mut data).unwrap();
            if name == "world/level.dat" {
                data = b"tampered".to_vec();
            }
            w.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(&data).unwrap();
        }
        w.finish().unwrap();
        let (_, v) = verify(&bad).unwrap();
        assert!(!v.ok);
        fs::write(inst.join("server/world/level.dat"), b"current").unwrap();
        assert!(restore(&bad, inst, &inst.join("stage")).is_err());
        assert_eq!(
            fs::read(inst.join("server/world/level.dat")).unwrap(),
            b"current",
            "untouched"
        );
    }

    #[test]
    fn space_check_reports_both_numbers() {
        let d = tempfile::tempdir().unwrap();
        assert!(ensure_space(d.path(), 1).is_ok());
        let err = ensure_space(d.path(), u64::MAX / 4).unwrap_err();
        assert_eq!(err.code, ErrorCode::InsufficientDiskSpace);
        assert!(err.detail.contains("GB"));
    }

    #[test]
    fn estimate_matches_included_files() {
        let d = server();
        let full = estimate(&d.path().join("server"), Scope::Full).unwrap();
        let world = estimate(&d.path().join("server"), Scope::World).unwrap();
        assert!(full > world);
        assert_eq!(world, 8 + 100_000 + 8);
    }
}
