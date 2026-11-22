//! Integration tests for `Root::resolve` against a real file system.
#![allow(clippy::unwrap_used)] // test helpers outside #[test] fns

use std::fs;
use std::path::Path;

use msm_fsguard::{ConfineError, Existence, RelPath, Root};

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

fn setup() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("world/region")).unwrap();
    fs::write(dir.path().join("world/level.dat"), b"x").unwrap();
    fs::write(dir.path().join("server.properties"), b"motd=hi\n").unwrap();
    let root = Root::open(dir.path()).unwrap();
    (dir, root)
}

#[test]
fn resolves_existing_paths_inside_root() {
    let (_dir, root) = setup();
    let p = root
        .resolve(&rel("world/level.dat"), Existence::MustExist)
        .unwrap();
    assert!(p.starts_with(root.path()));
    assert_eq!(fs::read(p).unwrap(), b"x");
}

#[test]
fn missing_target_depends_on_mode() {
    let (_dir, root) = setup();
    let missing = rel("world/new/file.txt");
    assert!(matches!(
        root.resolve(&missing, Existence::MustExist),
        Err(ConfineError::NotFound(p)) if p == "world/new"
    ));
    let p = root.resolve(&missing, Existence::MayBeMissing).unwrap();
    assert!(p.starts_with(root.path()));
    assert!(p.ends_with(Path::new("world").join("new").join("file.txt")));
}

#[test]
fn file_used_as_directory_is_rejected() {
    let (_dir, root) = setup();
    assert!(matches!(
        root.resolve(&rel("server.properties/x"), Existence::MayBeMissing),
        Err(ConfineError::NotADirectory(p)) if p == "server.properties"
    ));
}

#[test]
fn root_must_be_an_existing_directory() {
    let (dir, _root) = setup();
    assert!(matches!(
        Root::open(dir.path().join("server.properties")),
        Err(ConfineError::InvalidRoot { .. })
    ));
    assert!(matches!(
        Root::open(dir.path().join("nope")),
        Err(ConfineError::InvalidRoot { .. })
    ));
}

#[cfg(unix)]
#[test]
fn symlink_escape_is_rejected() {
    let (dir, root) = setup();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"s").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("world/link")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("file-link"))
        .unwrap();

    for path in ["world/link/secret", "world/link", "file-link"] {
        for mode in [Existence::MustExist, Existence::MayBeMissing] {
            assert!(
                matches!(
                    root.resolve(&rel(path), mode),
                    Err(ConfineError::TraversesLink(_))
                ),
                "{path} {mode:?}"
            );
        }
    }
}

/// Junctions need no special privileges on Windows, so this runs on every Windows machine.
#[cfg(windows)]
#[test]
fn junction_escape_is_rejected() {
    let (dir, root) = setup();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"s").unwrap();
    let link = dir.path().join("world").join("junction");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path())
        .output()
        .unwrap();
    assert!(status.status.success(), "mklink failed: {status:?}");

    for path in ["world/junction/secret", "world/junction"] {
        for mode in [Existence::MustExist, Existence::MayBeMissing] {
            assert!(
                matches!(
                    root.resolve(&rel(path), mode),
                    Err(ConfineError::TraversesLink(_))
                ),
                "{path} {mode:?}"
            );
        }
    }
}

#[test]
fn root_opened_through_a_link_is_pinned_to_its_real_location() {
    let real = tempfile::tempdir().unwrap();
    fs::write(real.path().join("f"), b"1").unwrap();
    let root = Root::open(real.path()).unwrap();
    // The canonical root resolves files normally.
    assert!(root.resolve(&rel("f"), Existence::MustExist).is_ok());
    assert_eq!(root.path(), fs::canonicalize(real.path()).unwrap());
}
