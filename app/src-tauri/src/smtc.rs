//! Windows System Media Transport Controls.
//!
//! This is what puts Rustify on the media overlay Windows shows when you hit
//! a media key, and what makes those keys work at all while another window
//! has focus. Without it, play/pause only worked with Rustify focused.
//!
//! SMTC is attached to the window handle, so it lives in the app rather than
//! the daemon. Closing the window only hides it, so the controls keep working
//! for as long as Rustify is in the tray. Quitting from the tray gives the
//! session up, which is correct — there is no window left to represent it.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::json;
use tracing::{debug, warn};
use windows::{
    core::HSTRING,
    Foundation::{TypedEventHandler, Uri},
    Media::{
        MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls,
        SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
    },
    Storage::Streams::RandomAccessStreamReference,
    Win32::{Foundation::HWND, System::WinRT::ISystemMediaTransportControlsInterop},
};

use crate::link::DaemonLink;

pub struct Smtc {
    controls: SystemMediaTransportControls,
}

// The COM objects are apartment-agnostic here and only touched from the Tauri
// event loop and its async tasks.
unsafe impl Send for Smtc {}
unsafe impl Sync for Smtc {}

impl Smtc {
    /// Attach media controls to the app window and route button presses to
    /// the daemon.
    pub fn new(hwnd_raw: isize, link: Arc<DaemonLink>) -> Result<Self> {
        // Build our own HWND rather than reusing Tauri's, so this does not
        // break if Tauri moves to a different `windows` crate version.
        let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);

        let interop: ISystemMediaTransportControlsInterop =
            windows::core::factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()
                .context("getting the SMTC interop factory")?;

        let controls: SystemMediaTransportControls = unsafe {
            interop
                .GetForWindow(hwnd)
                .context("attaching media controls to the window")?
        };

        controls.SetIsEnabled(true)?;
        controls.SetIsPlayEnabled(true)?;
        controls.SetIsPauseEnabled(true)?;
        controls.SetIsNextEnabled(true)?;
        controls.SetIsPreviousEnabled(true)?;
        controls.SetPlaybackStatus(MediaPlaybackStatus::Stopped)?;

        controls
            .DisplayUpdater()?
            .SetType(MediaPlaybackType::Music)?;

        // Media keys and the overlay's buttons both arrive here.
        controls.ButtonPressed(&TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_, args| {
            let Some(args) = args.as_ref() else {
                return Ok(());
            };
            let cmd = match args.Button()? {
                SystemMediaTransportControlsButton::Play => "play",
                SystemMediaTransportControlsButton::Pause => "pause",
                SystemMediaTransportControlsButton::Next => "next",
                SystemMediaTransportControlsButton::Previous => "previous",
                SystemMediaTransportControlsButton::Stop => "pause",
                other => {
                    debug!(?other, "ignoring media button");
                    return Ok(());
                }
            };

            let link = link.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = link.call(json!({ "cmd": cmd })).await {
                    warn!("media key {cmd} failed: {e}");
                }
            });
            Ok(())
        }))?;

        Ok(Self { controls })
    }

    pub fn set_playing(&self, playing: bool) {
        let status = if playing {
            MediaPlaybackStatus::Playing
        } else {
            MediaPlaybackStatus::Paused
        };
        if let Err(e) = self.controls.SetPlaybackStatus(status) {
            debug!("could not set playback status: {e}");
        }
    }

    pub fn set_stopped(&self) {
        let _ = self.controls.SetPlaybackStatus(MediaPlaybackStatus::Stopped);
    }

    /// Push the current track to the overlay.
    pub fn set_track(&self, title: &str, artist: &str, album: &str, art_url: Option<&str>) {
        if let Err(e) = self.update(title, artist, album, art_url) {
            debug!("could not update the media overlay: {e}");
        }
    }

    fn update(
        &self,
        title: &str,
        artist: &str,
        album: &str,
        art_url: Option<&str>,
    ) -> Result<()> {
        let updater = self.controls.DisplayUpdater()?;
        updater.SetType(MediaPlaybackType::Music)?;

        let music = updater.MusicProperties()?;
        music.SetTitle(&HSTRING::from(title))?;
        music.SetArtist(&HSTRING::from(artist))?;
        music.SetAlbumTitle(&HSTRING::from(album))?;

        // Windows fetches the artwork itself; handing it the CDN URL avoids
        // downloading and caching the image on our side.
        match art_url {
            Some(url) if url.starts_with("https://") => {
                let uri = Uri::CreateUri(&HSTRING::from(url))?;
                updater.SetThumbnail(&RandomAccessStreamReference::CreateFromUri(&uri)?)?;
            }
            _ => updater.SetThumbnail(None)?,
        }

        updater.Update()?;
        Ok(())
    }
}

/// Update the overlay from a daemon event frame.
///
/// Driven by the same events the window renders from, so the overlay cannot
/// drift from what is actually playing — including playback happening on
/// another device, which Rustify mirrors.
pub fn apply_event(app: &tauri::AppHandle, frame: &serde_json::Value) {
    use tauri::Manager;

    let Some(smtc) = app.try_state::<std::sync::Arc<Smtc>>() else {
        return;
    };
    let smtc = smtc.inner();

    let names = |v: &serde_json::Value| {
        v.get("artists")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("name").and_then(|n| n.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };

    match frame.get("event").and_then(|e| e.as_str()) {
        Some("trackChanged") => {
            smtc.set_track(
                frame.get("name").and_then(|n| n.as_str()).unwrap_or_default(),
                &names(frame),
                frame
                    .get("album")
                    .and_then(|a| a.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or_default(),
                frame.get("coverUrl").and_then(|c| c.as_str()),
            );
        }
        Some("position") => {
            smtc.set_playing(frame.get("playing").and_then(|p| p.as_bool()).unwrap_or(false));
        }
        Some("state") => {
            match frame.get("track") {
                Some(track) if !track.is_null() => {
                    smtc.set_track(
                        track.get("name").and_then(|n| n.as_str()).unwrap_or_default(),
                        &names(track),
                        track
                            .get("album")
                            .and_then(|a| a.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or_default(),
                        track.get("coverUrl").and_then(|c| c.as_str()),
                    );
                    smtc.set_playing(
                        frame.get("playing").and_then(|p| p.as_bool()).unwrap_or(false),
                    );
                }
                // Nothing loaded: leave the overlay empty rather than stale.
                _ => smtc.set_stopped(),
            }
        }
        _ => {}
    }
}
