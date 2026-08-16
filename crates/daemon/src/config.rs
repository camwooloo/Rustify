//! On-disk daemon configuration.
//!
//! Exists for one reason: the Web API client id. Playback uses librespot's
//! shared "keymaster" id, which is fine for streaming but permanently
//! rate-limited on `api.spotify.com` because every librespot-derived project
//! shares its budget. Browsing therefore needs an id belonging to the user.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Client id Rustify ships with.
///
/// This is a public PKCE client, so there is no secret to leak by embedding
/// it. It is used instead of a freshly registered app because Spotify
/// grandfathered apps created before its November 2024 cull: this one can
/// still reach `browse/categories`, `browse/new-releases` and artist
/// top-tracks, all of which return 403 for any app registered today.
///
/// Note the ceiling: while the app sits in Development mode Spotify allows at
/// most 25 listeners, each added by hand in the dashboard. Anyone else must
/// supply their own id in Settings, which still works.
pub const DEFAULT_WEB_CLIENT_ID: &str = "25a4b61276454466909f5ffc1a5c0b47";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Client id of a Spotify app registered by the user, used only for the
    /// Web API. Empty means browsing is unavailable.
    pub web_client_id: String,
    /// User-changeable playback settings.
    pub settings: spotify_proto::Settings,
}

pub fn path() -> Result<PathBuf> {
    Ok(spotify_player_core::auth::config_dir()?.join("config.json"))
}

pub fn load() -> Config {
    let mut config = path()
        .ok()
        .and_then(|p| std::fs::read_to_string(&p).ok().map(|raw| (p, raw)))
        .map(|(p, raw)| {
            serde_json::from_str(&raw).unwrap_or_else(|e| {
                tracing::warn!("ignoring malformed {}: {e}", p.display());
                Config::default()
            })
        })
        .unwrap_or_default();

    // The environment wins over the file, so a quick trial needs no edit —
    // but it must not discard the rest of the saved config.
    if let Ok(id) = std::env::var("SPOTIFY_RUST_CLIENT_ID") {
        if !id.trim().is_empty() {
            config.web_client_id = id.trim().to_string();
        }
    }

    // Fall back to the bundled id so browsing works out of the box.
    if config.web_client_id.trim().is_empty() {
        config.web_client_id = DEFAULT_WEB_CLIENT_ID.to_string();
    }

    config
}

pub fn log_path() -> Result<PathBuf> {
    Ok(spotify_player_core::auth::config_dir()?.join("daemon.log"))
}

fn audio_cache_dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "spotify-rust", "spotify-rust")?;
    Some(dirs.cache_dir().join("audio"))
}

/// Total bytes of cached audio. Walks the tree; the cache is shallow.
pub fn audio_cache_bytes() -> u64 {
    fn walk(dir: &std::path::Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path()),
                Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => 0,
            })
            .sum()
    }
    audio_cache_dir().map(|d| walk(&d)).unwrap_or(0)
}

/// Delete cached audio, returning how many bytes went away.
pub fn clear_audio_cache() -> Result<u64> {
    let Some(dir) = audio_cache_dir() else {
        return Ok(0);
    };
    let before = audio_cache_bytes();
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(before)
}

pub fn save(config: &Config) -> Result<()> {
    let path = path()?;
    let raw = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
