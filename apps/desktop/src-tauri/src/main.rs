//! Desktop controller entry point (LIFE-03, REM-02/03).
//!
//! * Closing the window hides it to the system tray; servers keep running in the background
//!   service (`msmd`) either way.
//! * The tray menu offers Open, Stop all servers, and Quit. Quitting closes only this app; the
//!   service and any running servers continue until stopped.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod daemon_link;
mod hosts;

use hosts::{HostRecord, Hosts};
use msm_api::ApiError;
use serde_json::{Value, json};
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

/// Quits the app window and tray (servers keep running).
#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
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
    let quit = MenuItem::with_id(
        app,
        "quit",
        "Quit (servers keep running)",
        true,
        None::<&str>,
    )?;
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
            "quit" => app.exit(0),
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
            // Closing hides to the tray instead of quitting; servers are unaffected either way.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                let _ = window.emit("msm-tray", "hidden");
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
