//! OAuth 2.0 authorization-code flow with PKCE.
//!
//! We drive this ourselves rather than using `librespot-oauth` because that
//! crate opens the browser from inside the token call and never surfaces the
//! authorization URL. A desktop UI needs the URL up front so it can show a
//! "waiting for your browser" state with a copyable fallback link.
//!
//! The resulting access token is used for *both* the librespot streaming
//! session and the Spotify Web API, so there is exactly one login.

use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use oauth2::{
    basic::BasicClient, AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge,
    RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tracing::{debug, warn};

/// Spotify's own desktop ("keymaster") client id, as used by librespot.
///
/// Required for the streaming session: a self-registered Developer Dashboard
/// app cannot request the scopes librespot needs against the internal access
/// points.
///
/// It must **not** be used for `api.spotify.com`. Spotify rate-limits the Web
/// API per client id, and this id is shared by every librespot-derived project
/// in existence, so its budget is permanently exhausted — requests return 429
/// immediately regardless of how little this machine has asked for. Browsing
/// therefore uses a separate, user-registered client id.
pub const CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";

/// Scopes for the streaming session. Several are keymaster-only.
const STREAMING_SCOPES: &[&str] = SCOPES;

/// Scopes for a user-registered app. Restricted to documented Web API scopes,
/// since custom clients cannot be granted the internal ones.
const WEB_SCOPES: &[&str] = &[
    "playlist-modify-private",
    "playlist-modify-public",
    "playlist-read-collaborative",
    "playlist-read-private",
    "user-library-modify",
    "user-library-read",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-read-email",
    "user-read-playback-position",
    "user-read-playback-state",
    "user-read-private",
    "user-read-recently-played",
    "user-top-read",
];

/// Which OAuth identity a flow is for. The two are cached separately so
/// signing in for one never invalidates the other.
#[derive(Debug, Clone)]
pub struct Profile {
    pub client_id: String,
    pub scopes: Vec<String>,
    pub cache_file: &'static str,
    pub label: &'static str,
}

impl Profile {
    /// The librespot streaming session.
    pub fn streaming() -> Self {
        Self {
            client_id: CLIENT_ID.to_string(),
            scopes: STREAMING_SCOPES.iter().map(|s| s.to_string()).collect(),
            cache_file: "token.json",
            label: "streaming",
        }
    }

    /// The Web API, using the user's own registered app.
    pub fn web(client_id: String) -> Self {
        Self {
            client_id,
            scopes: WEB_SCOPES.iter().map(|s| s.to_string()).collect(),
            cache_file: "token-web.json",
            label: "browsing",
        }
    }
}

const AUTH_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

/// Fixed loopback port keeps the redirect URI stable across runs.
pub const CALLBACK_PORT: u16 = 4382;

/// Superset of what playback and the Web API layer need.
pub const SCOPES: &[&str] = &[
    "app-remote-control",
    "playlist-modify-private",
    "playlist-modify-public",
    "playlist-read-collaborative",
    "playlist-read-private",
    "streaming",
    "user-follow-modify",
    "user-follow-read",
    "user-library-modify",
    "user-library-read",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-read-email",
    "user-read-playback-position",
    "user-read-playback-state",
    "user-read-private",
    "user-read-recently-played",
    "user-top-read",
];

/// Persisted across restarts so you log in once, not once per launch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds. `Instant` is not portable across process restarts.
    pub expires_at_unix: u64,
}

impl StoredToken {
    /// Treat a token expiring within 60s as already expired, so we never race
    /// a refresh against an in-flight request.
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.expires_at_unix.saturating_sub(60) <= now
    }
}

pub fn config_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "spotify-rust", "spotify-rust")
        .ok_or_else(|| anyhow!("could not determine a config directory"))?;
    let dir = dirs.config_dir().to_path_buf();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating config dir {}", dir.display()))?;
    Ok(dir)
}

fn cache_path(profile: &Profile) -> Result<PathBuf> {
    Ok(config_dir()?.join(profile.cache_file))
}

