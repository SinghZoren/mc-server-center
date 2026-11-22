//! Desktop controller entry point (LIFE-03, REM-02/03).
//!
//! * Closing the window or choosing Quit in the tray quits everything: servers are stopped
//!   cleanly, the background service (`msmd`) shuts down, and any leftover `msmd.exe` is ended.
//! * The tray menu offers Open, Stop all servers, and Quit.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod daemon_link;
mod hosts;

use hosts::{HostRecord, Hosts};
use msm_api::ApiError;
use serde_json::{Value, json};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

/// Sends a command to a daemon (`host` = `local` or a paired host ID). Every daemon-side check
/// (capabilities, validation, state machine) still applies; this is only a relay.
#[tauri::command]
async fn daemon_request(
    hosts: tauri::State<'_, Hosts>,
    host: Option<String>,
    cmd: String,
    params: Option<Value>,
) -> Result<Value, ApiError> {
    let link = hosts.link(host.as_deref().unwrap_or("local"))?;
    link.request(cmd, params.unwrap_or(Value::Null)).await
}

/// Subscribes to a daemon topic; events arrive as `msm-event` tagged with the host.
#[tauri::command]
async fn daemon_subscribe(
    hosts: tauri::State<'_, Hosts>,
    host: Option<String>,
    topic: String,
    from_seq: Option<u64>,
) -> Result<(), ApiError> {
    hosts
        .link(host.as_deref().unwrap_or("local"))?
        .subscribe(topic, from_seq)
        .await;
    Ok(())
}

/// The last known connection state of a host (events only report changes).
#[tauri::command]
fn connection_status(
    states: tauri::State<'_, daemon_link::ConnStates>,
    host: String,
) -> Option<daemon_link::UiConnection> {
    states.0.lock().ok().and_then(|m| m.get(&host).cloned())
}

/// Lists hosts: this PC plus paired remotes.
#[tauri::command]
fn list_hosts(hosts: tauri::State<'_, Hosts>) -> Value {
    let mut out = vec![json!({"id": "local", "name": "This PC", "address": null})];
    out.extend(
        hosts
            .records()
            .into_iter()
            .map(|r| json!({"id": r.id, "name": r.name, "address": r.address})),
    );
    Value::Array(out)
}

/// Pairs with a remote host using a one-time code.
#[tauri::command]
async fn pair_host(
    app: AppHandle,
    hosts: tauri::State<'_, Hosts>,
    address: String,
    code: String,
    name: String,
) -> Result<HostRecord, ApiError> {
    hosts.pair(&app, &address, &code, &name).await
}

/// Forgets a remote host on this desktop.
#[tauri::command]
fn remove_host(hosts: tauri::State<'_, Hosts>, id: String) -> Result<(), ApiError> {
    hosts.remove(&id)
}

/// Quits everything: stops this PC's servers cleanly, shuts down the background service, then
/// force-ends any `msmd.exe` still left (a hung or older service) so the next start is clean.
#[tauri::command]
fn quit_app(app: AppHandle) {
    quit_everything(&app);
}

fn quit_everything(app: &AppHandle) {
    use std::sync::atomic::Ordering;
    if daemon_link::QUITTING.swap(true, Ordering::SeqCst) {
        return; // already quitting
    }
    let _ = app.emit("msm-quitting", ());
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some("MC Server Center: stopping servers…"));
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let link = app.state::<Hosts>().link("local").ok();
        let mut pid = None;
        if let Some(link) = &link {
            let info = tokio::time::timeout(
                Duration::from_secs(3),
                link.request("system.info".into(), Value::Null),
            )
            .await;
            pid = info
                .ok()
                .and_then(Result::ok)
                .and_then(|v| v["pid"].as_u64());
            if pid.is_some() {
                // Waits while servers save and stop (each force-ended after ~50 s).
                let _ = tokio::time::timeout(
                    Duration::from_secs(100),
                    link.request("system.shutdown".into(), json!({"stop_servers": true})),
                )
                .await;
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
        }
        force_end(pid);
        app.exit(0);
    });
}

/// Ends the service if it is still there, plus any other `msmd.exe` (with their child processes).
fn force_end(pid: Option<u64>) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let run = |args: &[&str]| {
            let _ = std::process::Command::new("taskkill")
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        };
        if let Some(pid) = pid {
            run(&["/F", "/T", "/PID", &pid.to_string()]);
        }
        run(&["/F", "/T", "/IM", "msmd.exe"]);
    }
    #[cfg(not(windows))]
    {
        if let Some(pid) = pid {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open MC Server Center", true, None::<&str>)?;
    let stop_all = MenuItem::with_id(app, "stop_all", "Stop all servers", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit (stops servers)", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &sep, &stop_all, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("MC Server Center")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "stop_all" => {
                let Ok(link) = app.state::<Hosts>().link("local") else {
                    return;
                };
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let result = link.request("system.stop_all".into(), Value::Null).await;
                    let _ = app.emit(
                        "msm-tray",
                        if result.is_ok() {
                            "stopping_all"
                        } else {
                            "stop_all_failed"
                        },
                    );
                });
            }
            "quit" => quit_everything(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn main() {
    let result = tauri::Builder::default()
        .setup(|app| {
            let dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("msm-desktop"));
            app.manage(daemon_link::ConnStates::default());
            let hosts = Hosts::new(dir);
            hosts.start_all(app.handle());
            app.manage(hosts);
            build_tray(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window (X, or "Close window" on the taskbar) quits everything; the
            // window stays up showing progress until servers have stopped.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                quit_everything(window.app_handle());
            }
        })
        .invoke_handler(tauri::generate_handler![
            daemon_request,
            daemon_subscribe,
            quit_app,
            list_hosts,
            pair_host,
            remove_host,
            connection_status
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("failed to start the desktop app: {e}");
        std::process::exit(1);
    }
}
