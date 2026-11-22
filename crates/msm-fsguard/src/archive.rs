//! Safe extraction of ZIP and tar.gz archives (threats T1, T3).
//!
//! Every entry name must parse as a [`RelPath`], so traversal, absolute, drive, UNC and other
//! tricks are rejected before anything is written. Extraction targets a fresh, empty directory
//! that we created, so joining validated relative paths cannot escape it. Limits cap the number
//! of entries, the total unpacked size and the compression ratio (zip bombs). Duplicate names,
//! including names that differ only in case (which collide on Windows), are rejected.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use crate::RelPath;

/// Extraction limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum number of entries (files + directories).
    pub max_entries: usize,
    /// Maximum total uncompressed bytes.
    pub max_total_bytes: u64,
    /// Maximum per-file compression ratio for files larger than 1 MiB (ZIP only).
    pub max_ratio: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_total_bytes: 4 * 1024 * 1024 * 1024,
            max_ratio: 200,
        }
    }
}

/// What to do with symbolic/hard link entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPolicy {
    /// Fail the whole extraction (untrusted archives: modpacks, backups from elsewhere).
    Reject,
    /// Skip the entry and report it (verified vendor archives whose links are non-essential).
    Skip,
}

/// Summary of a successful extraction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Files written.
    pub files: usize,
    /// Bytes written.
    pub bytes: u64,
    /// Link entries skipped under [`LinkPolicy::Skip`].
    pub skipped_links: Vec<String>,
}

/// Why extraction stopped. The destination may contain partial output and should be deleted.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// An entry name failed path validation.
    #[error("unsafe path in archive: {name:?} ({reason})")]
    UnsafePath {
        /// Raw entry name.
        name: String,
        /// Why.
        reason: String,
    },
    /// Link entry under [`LinkPolicy::Reject`].
    #[error("archive contains a link entry: {0:?}")]
    Link(String),
    /// Two entries map to the same file.
    #[error("archive contains duplicate entry: {0:?}")]
    Duplicate(String),
    /// A limit was exceeded.
    #[error("archive exceeds limit: {0}")]
    Limit(String),
    /// Destination must be an existing empty directory.
    #[error("destination must be an existing empty directory")]
    Destination,
    /// Malformed archive.
    #[error("invalid archive: {0}")]
    Invalid(String),
    /// I/O error.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<ArchiveError> for msm_core::MsmError {
    fn from(e: ArchiveError) -> Self {
        use msm_core::ErrorCode as C;
        let code = match &e {
            ArchiveError::UnsafePath { .. }
            | ArchiveError::Link(_)
            | ArchiveError::Duplicate(_) => C::PathRejected,
            ArchiveError::Limit(_) => C::InsufficientDiskSpace,
            ArchiveError::Invalid(_) => C::UpstreamError,
            ArchiveError::Destination | ArchiveError::Io(_) => C::Internal,
        };
        msm_core::MsmError::new(code, e.to_string())
    }
}

struct Writer<'a> {
    dest: &'a Path,
    limits: Limits,
    seen: HashSet<String>,
    entries: usize,
    report: Report,
}

impl Writer<'_> {
    fn check_name(&mut self, raw: &str) -> Result<Option<RelPath>, ArchiveError> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            return Err(ArchiveError::Limit(format!(
                "more than {} entries",
                self.limits.max_entries
            )));
        }
        let trimmed = raw.trim_end_matches(['/', '\\']);
        if trimmed.is_empty() || trimmed == "." {
            return Ok(None); // the archive root itself
        }
        let rel = RelPath::parse(trimmed).map_err(|e| ArchiveError::UnsafePath {
            name: raw.to_owned(),
            reason: e.to_string(),
        })?;
        Ok(Some(rel))
    }

    fn claim(&mut self, rel: &RelPath, is_dir: bool) -> Result<(), ArchiveError> {
        let key = rel.as_str().to_lowercase();
        if !self.seen.insert(key) && !is_dir {
            return Err(ArchiveError::Duplicate(rel.to_string()));
        }
        Ok(())
    }

    fn dir(&mut self, rel: &RelPath) -> Result<(), ArchiveError> {
        self.claim(rel, true)?;
        fs::create_dir_all(rel.join_onto(self.dest))?;
        Ok(())
    }

    fn file(
        &mut self,
        rel: &RelPath,
        mut reader: impl Read,
        mode: Option<u32>,
    ) -> Result<(), ArchiveError> {
        self.claim(rel, false)?;
        let path = rel.join_onto(self.dest);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::options().write(true).create_new(true).open(&path)?;
        let budget = self
            .limits
            .max_total_bytes
            .saturating_sub(self.report.bytes);
        let written = io::copy(&mut (&mut reader).take(budget + 1), &mut out)?;
        if written > budget {
            return Err(ArchiveError::Limit(format!(
                "more than {} bytes unpacked",
                self.limits.max_total_bytes
            )));
        }
        out.flush()?;
        self.report.bytes += written;
        self.report.files += 1;
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt;
            // Keep only the executable bits; never setuid/setgid/sticky or world-writable.
            let perm = if m & 0o111 != 0 { 0o755 } else { 0o644 };
            fs::set_permissions(&path, fs::Permissions::from_mode(perm))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(())
    }
}

