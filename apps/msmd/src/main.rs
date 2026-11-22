//! `msmd` — the MC Server Center background service.
//!
//! ```text
//! msmd serve [--data-dir DIR]   run the daemon (local IPC only)
//! msmd runner <spec.json>       internal: own one server process (started by the daemon)
//! msmd --version
//! ```

mod daemon;
mod events;
mod remote_server;
mod runner;
mod runner_client;
mod session;

use std::path::PathBuf;

use msm_api::Principal;
use msm_storage::layout::DataDir;

fn usage() -> ! {
    eprintln!("usage: msmd serve [--data-dir DIR] | msmd --version");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => println!("msmd {}", env!("CARGO_PKG_VERSION")),
        Some("runner") => {
            let spec = args.get(1).map(PathBuf::from).unwrap_or_else(|| usage());
            let rt = tokio::runtime::Runtime::new().unwrap_or_else(|e| {
                eprintln!("failed to start runtime: {e}");
                std::process::exit(1)
            });
            if let Err(e) = rt.block_on(runner::run(spec)) {
                eprintln!("msmd runner: {e}");
                std::process::exit(1);
            }
        }
        Some("serve") => {
            let (data_dir, custom) = match (args.get(1).map(String::as_str), args.get(2)) {
                (None, _) => match DataDir::default_for_os() {
                    Ok(d) => (d.root().to_owned(), false),
                    Err(e) => {
                        eprintln!("cannot determine data directory: {e}");
                        std::process::exit(1)
                    }
                },
                (Some("--data-dir"), Some(dir)) => (PathBuf::from(dir), true),
                _ => usage(),
            };
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("failed to start runtime: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = rt.block_on(serve(DataDir::at(data_dir), custom)) {
                eprintln!("msmd: {e}");
                std::process::exit(1);
            }
        }
        _ => usage(),
    }
}

/// Writes where clients can find this daemon (`run/endpoint`).
fn write_endpoint(data: &DataDir, endpoint: &str) -> std::io::Result<()> {
    msm_fsguard::write_atomic(&data.run().join("endpoint"), endpoint.as_bytes())
}

async fn serve(data: DataDir, custom_dir: bool) -> Result<(), String> {
    data.ensure()
        .map_err(|e| format!("cannot create data directory: {e}"))?;
    let _lock = data.lock().map_err(|e| e.to_string())?;
    let daemon = daemon::Daemon::open(data.clone())
        .await
        .map_err(|e| e.to_string())?;
    eprintln!(
        "msmd {} data dir: {}",
        env!("CARGO_PKG_VERSION"),
        data.root().display()
    );
    daemon.spawn_dns_updater();
    if let Some((bind, port)) = daemon.remote_listen_setting() {
        match remote_server::start(daemon.clone(), bind, port).await {
            Ok(fp) => eprintln!(
                "remote access on {bind}:{port}, fingerprint {}",
                msm_transport::remote::display_fingerprint(&fp)
            ),
            Err(e) => eprintln!("remote access could not start: {e}"),
        }
    }

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    #[cfg(windows)]
    {
        use msm_transport::windows_pipe::{PipeListener, default_pipe_name};
        let mut name = default_pipe_name().map_err(|e| e.to_string())?;
        if custom_dir {
            name = format!("{name}-{}", msm_core::ids::RequestId::new());
        }
        let mut listener =
            PipeListener::bind(&name).map_err(|e| format!("cannot open pipe {name}: {e}"))?;
        write_endpoint(&data, &name).map_err(|e| e.to_string())?;
        eprintln!("listening on {name}");
        loop {
            tokio::select! {
                conn = listener.accept() => match conn {
                    Ok(stream) => { tokio::spawn(session::serve(daemon.clone(), stream, Principal::LocalOwner, None)); }
                    Err(e) => eprintln!("rejected local connection: {e}"),
                },
                _ = &mut shutdown => break,
                () = daemon.shutdown_requested() => break,
            }
        }
    }
    #[cfg(unix)]
    {
        let _ = custom_dir;
        use msm_transport::unix_socket::SocketListener;
        let path = data.run().join("msmd.sock");
        let mut listener = SocketListener::bind(&path)
            .await
            .map_err(|e| format!("cannot bind {}: {e}", path.display()))?;
        write_endpoint(&data, &path.to_string_lossy()).map_err(|e| e.to_string())?;
        eprintln!("listening on {}", path.display());
        loop {
            tokio::select! {
                conn = listener.accept() => match conn {
                    Ok(stream) => { tokio::spawn(session::serve(daemon.clone(), stream, Principal::LocalOwner, None)); }
                    Err(e) => eprintln!("rejected local connection: {e}"),
                },
                _ = &mut shutdown => break,
                () = daemon.shutdown_requested() => break,
            }
        }
    }
    // Servers keep running in their runner processes; a restarted daemon re-attaches to them.
    eprintln!("service stopped; running servers keep running");
    drop(daemon);
    Ok(())
}
