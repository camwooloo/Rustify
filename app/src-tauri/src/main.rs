// Release builds must not open a console window behind the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! The window. A thin, disposable view over the daemon.
//!
//! Deliberately holds no playback state of its own: everything rendered here
//! comes from the daemon, so closing and reopening the window is free and
//! cannot desynchronise anything.

mod link;
mod tray;
mod update;

use std::sync::Arc;

use serde_json::Value;
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

/// Is there a newer release on GitHub? `None` means nothing to do.
#[tauri::command]
async fn check_update() -> Option<update::UpdateInfo> {
    update::check().await
}

/// Download the installer and hand off to it. The app exits so the installer
/// can replace both executables.
#[tauri::command]
async fn apply_update(app: tauri::AppHandle, url: String) -> Result<(), String> {
    update::apply(&url).await.map_err(|e| format!("{e:#}"))?;

    // Give the installer a moment to start before the window disappears.
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        app.exit(0);
    });
    Ok(())
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

            tauri::async_runtime::spawn(async move {
                link::run(link, handle).await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            call,
            connected,
            check_update,
            apply_update
        ])
        .run(tauri::generate_context!())
        .expect("failed to start the app window");
}
