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
/// Label of the miniplayer window.
const MINI_WINDOW: &str = "mini";

mod spicetify;
mod statsfm;
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

/// Open the miniplayer and put the main window away.
///
/// A window of its own rather than a mode of the main one: it stays above
/// whatever else is on screen, which a hidden-chrome main window cannot do
/// without dragging the whole interface along with it.
#[tauri::command]
async fn open_mini(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(mini) = app.get_webview_window(MINI_WINDOW) {
        let _ = mini.set_focus();
        return Ok(());
    }

    tauri::WebviewWindowBuilder::new(
        &app,
        MINI_WINDOW,
        tauri::WebviewUrl::App("mini.html".into()),
    )
    .title("Rustify")
    .inner_size(300.0, 340.0)
    .min_inner_size(260.0, 300.0)
    .max_inner_size(420.0, 480.0)
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .resizable(true)
    .build()
    .map_err(|e| format!("opening the miniplayer: {e}"))?;

    // Whatever closes the miniplayer — its own button, Alt+F4, a crash of
    // the webview — the main window comes back. Without this the app could
    // end up with every window hidden and only the tray to find it by.
    let handle = app.clone();
    if let Some(mini) = app.get_webview_window(MINI_WINDOW) {
        mini.on_window_event(move |event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                if let Some(main) = handle.get_webview_window("main") {
                    let _ = main.show();
                }
            }
        });
    }

    if let Some(main) = app.get_webview_window("main") {
        let _ = main.hide();
    }
    Ok(())
}

/// Close the miniplayer, bringing the main window back when asked.
///
/// Called both by the miniplayer's own button and when its window is closed
/// any other way, so the main window can never end up hidden with nothing
/// left on screen to bring it back.
#[tauri::command]
async fn close_mini(app: tauri::AppHandle, restore: bool) -> Result<(), String> {
    if let Some(mini) = app.get_webview_window(MINI_WINDOW) {
        let _ = mini.close();
    }
    if restore {
        if let Some(main) = app.get_webview_window("main") {
            let _ = main.show();
            let _ = main.unminimize();
            let _ = main.set_focus();
        }
    }
    Ok(())
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

/// Find a stats.fm profile by name.
#[tauri::command]
async fn statsfm_search(query: String) -> Result<Vec<statsfm::Account>, String> {
    statsfm::search(&query).await.map_err(|e| format!("{e:#}"))
}

/// Everything the stats page shows, for one profile and range.
#[tauri::command]
async fn statsfm_overview(user: String, range: String) -> Result<statsfm::Overview, String> {
    statsfm::overview(&user, &range)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Can this build install an update itself? False on macOS and Linux, where
/// the window offers the download instead.
#[tauri::command]
fn update_installs_itself() -> bool {
    update::installs_itself()
}

/// Is there a newer release on GitHub? `None` means nothing to do.
#[tauri::command]
async fn check_update() -> Option<update::UpdateInfo> {
    update::check().await
}

/// Download the installer and hand off to it, reporting download progress to
/// the window as it goes.
///
/// The app then quits, because Windows will not let anything overwrite the
/// image of a running process: a window left open is a window the installer
/// silently skips, which is how an install can end up with a fresh daemon
/// beside a shell from months ago. The `/R` switch brings it back up.
#[tauri::command]
async fn apply_update(app: tauri::AppHandle, url: String) -> Result<(), String> {
    use tauri::Emitter;

    let window = app.clone();
    update::apply(&url, move |pct| {
        let _ = window.emit("update-progress", pct);
    })
    .await
    .map_err(|e| format!("{e:#}"))?;

    let _ = app.emit("update-installing", ());

    // Long enough for the installer to be up and for the window to say what
    // is about to happen, short enough not to look like a hang.
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    app.exit(0);

    Ok(())
}

/// What this build actually is, as opposed to what its baked-in changelog
/// says. The two can differ when an update only half lands.
#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
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
            app_version,
            update_installs_itself,
            spicetify_themes,
            open_mini,
            close_mini,
            statsfm_search,
            statsfm_overview
        ])
        .run(tauri::generate_context!())
        .expect("failed to start the app window");
}
