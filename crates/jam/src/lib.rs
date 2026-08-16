//! Jam / Group Session support — **experimental**.
//!
//! # Read this before trusting anything in here
//!
//! Unlike playback (librespot, a decade of maintenance) and browsing (the
//! documented Web API), Jam has *no public API*. It runs over Spotify's
//! private `social-connect` service, reached through the same authenticated
//! `spclient` channel librespot already holds open, with live updates pushed
//! over the dealer websocket.
//!
//! Consequences you should design around:
//!
//! * Every endpoint path and JSON field below was derived from observing the
//!   official clients. Spotify can change them without notice, and has.
//! * Responses are parsed permissively — every field is optional — so a schema
//!   change degrades to missing data in the UI instead of a hard failure.
//! * Joining someone else's Jam is the better-understood half. Hosting is
//!   implemented but is the more likely of the two to break.
//!
//! Everything is confined to this crate on purpose: when it breaks, this is
//! the only file that needs revisiting, and the rest of the app is unaffected.

pub mod social;

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use http::Method;
use librespot_core::{
    dealer::protocol::{Message, PayloadValue},
    Session,
};
use serde::Deserialize;
use spotify_proto::{JamParticipant, JamState};
use tracing::{debug, warn};

/// Private endpoints. Grouped here so a breakage is a one-place fix.
mod endpoints {
    /// Returns the caller's current session, creating one if absent.
    pub const CURRENT_OR_NEW: &str = "/social-connect/v2/sessions/current_or_new";
    /// Read the current session without creating one.
    pub const CURRENT: &str = "/social-connect/v2/sessions/current";
    /// Join by token: append the join token.
    pub const JOIN: &str = "/social-connect/v2/sessions/join/";
    /// Leave the session the caller is currently in.
    pub const LEAVE: &str = "/social-connect/v2/sessions/leave";
    /// Dealer topic carrying live session updates.
    pub const DEALER_TOPIC: &str = "hm://social-connect/v2/sessions";
}

/// Wire shape of a social-connect session.
///
/// Every field is optional: this is an undocumented API and a missing key must
/// never be a parse error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "snake_case")]
struct RawSession {
    session_id: Option<String>,
    join_session_token: Option<String>,
    join_session_url: Option<String>,
    session_owner_id: Option<String>,
    #[serde(alias = "active")]
    is_session_owner: Option<bool>,
    session_members: Vec<RawMember>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "snake_case")]
struct RawMember {
    id: Option<String>,
    username: Option<String>,
    display_name: Option<String>,
    image_url: Option<String>,
    large_image_url: Option<String>,
    is_listening: Option<bool>,
    is_current_user: Option<bool>,
}

impl RawSession {
    fn into_state(self, self_user: &str) -> JamState {
        let owner = self.session_owner_id.clone().unwrap_or_default();
        let is_host = self
            .is_session_owner
            .unwrap_or_else(|| !owner.is_empty() && owner == self_user);

        let participants = self
            .session_members
            .into_iter()
            .map(|m| {
                let id = m.id.or(m.username).unwrap_or_default();
                JamParticipant {
                    is_host: !owner.is_empty() && id == owner,
                    is_listening: m.is_listening.unwrap_or(false),
                    display_name: m
                        .display_name
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| id.clone()),
                    image_url: m.large_image_url.or(m.image_url),
                    id,
                }
            })
            .collect::<Vec<_>>();

        // A session with an id but no members is a stale shell; treat it as
        // inactive so the UI does not show an empty Jam panel.
        let active = self.session_id.is_some() && !participants.is_empty();

        JamState {
            active,
            session_id: self.session_id,
            // Build the shareable link from the token. `join_session_url`
            // comes back as an internal `hm://social-connect/...` URI, which
            // is useless to a friend, so it is only used if it happens to be
            // a real web link.
            join_url: self
                .join_session_token
                .map(|t| format!("https://open.spotify.com/socialsession/{t}"))
                .or_else(|| {
                    self.join_session_url
                        .filter(|u| u.starts_with("https://"))
                }),
            is_host,
            participants,
        }
    }
}

