//! Lyrics — a **private Spotify API**.
//!
//! Lives beside Jam for the same reason: not part of the public Web API,
//! derived from observing the official clients, and able to break without
//! notice. Keeping it here means a breakage has one obvious place to look.
//!
//! It goes through librespot's authenticated `spclient` channel, so it needs
//! no Developer Dashboard app and is not subject to Web API rate limits.
//!
//! Friend activity used to live here too. It was removed: the buddylist
//! endpoint rejects every credential this app can obtain (401 for spclient and
//! streaming tokens, 403 RBAC for a Web API token) and only accepts a web
//! token minted from an `sp_dc` browser cookie.

use anyhow::{Context, Result};
use librespot_core::{Session, SpotifyId, SpotifyUri};
use serde::Deserialize;
use spotify_proto::{LyricLine, Lyrics};
use tracing::debug;

// ---------------------------------------------------------------------------
// Lyrics
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLyricsEnvelope {
    lyrics: Option<RawLyrics>,
}

#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawLyrics {
    sync_type: String,
    lines: Vec<RawLine>,
    provider_display_name: String,
}

impl Default for RawLyrics {
    fn default() -> Self {
        Self {
            sync_type: "UNSYNCED".to_string(),
            lines: Vec::new(),
            provider_display_name: String::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawLine {
    /// Milliseconds, but delivered as a string.
    start_time_ms: String,
    words: String,
}

/// Fetch time-synced lyrics for a track.
///
/// Spotify serves these per-track and not for everything; a track without
/// lyrics is a normal outcome, reported as an empty result rather than an
/// error so the UI can simply say "no lyrics".
pub async fn lyrics(session: &Session, track_uri: &str) -> Result<Lyrics> {
    // Accept either a full `spotify:track:...` URI or a bare base62 id.
    let id = SpotifyUri::from_uri(track_uri)
        .ok()
        .and_then(|uri| SpotifyId::try_from(&uri).ok())
        .or_else(|| SpotifyId::from_base62(track_uri).ok())
        .ok_or_else(|| anyhow::anyhow!("not a track id: {track_uri}"))?;

    let bytes = session
        .spclient()
        .get_lyrics(&id)
        .await
        .context("requesting lyrics")?;

    let text = String::from_utf8_lossy(&bytes);

    // A 404 body is not JSON; treat anything unparseable as "no lyrics".
    let envelope: RawLyricsEnvelope = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            debug!("no lyrics for {track_uri}: {e}");
            return Ok(Lyrics::default());
        }
    };

    let Some(raw) = envelope.lyrics else {
        return Ok(Lyrics::default());
    };

    let synced = raw.sync_type.eq_ignore_ascii_case("LINE_SYNCED");

    Ok(Lyrics {
        synced,
        provider: (!raw.provider_display_name.is_empty()).then_some(raw.provider_display_name),
        lines: raw
            .lines
            .into_iter()
            .map(|l| LyricLine {
                time_ms: l.start_time_ms.parse().unwrap_or(0),
                text: l.words,
            })
            .collect(),
    })
}

// ---------------------------------------------------------------------------
// Radio
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawRadio {
    media_items: Vec<RawMediaItem>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawMediaItem {
    uri: String,
}

/// Build a radio station seeded from a track.
///
/// This is librespot's internal `inspiredby-mix` endpoint, authenticated by
/// the streaming session — so it works regardless of which Web API app the
/// browsing half uses. Spotify's public `/recommendations` was retired
/// entirely, which is why radio has to come from here.
pub async fn radio_for(session: &Session, seed_uri: &str) -> Result<Option<String>> {
    let uri = SpotifyUri::from_uri(seed_uri)
        .map_err(|e| anyhow::anyhow!("not a playable uri: {e}"))?;

    let bytes = session
        .spclient()
        .get_radio_for_track(&uri)
        .await
        .context("requesting radio")?;

    let text = String::from_utf8_lossy(&bytes);
    let raw: RawRadio = serde_json::from_str(&text).unwrap_or_else(|e| {
        debug!("radio response was not the expected shape: {e}");
        RawRadio::default()
    });

    // The endpoint hands back a generated *playlist* to play, not a track
    // list — e.g. spotify:playlist:37i9dQZF1E8... Playing it as a context
    // keeps Spotify's own ordering and lets it extend as it goes.
    Ok(raw
        .media_items
        .into_iter()
        .map(|m| m.uri)
        .find(|u| u.contains(":playlist:")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_lyrics_parse_with_string_timestamps() {
        let body = r#"{"lyrics":{"syncType":"LINE_SYNCED","providerDisplayName":"Musixmatch",
            "lines":[{"startTimeMs":"1200","words":"Take me back to Eden"},
                     {"startTimeMs":"4800","words":"♪"}]}}"#;
        let raw: RawLyricsEnvelope = serde_json::from_str(body).unwrap();
        let lyrics = raw.lyrics.unwrap();
        assert_eq!(lyrics.lines.len(), 2);
        assert_eq!(lyrics.lines[0].start_time_ms, "1200");
    }

    #[test]
    fn a_track_without_lyrics_is_not_an_error() {
        // Spotify returns non-JSON for tracks with no lyrics.
        let envelope: Result<RawLyricsEnvelope, _> = serde_json::from_str("not json");
        assert!(envelope.is_err(), "guarded by the caller, which returns empty");
    }

}
