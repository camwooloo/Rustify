//! The always-on half of the app.
//!
//! This process holds the Spotify session, decodes audio, and advertises the
//! Connect device. It never opens a window, never touches the GPU, and keeps
//! running when the UI is closed — which is the entire reason this project
//! exists. Close the window before a game and playback continues from here.

mod config;
mod server;
mod state;

use std::time::Duration;

use anyhow::Result;
use librespot_playback::config::Bitrate;
use spotify_player_core::{auth, EngineConfig};
use spotify_proto::{Event, DEFAULT_PORT};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::state::Daemon;

/// How often the progress bar advances. Two updates a second is plenty for a
/// human, and the tick is skipped entirely when no UI is attached.
const TICK: Duration = Duration::from_millis(500);

/// Writes to the log file *and* stdout.
///
/// The app launches the daemon detached, so stdout goes nowhere in normal use
/// and a file is the only way to diagnose a failure after the fact. Running it
/// from a terminal should still print, hence both.
struct Tee(std::fs::File);

impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Never let a broken stdout (detached process) fail the real log.
        let _ = std::io::Write::write_all(&mut std::io::stdout(), buf);
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::Write::flush(&mut std::io::stdout());
        self.0.flush()
    }
}

/// Open (and size-cap) the daemon log next to the token cache.
fn open_log() -> Option<(std::path::PathBuf, std::fs::File)> {
    const MAX_BYTES: u64 = 5 * 1024 * 1024;

    let dirs = directories::ProjectDirs::from("dev", "spotify-rust", "spotify-rust")?;
    let dir = dirs.config_dir().to_path_buf();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("daemon.log");

    // Start fresh rather than growing without bound.
    if std::fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    Some((path, file))
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = || {
        EnvFilter::try_from_env("SPOTIFY_RUST_LOG")
            .unwrap_or_else(|_| EnvFilter::new("info,librespot=warn"))
    };

    let log_file = open_log();
    match &log_file {
        Some((_, file)) => {
            let file = file.try_clone()?;
            tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_ansi(false)
                .with_writer(move || Tee(file.try_clone().expect("clone log handle")))
                .init();
        }
        None => tracing_subscriber::fmt().with_env_filter(filter()).init(),
    }

    if let Some((path, _)) = &log_file {
        info!("logging to {}", path.display());
    }

    // Saved settings are read once here: librespot bakes them into the
    // session and player at construction, which is why the settings page says
    // playback options apply on restart.
    let saved = config::load().settings;

    let config = EngineConfig {
        device_name: std::env::var("SPOTIFY_RUST_DEVICE_NAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| Some(saved.device_name.clone()).filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(default_device_name),
        bitrate: match saved.bitrate {
            96 => Bitrate::Bitrate96,
            160 => Bitrate::Bitrate160,
            _ => Bitrate::Bitrate320,
        },
        normalisation: saved.normalise,
        cache_audio: saved.cache_audio,
        ..Default::default()
    };

    let port = std::env::var("SPOTIFY_RUST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let daemon = Daemon::new(config);

    // A cached token means no browser round-trip on startup.
    match auth::current_token(&auth::Profile::streaming()).await {
        Some(token) => {
            if let Err(e) = daemon.boot(&token).await {
                warn!("could not start from the cached token: {e:#}");
                warn!("sign in again from the app");
            }
        }
        None => info!("no cached credentials; waiting for sign-in"),
    }

    tokio::spawn(tick_position(daemon.clone()));
    tokio::spawn(poll_remote(daemon.clone()));

    server::serve(daemon, port).await
}

/// Advance the reported playback position between librespot's own events.
///
/// librespot only reports position on state changes, which would leave the
/// progress bar frozen mid-track.
///
/// The counter advances even with no UI attached. Skipping the update while
/// nothing listens looks like a saving, but it desynchronises the position:
/// reopening the window would show the track's time frozen wherever it was
/// when the window closed. librespot's own position events resync any drift.
/// Only the broadcast is skipped when there are no receivers, which is where
/// the actual cost is.
async fn tick_position(daemon: std::sync::Arc<Daemon>) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let (playing, position_ms) = {
            let mut s = daemon.state.write().await;
            if !s.playing {
                (false, s.position_ms)
            } else {
                let duration = s.track.as_ref().map(|t| t.duration_ms).unwrap_or(u32::MAX);
                // Clamp at the track end; the real EndOfTrack event will
                // arrive and correct us.
                s.position_ms = (s.position_ms + TICK.as_millis() as u32).min(duration);
                (true, s.position_ms)
            }
        };

        if playing && daemon.events.receiver_count() > 0 {
            let _ = daemon.events.send(Event::Position {
                position_ms,
                playing,
            });
        }
    }
}

/// Mirror the account's playback while this device is idle.
///
/// Polled rather than pushed because the Web API has no event stream. Only
/// runs when a UI is attached: with the window closed there is nobody to show
/// it to, and this is the process that must stay cheap while you game.
async fn poll_remote(daemon: std::sync::Arc<Daemon>) {
    const EVERY: Duration = Duration::from_secs(3);

    let mut interval = tokio::time::interval(EVERY);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        if daemon.events.receiver_count() == 0 {
            continue;
        }
        daemon.poll_remote().await;
    }
}

fn default_device_name() -> String {
    // Matches what other Connect devices show: something recognisable in the
    // device list on a phone.
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .map(|h| format!("{h} (Rustify)"))
        .unwrap_or_else(|_| "Rustify".to_string())
}
