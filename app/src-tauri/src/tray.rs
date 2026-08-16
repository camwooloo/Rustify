//! System tray icon.
//!
//! Earns its place here more than in most apps: the daemon keeps playing with
//! the window closed, so without a tray there is no way to pause or skip
//! without reopening the window — which is the exact thing you close before
//! starting a game.
//!
//! Left-clicking the icon shows the window; the menu drives playback straight
//! through the daemon, so it works whether or not a window exists.

use std::sync::Arc;

use serde_json::json;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};
use tracing::{debug, warn};

use crate::link::DaemonLink;

pub fn build(app: &AppHandle, link: Arc<DaemonLink>) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Open Rustify", true, None::<&str>)?;
    let play = MenuItem::with_id(app, "playPause", "Play / Pause", true, None::<&str>)?;
    let next = MenuItem::with_id(app, "next", "Next", true, None::<&str>)?;
    let prev = MenuItem::with_id(app, "previous", "Previous", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Rustify", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &show,
            &PredefinedMenuItem::separator(app)?,
            &prev,
            &play,
            &next,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let handle = app.clone();

    TrayIconBuilder::with_id("rustify")
        .icon(app.default_window_icon().cloned().ok_or_else(|| {
            tauri::Error::AssetNotFound("window icon missing for tray".into())
        })?)
        .tooltip("Rustify")
        .menu(&menu)
        // The menu should only appear on right-click; a left-click opens the
        // window, which is what people expect from a media player's tray icon.
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| {
            let id = event.id().as_ref();

            if id == "show" {
                show_window(app);
                return;
            }
            if id == "quit" {
                // Closes the window only. The daemon is deliberately left
                // running so music does not stop; quit it from the terminal
                // or Task Manager if you really want silence.
                app.exit(0);
                return;
            }

            let link = link.clone();
            let cmd = id.to_string();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = link.call(json!({ "cmd": cmd })).await {
                    warn!("tray command {cmd} failed: {e}");
                }
            });
        })
        .on_tray_icon_event(move |_tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(&handle);
            }
        })
        .build(app)?;

    Ok(())
}

/// Bring the window back, un-minimising it if needed.
fn show_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        debug!("no main window to show");
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}
