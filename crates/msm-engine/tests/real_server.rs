//! Real end-to-end tests: install the latest release of each server type from its official
//! source, accept the EULA, start it with an installed Java of the required major, wait for
//! "Done", run a command, and stop it gracefully. They download tens of MB and boot JVMs, so
//! they are `#[ignore]`d by default:
//!
//! ```text
//! cargo test -p msm-engine --test real_server -- --ignored --nocapture
//! ```
//! Nightly CI runs them (planning/08-test-and-release-plan.md).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use msm_engine::java;
use msm_engine::process::{LaunchSpec, ServerProcess};
use msm_engine::vanilla::{self, InstallDirs, InstanceManifest, Software, VanillaRequest};
use msm_providers::http::Downloader;
use msm_providers::mojang::MojangClient;
use tokio_util::sync::CancellationToken;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn install_start_command_stop(software: Software) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let downloader = Downloader::new(root.join("cache"), root.join("partial"));
    let mojang = MojangClient::new(downloader.clone());
    let latest = mojang.manifest().await.unwrap().latest.release;
    eprintln!("{software:?} on latest release {latest}");

    let dirs = InstallDirs {
        stage_root: root.join("stage"),
        instances_root: root.join("instances"),
    };
    let req = VanillaRequest {
        software,
        name: "IT server".into(),
        mc_version: latest,
        port: free_port(),
        memory_mb: 1536,
        allow_unsupported: false,
        fabric_loader: None,
    };
    let progress = |_, _| {};
    let manifest = vanilla::install(
        &downloader,
        &mojang,
        &dirs,
        &req,
        &progress,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    eprintln!(
        "installed: build {:?}, provenance {:?}",
        manifest.build,
        manifest
            .provenance
            .iter()
            .map(|p| (&p.artifact, p.assurance))
            .collect::<Vec<_>>()
    );
    let instance_dir = dirs.instances_root.join(manifest.id.to_string());
    assert_eq!(InstanceManifest::load(&instance_dir).unwrap(), manifest);
    assert!(
        !vanilla::eula_accepted(&instance_dir),
        "EULA must never be pre-accepted"
    );
    assert_eq!(
        std::fs::read_dir(&dirs.stage_root).unwrap().count(),
        0,
        "stage cleaned up"
    );

    let runtimes = java::discover(&[]).await;
    let Some(rt) = java::select(&runtimes, manifest.java_major, &[]) else {
        eprintln!("SKIP: no installed Java {}", manifest.java_major);
        return;
    };

    vanilla::accept_eula(&instance_dir).unwrap();
    let spec = LaunchSpec {
        java: rt.executable.clone(),
        jvm_args: manifest.jvm_args(),
        target: manifest.target.clone(),
        server_args: manifest.server_args.clone(),
        ready_pattern: manifest.ready_pattern.clone(),
    };
    let server_dir = instance_dir.join("server");
    let proc = ServerProcess::spawn(&spec, &server_dir, &instance_dir.join("logs/console.log"))
        .await
        .unwrap();
    // First boots of Paper (patching) and Fabric (libraries) take longer than Vanilla.
    let ready = proc.wait_ready(Duration::from_secs(420)).await;
    let (backlog, _) = proc.subscribe(0);
    if !ready {
        for l in backlog.iter().rev().take(40).rev() {
            eprintln!("{}", l.text);
        }
        panic!("server did not become ready");
    }

    let (_, mut rx) = proc.subscribe(u64::MAX);
    proc.send_line("list").await.unwrap();
    let saw_list = tokio::time::timeout(Duration::from_secs(20), async {
        while let Ok(line) = rx.recv().await {
            if line.text.contains("players online") {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(saw_list, "no response to `list`");
    assert!(
        proc.send_line("say hi\nop attacker").await.is_err(),
        "multi-line input refused"
    );

    let code = proc.stop(Duration::from_secs(120)).await.unwrap();
    assert_eq!(code, Some(0), "graceful stop exits cleanly");
    assert!(
        server_dir.join("world").is_dir(),
        "world was created and saved"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "downloads the Minecraft server and boots a JVM"]
async fn vanilla_latest() {
    install_start_command_stop(Software::Vanilla).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "downloads Paper and boots a JVM"]
async fn paper_latest() {
    install_start_command_stop(Software::Paper).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "downloads Fabric and boots a JVM"]
async fn fabric_latest() {
    install_start_command_stop(Software::Fabric).await;
}
