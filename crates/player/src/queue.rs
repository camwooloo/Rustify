//! Putting a track at the front of the queue — "play next".
//!
//! Spotify has no public way to do this. `POST /me/player/queue` only appends,
//! and there is no reorder or delete endpoint, so the documented API cannot
//! express "before everything else".
//!
//! The official clients do it with a `set_queue` player command, sent to
//! whichever device is playing through the private connect-state channel. That
//! is what this does. librespot implements the receiving half
//! (`ConnectState::handle_set_queue`), so a Rustify device understands it, and
//! so does the real Spotify app.
//!
//! Two consequences worth knowing, both from `set_queue` replacing the whole
//! list rather than editing it:
//!
//! * We can only send back what we know about, so the caller passes everything
//!   it has queued. Anything queued from another client is not visible to us
//!   and is dropped.
//! * The upcoming context tracks do not need sending: librespot refills them
//!   from the context index the moment the queued track is consumed.
//!
//! Being private, this can break without warning. It fails loudly rather than
//! silently doing nothing.

use anyhow::{anyhow, Result};
use librespot_core::Session;
use serde_json::json;
use tracing::debug;

/// Replace the queue with `uris`, in order, on `to_device`.
///
/// The first entry becomes the next thing that plays.
pub async fn set_queue(session: &Session, to_device: &str, uris: &[String]) -> Result<()> {
    let next_tracks: Vec<_> = uris
        .iter()
        .map(|uri| {
            json!({
                "uri": uri,
                "uid": "",
                // "queue" is what marks a track as yours rather than the
                // context's: it plays once and does not move the album or
                // playlist position along.
                "provider": "queue",
                "metadata": { "is_queued": "true" },
            })
        })
        .collect();

    let body = json!({
        "command": {
            "endpoint": "set_queue",
            "next_tracks": next_tracks,
            // Left to rebuild itself from playback. We cannot read the current
            // history, and sending a guess would be worse than sending none.
            "prev_tracks": [],
            "queue_revision": "",
            "logging_params": {},
        }
    });

    let from = session.device_id().to_string();
    let base = session.spclient().base_url().await?;
    let url = format!("{base}/connect-state/v1/player/command/from/{from}/to/{to_device}");

    // Sent with a plain client rather than through librespot's `spclient`
    // helper: that appends metrics and salt query parameters, which this
    // endpoint rejects outright. Going direct also lets us read the body of a
    // failure, which is the only thing that says *why* on a private API.
    // The same token spclient itself uses. The older keymaster token endpoint
    // answers 403 for this client id, so asking for scopes by name is not an
    // option here.
    let token = session.login5().auth_token().await?;

    let response = reqwest::Client::new()
        .post(&url)
        .bearer_auth(&token.access_token)
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    debug!(%url, %status, body = %&text[..text.len().min(300)], "set_queue");

    if !status.is_success() {
        return Err(anyhow!(
            "Spotify refused the queue change ({status}): {}",
            text.trim().chars().take(200).collect::<String>()
        ));
    }
    Ok(())
}