pub struct JamClient {
    session: Session,
}

impl JamClient {
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    fn self_user(&self) -> String {
        self.session.username()
    }

    /// Issue a social-connect request and parse it permissively.
    async fn call(&self, method: Method, endpoint: &str) -> Result<RawSession> {
        let bytes = self
            .session
            .spclient()
            .request_as_json(&method, endpoint, None, None)
            .await
            .with_context(|| format!("social-connect request to {endpoint} failed"))?;

        // Log the raw body at trace level: when Spotify changes the schema,
        // this is the single most useful diagnostic.
        let text = String::from_utf8_lossy(&bytes);
        debug!(%endpoint, len = text.len(), body = %&text[..text.len().min(400)],
               "social-connect response");

        parse_session(&text)
            .with_context(|| format!("parsing social-connect response from {endpoint}"))
    }

    /// Read the current Jam without creating one.
    pub async fn current(&self) -> Result<JamState> {
        match self.call(Method::GET, endpoints::CURRENT).await {
            Ok(raw) => Ok(raw.into_state(&self.self_user())),
            // "Not in a session" is a normal state, not an error to surface.
            Err(e) => {
                debug!("no active jam: {e:#}");
                Ok(JamState::default())
            }
        }
    }

    /// Host a Jam on this device, returning the shareable link.
    ///
    /// `current_or_new` is a GET even though it creates a session — POST
    /// returns 405. That is not a typo: the service treats it as "fetch my
    /// session, making one if absent".
    pub async fn create(&self) -> Result<JamState> {
        let raw = self.call(Method::GET, endpoints::CURRENT_OR_NEW).await?;
        let state = raw.into_state(&self.self_user());
        if state.join_url.is_none() {
            warn!("jam created but no join link was returned; schema may have changed");
        }
        Ok(state)
    }

    /// Join an existing Jam from a share link or a bare join token.
    pub async fn join(&self, link: &str) -> Result<JamState> {
        let token = extract_join_token(link)
            .ok_or_else(|| anyhow!("could not find a Jam join token in {link:?}"))?;
        let endpoint = format!("{}{token}", endpoints::JOIN);
        let raw = self.call(Method::POST, &endpoint).await?;
        Ok(raw.into_state(&self.self_user()))
    }

    pub async fn leave(&self) -> Result<JamState> {
        // The response body is not useful here; success is the signal.
        let _ = self.call(Method::POST, endpoints::LEAVE).await;
        Ok(JamState::default())
    }

    /// Stream live Jam updates pushed over the dealer websocket.
    ///
    /// This is how participants joining and leaving reach the UI without
    /// polling. The stream ends if the dealer connection drops.
    pub fn subscribe(&self) -> Result<impl futures_util::Stream<Item = JamState> + Send> {
        let self_user = self.self_user();
        let stream = self
            .session
            .dealer()
            .listen_for(endpoints::DEALER_TOPIC, move |msg: Message| {
                Ok(message_to_state(&msg, &self_user))
            })
            .context("subscribing to the social-connect dealer topic")?;

        // Drop anything that failed to map, rather than killing the stream.
        Ok(stream.filter_map(|res| async move {
            match res {
                Ok(Some(state)) => Some(state),
                Ok(None) => None,
                Err(e) => {
                    warn!("ignoring malformed jam update: {e}");
                    None
                }
            }
        }))
    }
}

/// Pull the session object out of a response body.
///
/// The service has used both a bare object and a `{"session": {...}}` wrapper,
/// so accept either.
fn parse_session(text: &str) -> Result<RawSession> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    let inner = value.get("session").unwrap_or(&value);
    Ok(serde_json::from_value(inner.clone())?)
}