fn prepare(dest: &Path) -> Result<(), ArchiveError> {
    let meta = fs::symlink_metadata(dest).map_err(|_| ArchiveError::Destination)?;
    if !meta.is_dir() || meta.file_type().is_symlink() || fs::read_dir(dest)?.next().is_some() {
        return Err(ArchiveError::Destination);
    }
    Ok(())
}

/// Extracts a ZIP archive into an existing, empty directory.
///
/// # Errors
/// See [`ArchiveError`].
pub fn extract_zip(
    archive: &Path,
    dest: &Path,
    limits: Limits,
    links: LinkPolicy,
) -> Result<Report, ArchiveError> {
    prepare(dest)?;
    let mut zip = zip::ZipArchive::new(File::open(archive)?)
        .map_err(|e| ArchiveError::Invalid(e.to_string()))?;
    let mut w = Writer {
        dest,
        limits,
        seen: HashSet::new(),
        entries: 0,
        report: Report::default(),
    };
    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .map_err(|e| ArchiveError::Invalid(e.to_string()))?;
        let raw = String::from_utf8_lossy(entry.name_raw()).into_owned();
        let Some(rel) = w.check_name(&raw)? else {
            continue;
        };
        let mode = entry.unix_mode();
        if mode.is_some_and(|m| m & 0o170_000 == 0o120_000) {
            match links {
                LinkPolicy::Reject => return Err(ArchiveError::Link(raw)),
                LinkPolicy::Skip => {
                    w.report.skipped_links.push(raw);
                    continue;
                }
            }
        }
        if entry.is_dir() {
            w.dir(&rel)?;
            continue;
        }
        let (size, compressed) = (entry.size(), entry.compressed_size().max(1));
        if size > 1024 * 1024 && size / compressed > limits.max_ratio {
            return Err(ArchiveError::Limit(format!(
                "{raw:?} compresses suspiciously well ({}:1)",
                size / compressed
            )));
        }
        w.file(&rel, entry, mode)?;
    }
    Ok(w.report)
}