pub fn load_cached_token(profile: &Profile) -> Option<StoredToken> {
    let path = cache_path(profile).ok()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&raw) {
        Ok(t) => Some(t),
        Err(e) => {
            warn!("discarding unreadable token cache at {}: {e}", path.display());
            None
        }
    }
}

pub fn save_token(profile: &Profile, token: &StoredToken) -> Result<()> {
    let path = cache_path(profile)?;
    let raw = serde_json::to_string_pretty(token)?;
    std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Remove every cached token. Used by sign-out.
pub fn clear_token() -> Result<()> {
    for profile in [Profile::streaming(), Profile::web(String::new())] {
        let path = cache_path(&profile)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

type SpotifyClient = BasicClient<
    oauth2::EndpointSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointSet,
>;

fn client(profile: &Profile) -> Result<SpotifyClient> {
    Ok(BasicClient::new(ClientId::new(profile.client_id.clone()))
        .set_auth_uri(AuthUrl::new(AUTH_URL.to_string())?)
        .set_token_uri(TokenUrl::new(TOKEN_URL.to_string())?)
        .set_redirect_uri(RedirectUrl::new(format!(
            "http://127.0.0.1:{CALLBACK_PORT}/login"
        ))?))
}

fn http_client() -> Result<reqwest::Client> {
    // The token endpoint must not be followed through redirects; that would be
    // an SSRF-shaped footgun in an OAuth client.
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building OAuth HTTP client")
}

fn expires_at_unix(expires_in: Option<Duration>) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now + expires_in.unwrap_or(Duration::from_secs(3600)).as_secs()
}

/// A login in progress: the URL to visit, plus the pending server-side half.
pub struct PendingLogin {
    /// Show or open this. It is the real authorize URL, PKCE challenge included.
    pub auth_url: String,
    listener: TcpListener,
    csrf: CsrfToken,
    verifier: oauth2::PkceCodeVerifier,
    client: SpotifyClient,
    profile: Profile,
}

/// Bind the callback listener and build the authorize URL, but do not block.
///
/// Binding here rather than inside [`PendingLogin::wait`] means a port clash
/// (a second instance mid-login) is reported immediately instead of after the
/// user has already been sent to their browser.
pub async fn begin_login(profile: Profile) -> Result<PendingLogin> {
    let client = client(&profile)?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, csrf) = client
        .authorize_url(CsrfToken::new_random)
        .add_scopes(profile.scopes.iter().map(|s| Scope::new(s.clone())))
        .set_pkce_challenge(challenge)
        .url();

    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .with_context(|| format!("binding OAuth callback listener on port {CALLBACK_PORT}"))?;

    Ok(PendingLogin {
        auth_url: url.to_string(),
        listener,
        csrf,
        verifier,
        client,
        profile,
    })
}

impl PendingLogin {
    /// Wait for the browser redirect, then exchange the code for a token.
    ///
    /// Times out so a user who abandons the flow does not leave the port bound
    /// and the daemon waiting forever.
    pub async fn wait(self, timeout: Duration) -> Result<StoredToken> {
        let code = tokio::time::timeout(timeout, accept_code(&self.listener, &self.csrf))
            .await
            .map_err(|_| anyhow!("timed out waiting for the browser to complete login"))??;

        let http = http_client()?;
        let resp = self
            .client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(self.verifier)
            .request_async(&http)
            .await
            .context("exchanging authorization code for a token")?;

        let token = StoredToken {
            access_token: resp.access_token().secret().to_string(),
            refresh_token: resp
                .refresh_token()
                .map(|t| t.secret().to_string())
                .unwrap_or_default(),
            expires_at_unix: expires_at_unix(resp.expires_in()),
        };
        save_token(&self.profile, &token)?;
        Ok(token)
    }
}

/// Exchange a refresh token for a fresh access token.
///
/// Spotify may or may not rotate the refresh token; when it does not, we keep
/// the existing one rather than storing an empty string.
pub async fn refresh(profile: &Profile, stored: &StoredToken) -> Result<StoredToken> {
    if stored.refresh_token.is_empty() {
        return Err(anyhow!("no refresh token stored; interactive login required"));
    }
    let client = client(profile)?;
    let http = http_client()?;
    let resp = client
        .exchange_refresh_token(&RefreshToken::new(stored.refresh_token.clone()))
        .request_async(&http)
        .await
        .context("refreshing access token")?;

    let token = StoredToken {
        access_token: resp.access_token().secret().to_string(),
        refresh_token: resp
            .refresh_token()
            .map(|t| t.secret().to_string())
            .unwrap_or_else(|| stored.refresh_token.clone()),
        expires_at_unix: expires_at_unix(resp.expires_in()),
    };
    save_token(profile, &token)?;
    debug!("{} access token refreshed", profile.label);
    Ok(token)
}

/// Return a valid access token, refreshing if needed. `None` means the user
/// must log in interactively.
pub async fn current_token(profile: &Profile) -> Option<StoredToken> {
    let stored = load_cached_token(profile)?;
    if !stored.is_expired() {
        return Some(stored);
    }
    match refresh(profile, &stored).await {
        Ok(t) => Some(t),
        Err(e) => {
            warn!("token refresh failed, interactive login required: {e:#}");
            None
        }
    }
}

/// Accept exactly one loopback request and pull the `code` out of it.
async fn accept_code(listener: &TcpListener, csrf: &CsrfToken) -> Result<String> {
    loop {
        let (stream, _) = listener.accept().await?;
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await? == 0 {
            continue;
        }

        // "GET /login?code=...&state=... HTTP/1.1"
        let target = request_line.split_whitespace().nth(1).unwrap_or_default();

        // Browsers reliably request /favicon.ico alongside; ignore anything
        // that is not the redirect itself.
        if !target.starts_with("/login") {
            let mut stream = reader.into_inner();
            let _ = stream.write_all(http_response(404, "Not found").as_bytes()).await;
            let _ = stream.shutdown().await;
            continue;
        }

        let url = url::Url::parse(&format!("http://127.0.0.1{target}"))
            .context("parsing OAuth redirect URL")?;
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();

        let mut stream = reader.into_inner();

        if let Some(err) = params.get("error") {
            let body = http_response(400, "Login was denied. You can close this tab.");
            let _ = stream.write_all(body.as_bytes()).await;
            let _ = stream.shutdown().await;
            return Err(anyhow!("authorization denied: {err}"));
        }

        // CSRF check: a mismatched state means this redirect is not ours.
        //
        // Reject it but keep listening rather than aborting. A stale tab from
        // an earlier attempt is the common cause, and killing the live flow
        // over it would strand the user. It also means a bogus request cannot
        // be used to deny service to a legitimate login.
        match params.get("state") {
            Some(state) if state == csrf.secret() => {}
            _ => {
                warn!("ignoring OAuth redirect with a mismatched state (stale tab?)");
                let body = http_response(
                    400,
                    "This sign-in link has expired. Click Log in again in the app.",
                );
                let _ = stream.write_all(body.as_bytes()).await;
                let _ = stream.shutdown().await;
                continue;
            }
        }

        let code = params
            .get("code")
            .cloned()
            .ok_or_else(|| anyhow!("redirect carried no authorization code"))?;

        let body = http_response(200, "Signed in. You can close this tab and return to the app.");
        let _ = stream.write_all(body.as_bytes()).await;
        let _ = stream.shutdown().await;
        return Ok(code);
    }
}

fn http_response(status: u16, message: &str) -> String {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Rustify</title>\
         <body style=\"background:#121212;color:#fff;font:16px/1.5 system-ui;\
         display:grid;place-items:center;height:100vh;margin:0\">\
         <p>{message}</p></body>"
    );
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_expiring_within_the_skew_window_counts_as_expired() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let almost = StoredToken {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at_unix: now + 30, // inside the 60s guard
        };
        assert!(almost.is_expired());

        let fresh = StoredToken {
            expires_at_unix: now + 3600,
            ..almost.clone()
        };
        assert!(!fresh.is_expired());
    }

    #[test]
    fn scopes_cover_both_streaming_and_library_reads() {
        assert!(SCOPES.contains(&"streaming"));
        assert!(SCOPES.contains(&"user-library-read"));
        assert!(SCOPES.contains(&"user-modify-playback-state"));
    }
}
