//! Installs a managed Eclipse Temurin JRE (JAVA-02): download (sha256-verified), extract with
//! the safe extractor into a staging directory, verify by running it, then move it into
//! `runtimes/java/temurin-<major>-<release>/`. Existing runtimes are reused.

use std::path::{Path, PathBuf};

use msm_core::{ErrorCode, MsmError};
use msm_fsguard::archive::{self, Limits, LinkPolicy};
use msm_providers::adoptium::{self, ArchiveKind};
use msm_providers::http::{Downloader, Progress};
use tokio_util::sync::CancellationToken;

use crate::java::{self, JavaInfo};

fn exe() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

/// Finds `bin/java` inside an extracted JRE (Temurin archives have one top-level folder).
fn find_java(root: &Path) -> Option<PathBuf> {
    let direct = root.join("bin").join(exe());
    if direct.is_file() {
        return Some(direct);
    }
    std::fs::read_dir(root).ok()?.flatten().find_map(|e| {
        let p = e.path().join("bin").join(exe());
        p.is_file().then_some(p)
    })
}

fn safe_dir_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Installs (or reuses) a Temurin JRE for `major` and returns the verified runtime.
///
/// # Errors
/// Download, extraction or verification failures; nothing is left in `runtimes_dir` on failure.
pub async fn ensure_temurin(
    downloader: &Downloader,
    runtimes_dir: &Path,
    stage_root: &Path,
    major: u32,
    progress: Progress<'_>,
    cancel: &CancellationToken,
) -> Result<JavaInfo, MsmError> {
    // Reuse an installed managed runtime of this major.
    let installed = java::discover_in(&[runtimes_dir.to_owned()]).await;
    if let Some(rt) = java::select(&installed, major, &[runtimes_dir.to_owned()]) {
        return Ok(rt.clone());
    }

    let release = adoptium::latest_jre(downloader, major).await?;
    let blob = downloader
        .fetch(&release.download, progress, cancel)
        .await?;

    let stage = stage_root.join(format!("jre-{}", msm_core::ids::TaskId::new()));
    tokio::fs::create_dir_all(&stage)
        .await
        .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
    let result = async {
        let (archive_path, dest) = (blob.path.clone(), stage.clone());
        let kind = release.kind;
        tokio::task::spawn_blocking(move || {
            let limits = Limits {
                max_total_bytes: 1024 * 1024 * 1024,
                ..Limits::default()
            };
            match kind {
                ArchiveKind::Zip => {
                    archive::extract_zip(&archive_path, &dest, limits, LinkPolicy::Skip)
                }
                ArchiveKind::TarGz => {
                    archive::extract_tar_gz(&archive_path, &dest, limits, LinkPolicy::Skip)
                }
            }
        })
        .await
        .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))??;

        let java_path = find_java(&stage).ok_or_else(|| {
            MsmError::new(
                ErrorCode::UpstreamError,
                "the Java download did not contain bin/java",
            )
        })?;
        let probed = java::probe(&java_path)
            .await
            .map_err(|e| MsmError::new(ErrorCode::UpstreamError, e.to_string()))?;
        if probed.major != major || !probed.is_64bit {
            return Err(MsmError::new(
                ErrorCode::UpstreamError,
                format!(
                    "downloaded Java reports version {} ({}), expected {major}",
                    probed.major, probed.arch
                ),
            ));
        }
        // The JRE folder is the parent of bin/.
        let jre_root = java_path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| MsmError::new(ErrorCode::Internal, "unexpected layout"))?
            .to_owned();
        tokio::fs::create_dir_all(runtimes_dir)
            .await
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
        let target = runtimes_dir.join(format!(
            "temurin-{major}-{}",
            safe_dir_name(&release.release_name)
        ));
        if target.exists() {
            tokio::fs::remove_dir_all(&target)
                .await
                .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
        }
        msm_fsguard::rename_with_retry(&jre_root, &target)
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))?;
        let final_java = target.join("bin").join(exe());
        java::probe(&final_java)
            .await
            .map_err(|e| MsmError::new(ErrorCode::Internal, e.to_string()))
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&stage).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_java_one_level_down() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("jdk-25.0.5.1+2-jre").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(exe()), b"").unwrap();
        assert_eq!(find_java(d.path()).unwrap(), bin.join(exe()));
        assert_eq!(safe_dir_name("jdk-25.0.5.1+2"), "jdk-25.0.5.1_2");
    }
}
