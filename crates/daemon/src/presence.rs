//! Discord Rich Presence.
//!
//! Off unless someone turns it on. It exists so that a person listening
//! through Rustify shows up as listening through Rustify — Discord's own
//! Spotify integration only reads the official client, so using this player
//! otherwise costs you the status you had.
//!
//! It lives in the daemon rather than the window because the daemon is what
//! knows what is playing, and it outlives the window by design.
//!
//! Discord is optional in every direction: if it is not running, the
//! connection fails and is retried later; if it goes away mid-song, the next
//! update reconnects. Nothing here is allowed to interrupt playback.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use discord_rich_presence::activity::{Activity, Assets, Button, Timestamps};
use discord_rich_presence::{DiscordIpc, DiscordIpcClient};
use spotify_proto::PlayerState;
use tracing::{debug, warn};

/// The Discord application this presence belongs to.
///
/// **This is a placeholder and has to be replaced.** Create an application at
/// <https://discord.com/developers/applications>, put its id here, and upload
/// the logo under Rich Presence assets as `rustify`. The id decides the name
/// Discord prints above the status — which is the point of the feature — and
/// until it is a real one, Discord accepts the connection and shows nothing.
const APP_ID: &str = "0000000000000000000";

/// Asset key for the artwork shown beside the status. Uploaded to the
/// application's Rich Presence assets under this name.
const LOGO_ASSET: &str = "rustify";

const REPO: &str = "https://github.com/camwooloo/Rustify";

pub struct Presence {
    client: Option<DiscordIpcClient>,
    /// What was last sent, so an unchanged state is not resent every tick.
    last: Option<String>,
}

impl Presence {
    pub fn new() -> Self {
        Self {
            client: None,
            last: None,
        }
    }

    /// Connect if not connected. Failure here is normal — Discord may simply
    /// not be running — so it is reported once at debug and retried later.
    fn connect(&mut self) -> Result<&mut DiscordIpcClient> {
        if self.client.is_none() {
            let mut client = DiscordIpcClient::new(APP_ID);
            client.connect().context("connecting to Discord")?;
            debug!("connected to Discord");
            self.client = Some(client);
        }
        Ok(self.client.as_mut().expect("just connected"))
    }

    /// Drop the connection so the next update starts a fresh one.
    fn reset(&mut self) {
        if let Some(mut client) = self.client.take() {
            let _ = client.close();
        }
        self.last = None;
    }

    /// Show what is playing, or clear the status when nothing is.
    pub fn update(&mut self, state: &PlayerState) {
        let Some(track) = state.track.as_ref() else {
            self.clear();
            return;
        };

        if !state.playing {
            // A paused player should not claim to be listening.
            self.clear();
            return;
        }

        let artists = track
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");

        // Discord requires two characters in these fields and silently drops
        // an activity that breaks the rule.
        let details = pad(&track.name);
        let state_line = pad(&artists);

        let key = format!("{details}|{state_line}|{}", state.position_ms / 1000);
        if self.last.as_deref() == Some(key.as_str()) {
            return;
        }

        // An end timestamp makes Discord draw the remaining-time bar.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let ends_at = now + ((track.duration_ms.saturating_sub(state.position_ms)) / 1000) as i64;

        let album = track.album.as_ref().map(|a| a.name.clone()).unwrap_or_default();
        let large_text = if album.is_empty() {
            "Rustify".to_string()
        } else {
            pad(&album)
        };

        let result = (|| -> Result<()> {
            let client = self.connect()?;
            let activity = Activity::new()
                .details(&details)
                .state(&state_line)
                .assets(
                    Assets::new()
                        .large_image(LOGO_ASSET)
                        .large_text(&large_text),
                )
                .timestamps(Timestamps::new().end(ends_at))
                .buttons(vec![Button::new("Get Rustify", REPO)]);

            client.set_activity(activity).context("setting the activity")
        })();

        match result {
            Ok(()) => self.last = Some(key),
            Err(e) => {
                debug!("discord presence unavailable: {e:#}");
                self.reset();
            }
        }
    }

    /// Take the status down without dropping the connection.
    pub fn clear(&mut self) {
        if self.last.is_none() {
            return;
        }
        if let Some(client) = self.client.as_mut() {
            if let Err(e) = client.clear_activity() {
                warn!("could not clear the Discord status: {e}");
                self.reset();
                return;
            }
        }
        self.last = None;
    }

    /// Disconnect entirely, for when the setting is switched off.
    pub fn shutdown(&mut self) {
        if self.client.is_some() {
            debug!("disconnecting from Discord");
        }
        self.reset();
    }
}

/// Discord rejects any field shorter than two characters.
fn pad(text: &str) -> String {
    let trimmed = text.trim();
    match trimmed.chars().count() {
        0 => "  ".to_string(),
        1 => format!("{trimmed} "),
        _ => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::pad;

    #[test]
    fn short_fields_are_padded_to_what_discord_accepts() {
        assert_eq!(pad("4"), "4 ");
        assert_eq!(pad(""), "  ");
        assert_eq!(pad("  "), "  ");
        assert_eq!(pad("Church"), "Church");
        // Padding is not trimming: a normal title is untouched apart from
        // the whitespace around it.
        assert_eq!(pad("  So Stunning  "), "So Stunning");
    }
}
