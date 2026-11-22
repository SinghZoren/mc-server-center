//! Real download of a managed Temurin JRE (~50 MB). Ignored by default:
//! `cargo test -p msm-engine --test real_java -- --ignored --nocapture`
#![allow(clippy::unwrap_used)]

use msm_engine::java_install::ensure_temurin;
use msm_providers::http::Downloader;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "downloads a JRE from Adoptium"]
async fn installs_and_reuses_temurin_21() {
    let dir = tempfile::tempdir().unwrap();
    let dl = Downloader::new(dir.path().join("cache"), dir.path().join("partial"));
    let runtimes = dir.path().join("runtimes");
    let stage = dir.path().join("stage");
    let progress = |_, _| {};
    let rt = ensure_temurin(
        &dl,
        &runtimes,
        &stage,
        21,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    eprintln!("installed: {rt:?}");
    assert_eq!(rt.major, 21);
    assert!(rt.is_64bit);
    assert!(
        rt.executable
            .starts_with(std::fs::canonicalize(&runtimes).unwrap())
    );
    assert_eq!(
        std::fs::read_dir(&stage).unwrap().count(),
        0,
        "stage cleaned"
    );
    // Second call reuses the installed runtime without downloading.
    let again = ensure_temurin(
        &dl,
        &runtimes,
        &stage,
        21,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(again.executable, rt.executable);
}