/// Extracts a gzip-compressed tar archive into an existing, empty directory.
///
/// # Errors
/// See [`ArchiveError`].
pub fn extract_tar_gz(
    archive: &Path,
    dest: &Path,
    limits: Limits,
    links: LinkPolicy,
) -> Result<Report, ArchiveError> {
    prepare(dest)?;
    let gz = flate2::read::GzDecoder::new(File::open(archive)?);
    let mut tar = tar::Archive::new(gz);
    let mut w = Writer {
        dest,
        limits,
        seen: HashSet::new(),
        entries: 0,
        report: Report::default(),
    };
    for entry in tar
        .entries()
        .map_err(|e| ArchiveError::Invalid(e.to_string()))?
    {
        let entry = entry.map_err(|e| ArchiveError::Invalid(e.to_string()))?;
        let raw = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions()
            || kind.is_gnu_longname()
        {
            continue;
        }
        let Some(rel) = w.check_name(&raw)? else {
            continue;
        };
        if kind.is_symlink() || kind.is_hard_link() {
            match links {
                LinkPolicy::Reject => return Err(ArchiveError::Link(raw)),
                LinkPolicy::Skip => {
                    w.report.skipped_links.push(raw);
                    continue;
                }
            }
        }
        if kind.is_dir() {
            w.dir(&rel)?;
        } else if kind.is_file() || kind.is_contiguous() {
            let mode = entry.header().mode().ok();
            w.file(&rel, entry, mode)?;
        } else {
            // Devices, FIFOs and other special entries are never created.
            return Err(ArchiveError::UnsafePath {
                name: raw,
                reason: "special file type".into(),
            });
        }
    }
    Ok(w.report)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn zip_with(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let mut z = zip::ZipWriter::new(File::create(f.path()).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            if name.ends_with('/') {
                z.add_directory(*name, opts).unwrap();
            } else {
                z.start_file(*name, opts).unwrap();
                z.write_all(data).unwrap();
            }
        }
        z.finish().unwrap();
        f
    }

    fn dest() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn extracts_normal_zip() {
        let a = zip_with(&[
            ("jdk/", b""),
            ("jdk/bin/java.exe", b"MZ"),
            ("jdk/release", b"JAVA_VERSION=25"),
        ]);
        let d = dest();
        let r = extract_zip(a.path(), d.path(), Limits::default(), LinkPolicy::Reject).unwrap();
        assert_eq!(r.files, 2);
        assert_eq!(fs::read(d.path().join("jdk/bin/java.exe")).unwrap(), b"MZ");
    }

    #[test]
    fn rejects_malicious_zip_names() {
        for bad in [
            "../evil.txt",
            "a/../../evil",
            "/abs.txt",
            "C:/win.txt",
            "con.txt",
            "a\\..\\..\\x",
            "x:stream",
        ] {
            let a = zip_with(&[(bad, b"x")]);
            let d = dest();
            let err =
                extract_zip(a.path(), d.path(), Limits::default(), LinkPolicy::Reject).unwrap_err();
            assert!(
                matches!(err, ArchiveError::UnsafePath { .. }),
                "{bad}: {err:?}"
            );
            assert!(!d.path().parent().unwrap().join("evil.txt").exists());
        }
    }

    #[test]
    fn rejects_case_insensitive_duplicates() {
        let a = zip_with(&[("mods/A.jar", b"1"), ("mods/a.jar", b"2")]);
        let err = extract_zip(
            a.path(),
            dest().path(),
            Limits::default(),
            LinkPolicy::Reject,
        )
        .unwrap_err();
        assert!(matches!(err, ArchiveError::Duplicate(_)));
    }

    #[test]
    fn enforces_limits() {
        let a = zip_with(&[("a", b"1"), ("b", b"2"), ("c", b"3")]);
        let few = Limits {
            max_entries: 2,
            ..Limits::default()
        };
        assert!(matches!(
            extract_zip(a.path(), dest().path(), few, LinkPolicy::Reject),
            Err(ArchiveError::Limit(_))
        ));
        let small = Limits {
            max_total_bytes: 2,
            ..Limits::default()
        };
        assert!(matches!(
            extract_zip(a.path(), dest().path(), small, LinkPolicy::Reject),
            Err(ArchiveError::Limit(_))
        ));
        // A 20 MiB file of zeros compresses ~1000:1: a bomb under the default ratio limit.
        let zeros = vec![0u8; 20 * 1024 * 1024];
        let bomb = zip_with(&[("big.bin", &zeros)]);
        assert!(matches!(
            extract_zip(
                bomb.path(),
                dest().path(),
                Limits::default(),
                LinkPolicy::Reject
            ),
            Err(ArchiveError::Limit(_))
        ));
    }

    #[test]
    fn destination_must_be_empty() {
        let a = zip_with(&[("a", b"1")]);
        let d = dest();
        fs::write(d.path().join("existing"), b"x").unwrap();
        assert!(matches!(
            extract_zip(a.path(), d.path(), Limits::default(), LinkPolicy::Reject),
            Err(ArchiveError::Destination)
        ));
    }

    fn tar_gz(
        build: impl FnOnce(&mut tar::Builder<flate2::write::GzEncoder<File>>),
    ) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let gz = flate2::write::GzEncoder::new(
            File::create(f.path()).unwrap(),
            flate2::Compression::fast(),
        );
        let mut b = tar::Builder::new(gz);
        build(&mut b);
        b.into_inner().unwrap().finish().unwrap();
        f
    }

    fn file_header(path: &str, len: u64, mode: u32) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_path(path).unwrap();
        h.set_size(len);
        h.set_mode(mode);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        h
    }

    #[test]
    fn extracts_tar_gz_and_skips_or_rejects_links() {
        let make = || {
            tar_gz(|b| {
                let data = b"#!java";
                b.append(
                    &file_header("jre/bin/java", data.len() as u64, 0o755),
                    Cursor::new(data),
                )
                .unwrap();
                let mut link = tar::Header::new_gnu();
                link.set_entry_type(tar::EntryType::Symlink);
                link.set_path("jre/legal/LICENSE").unwrap();
                link.set_link_name("../../../../etc/passwd").unwrap();
                link.set_size(0);
                link.set_cksum();
                b.append(&link, io::empty()).unwrap();
            })
        };
        let d = dest();
        let r =
            extract_tar_gz(make().path(), d.path(), Limits::default(), LinkPolicy::Skip).unwrap();
        assert_eq!(r.files, 1);
        assert_eq!(r.skipped_links, vec!["jre/legal/LICENSE".to_string()]);
        assert!(!d.path().join("jre/legal/LICENSE").exists());
        let err = extract_tar_gz(
            make().path(),
            dest().path(),
            Limits::default(),
            LinkPolicy::Reject,
        )
        .unwrap_err();
        assert!(matches!(err, ArchiveError::Link(_)));
    }

    #[test]
    fn rejects_tar_traversal() {
        let a = tar_gz(|b| {
            // tar::Builder refuses ".." in set_path, so write the raw name bytes.
            let mut h = tar::Header::new_gnu();
            h.as_old_mut().name[..9].copy_from_slice(b"../escape");
            h.set_size(1);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append(&h, Cursor::new(b"x")).unwrap();
        });
        let err = extract_tar_gz(
            a.path(),
            dest().path(),
            Limits::default(),
            LinkPolicy::Reject,
        )
        .unwrap_err();
        assert!(matches!(err, ArchiveError::UnsafePath { .. }), "{err:?}");
    }
}
