// Release builds must not open a console window behind the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! The window. A thin, disposable view over the daemon.
//!
//! Deliberately holds no playback state of its own: everything rendered here
//! comes from the daemon, so closing and reopening the window is free and
//! cannot desynchronise anything.

mod link;
#[cfg(windows)]
mod smtc;
#[cfg(windows)]
mod thumbbar;
mod tray;
mod spicetify;
mod update;

use std::sync::Arc;

use serde_json::Value;
use tauri::Manager;
use tracing_subscriber::EnvFilter;

use crate::link::DaemonLink;

/// Forward a command to the daemon and return its reply.
///
/// A single generic passthrough keeps the protocol defined in exactly one
/// place (`spotify-proto`) instead of being restated per command here and
/// again in JavaScript.
#[tauri::command]
async fn call(
    state: tauri::State<'_, Arc<DaemonLink>>,
    command: Value,
) -> Result<Value, String> {
    state.call(command).await
}

#[tauri::command]
fn connected(state: tauri::State<'_, Arc<DaemonLink>>) -> bool {
    state.is_connected()
}

/// The Spicetify theme catalogue, cached on disk between runs.
#[tauri::command]
async fn spicetify_themes(
    app: tauri::AppHandle,
    refresh: bool,
) -> Result<Vec<spicetify::Theme>, String> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("no cache directory: {e}"))?;

    spicetify::catalogue(dir, refresh)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Is there a newer release on GitHub? `None` means nothing to do.
#[tauri::command]
async fn check_update() -> Option<update::UpdateInfo> {
    update::check().await
}

/// Download the installer and hand off to it, reporting download progress to
/// the window as it goes.
///
/// The app does not exit here: the silent installer closes Rustify itself and
/// starts the new build when it is done.
#[tauri::command]
async fn apply_update(app: tauri::AppHandle, url: String) -> Result<(), String> {
    use tauri::Emitter;

    let window = app.clone();
    update::apply(&url, move |pct| {
        let _ = window.emit("update-progress", pct);
    })
    .await
    .map_err(|e| format!("{e:#}"))?;

    Ok(())
}

#[cfg(windows)]
fn anyhow_msg(msg: &str) -> String {
    msg.to_string()
}

/// Tell the daemon whether a window is actually on screen.
fn report_visible(app: &tauri::AppHandle, visible: bool) {
    let Some(link) = app.try_state::<Arc<DaemonLink>>() else {
        return;
    };
    let link = link.inner().clone();
    tauri::async_runtime::spawn(async move {
        let _ = link
            .call(serde_json::json!({ "cmd": "setUiVisible", "visible": visible }))
            .await;
    });
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SPOTIFY_RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let port = std::env::var("SPOTIFY_RUST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(spotify_proto::DEFAULT_PORT);

    let daemon_link = DaemonLink::new(port);

    tauri::Builder::default()
        // Must be registered first. Closing the window only hides it, so a
        // second launch would otherwise start a whole new app — and a second
        // tray icon — while the first sat hidden.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .manage(daemon_link.clone())
        .setup(move |app| {
            let handle = app.handle().clone();
            let link = daemon_link.clone();

            // A tray icon matters here: the daemon keeps playing with the
            // window closed, so this is the only way to skip or pause without
            // reopening it.
            if let Err(e) = tray::build(&handle, link.clone()) {
                tracing::warn!("tray icon unavailable: {e}");
            }

            // Media keys and the Windows media overlay. Attached to the window
            // handle, so it survives the window being hidden to the tray.
            #[cfg(windows)]
            {
                use tauri::Manager;
                let hwnd = app
                    .get_webview_window("main")
                    .ok_or_else(|| anyhow_msg("no main window"))
                    .and_then(|w| w.hwnd().map_err(|e| anyhow_msg(&e.to_string())))
                    .map(|hwnd| hwnd.0 as isize);

                match hwnd.clone().and_then(|hwnd| {
                    smtc::Smtc::new(hwnd, link.clone())
                        .map_err(|e| anyhow_msg(&format!("{e:#}")))
                }) {
                    Ok(smtc) => {
                        app.manage(std::sync::Arc::new(smtc));
                        tracing::info!("media controls registered");
                    }
                    Err(e) => tracing::warn!("media controls unavailable: {e}"),
                }

                // The transport buttons under the taskbar preview. Same window
                // handle, and it must be set up on this thread — the shell
                // sends their clicks to the window's message queue.
                match hwnd.and_then(|hwnd| {
                    thumbbar::ThumbBar::new(hwnd, link.clone())
                        .map_err(|e| anyhow_msg(&format!("{e:#}")))
                }) {
                    Ok(bar) => {
                        app.manage(bar);
                        tracing::info!("taskbar buttons registered");
                    }
                    Err(e) => tracing::warn!("taskbar buttons unavailable: {e}"),
                }
            }

            tauri::async_runtime::spawn(async move {
                link::run(link, handle).await;
            });
            Ok(())
        })
        .on_window_event(move |window, event| {
            match event {
                // Hide rather than exit: the player keeps running, and the
                // tray icon is the only way to control it once gone.
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                    // Nothing is on screen, so the daemon can stop polling
                    // Spotify for what the account is doing elsewhere.
                    report_visible(window.app_handle(), false);
                }
                // Covers minimise/restore and the tray's show, so polling
                // resumes exactly when there is something to look at.
                tauri::WindowEvent::Focused(true) => {
                    report_visible(window.app_handle(), true)
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            call,
            connected,
            check_update,
            apply_update,
            spicetify_themes
        ])
        .run(tauri::generate_context!())
        .expect("failed to start the app window");
}