/// Decode a dealer payload into a state update, if it carries one.
///
/// librespot has already handled framing and gzip by this point, so the
/// payload is either decoded JSON or raw bytes we attempt to read as UTF-8.
fn message_to_state(msg: &Message, self_user: &str) -> Option<JamState> {
    let body: std::borrow::Cow<'_, str> = match &msg.payload {
        PayloadValue::Json(text) => std::borrow::Cow::Borrowed(text.as_str()),
        PayloadValue::Raw(bytes) => String::from_utf8_lossy(bytes),
        // Keep-alives and acks carry no body.
        PayloadValue::Empty => return None,
    };

    if body.trim().is_empty() {
        return None;
    }

    match parse_session(&body) {
        Ok(raw) => Some(raw.into_state(self_user)),
        Err(e) => {
            debug!("dealer payload was not a session object: {e}");
            None
        }
    }
}

/// Accept a full share link, a `spotify:socialsession:` URI, or a bare token.
fn extract_join_token(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix("spotify:socialsession:") {
        return Some(rest.to_string());
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let url = url::Url::parse(trimmed).ok()?;
        // .../socialsession/<token>
        let token = url
            .path_segments()?
            .filter(|s| !s.is_empty())
            .next_back()?
            .to_string();
        return (!token.is_empty()).then_some(token);
    }

    // Assume a bare token; reject anything with obvious URL punctuation so a
    // malformed paste produces a clear error instead of a confusing 404.
    (!trimmed.contains('/') && !trimmed.contains(' ')).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_tokens_come_out_of_every_accepted_input_shape() {
        assert_eq!(
            extract_join_token("https://open.spotify.com/socialsession/abc123"),
            Some("abc123".into())
        );
        assert_eq!(
            extract_join_token("spotify:socialsession:xyz789"),
            Some("xyz789".into())
        );
        assert_eq!(extract_join_token("  rawtoken  "), Some("rawtoken".into()));
        assert_eq!(extract_join_token(""), None);
        assert_eq!(extract_join_token("not a token"), None);
    }

    #[test]
    fn trailing_slashes_and_query_strings_do_not_break_extraction() {
        assert_eq!(
            extract_join_token("https://open.spotify.com/socialsession/tok/"),
            Some("tok".into())
        );
        assert_eq!(
            extract_join_token("https://open.spotify.com/socialsession/tok?si=1"),
            Some("tok".into())
        );
    }

    #[test]
    fn sessions_parse_from_both_bare_and_wrapped_bodies() {
        let bare = r#"{"session_id":"s1","session_owner_id":"me",
            "session_members":[{"id":"me","display_name":"Me","is_listening":true}]}"#;
        let state = parse_session(bare).unwrap().into_state("me");
        assert!(state.active);
        assert!(state.is_host);
        assert_eq!(state.participants.len(), 1);
        assert!(state.participants[0].is_host);

        let wrapped = format!(r#"{{"session":{bare}}}"#);
        let state2 = parse_session(&wrapped).unwrap().into_state("me");
        assert_eq!(state2.session_id, state.session_id);
    }

    #[test]
    fn unknown_fields_and_missing_keys_are_tolerated() {
        // The whole point: a schema change must degrade, not crash.
        let odd = r#"{"session_id":"s1","brand_new_field":42,"session_members":[]}"#;
        let state = parse_session(odd).unwrap().into_state("me");
        assert!(!state.active, "a memberless session is not a live jam");
        assert_eq!(state.session_id.as_deref(), Some("s1"));
    }

    /// Regression: the service returns an internal hm:// URI alongside the
    /// token. Handing that to a friend does nothing.
    #[test]
    fn an_internal_hm_url_never_becomes_the_share_link() {
        let raw = r#"{"session_id":"s","join_session_token":"tok",
            "join_session_url":"hm://social-connect/v2/sessions/join/tok",
            "session_members":[{"id":"a"}]}"#;
        let state = parse_session(raw).unwrap().into_state("a");
        assert_eq!(
            state.join_url.as_deref(),
            Some("https://open.spotify.com/socialsession/tok")
        );
    }

    #[test]
    fn a_join_link_is_synthesised_when_only_a_token_is_returned() {
        let raw = r#"{"session_id":"s","join_session_token":"tok",
            "session_members":[{"id":"a"}]}"#;
        let state = parse_session(raw).unwrap().into_state("a");
        assert_eq!(
            state.join_url.as_deref(),
            Some("https://open.spotify.com/socialsession/tok")
        );
    }
}
