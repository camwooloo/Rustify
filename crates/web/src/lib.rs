//! Spotify Web API access: catalogue browsing, library, and the Connect
//! device list.
//!
//! Playback itself never goes through here — that is librespot's job. This
//! layer answers "what can I play" and "where else could it play".

mod convert;

use anyhow::{anyhow, Context, Result};
use rspotify::{
    http::HttpError,
    model::{AlbumId, AlbumType, ArtistId, LibraryId, Market, PlaylistId, SearchType, TrackId},
    prelude::*,
    AuthCodeSpotify, ClientError, Token,
};
use spotify_proto as wire;
use std::{collections::HashSet, time::Duration as StdDuration};
use tracing::warn;

/// How long Spotify says to wait, if this error is a rate limit.
///
/// Spotify rate-limits per app over a rolling window and returns 429 with a
/// `Retry-After` header. Any client that browses at a normal pace will hit
/// this eventually, so it has to be handled rather than surfaced as a failure.
fn retry_after(err: &ClientError) -> Option<StdDuration> {
    let ClientError::Http(http) = err else {
        return None;
    };
    let HttpError::StatusCode(response) = &**http else {
        return None;
    };
    // 429; compared numerically to avoid depending on reqwest directly.
    if response.status().as_u16() != 429 {
        return None;
    }
    let secs = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        // Spotify does not always send the header; a few seconds is a sane
        // floor and the retry budget bounds the total wait anyway.
        .unwrap_or(3);
    Some(StdDuration::from_secs(secs))
}

/// Run a Web API call, waiting out short rate limits.
///
/// Long waits are reported rather than slept through, so the UI can say
/// something truthful instead of appearing to hang.
async fn retrying<T, F, Fut>(what: &str, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = rspotify::ClientResult<T>>,
{
    const MAX_ATTEMPTS: u32 = 3;
    const MAX_WAIT: StdDuration = StdDuration::from_secs(10);

    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                attempt += 1;
                match retry_after(&e) {
                    Some(wait) if attempt < MAX_ATTEMPTS && wait <= MAX_WAIT => {
                        warn!("{what}: rate limited, retrying in {}s", wait.as_secs());
                        tokio::time::sleep(wait).await;
                    }
                    Some(wait) => {
                        return Err(anyhow!(
                            "Spotify is rate limiting this account — try again in about {}s",
                            wait.as_secs().max(1)
                        ))
                    }
                    None => return Err(anyhow::Error::new(e).context(what.to_string())),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Hand-rolled models for /me/player
//
// Every field is optional and unknown keys are ignored, so Spotify adding or
// removing something cannot blank out the now-playing bar.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawPlayer {
    device: RawDevice,
    is_playing: bool,
    progress_ms: Option<u32>,
    shuffle_state: bool,
    item: Option<RawItem>,
    context: Option<RawContext>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawContext {
    uri: String,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawQueue {
    queue: Vec<RawItem>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawRecent {
    items: Vec<RawPlay>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawPlay {
    track: Option<RawItem>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawDevice {
    id: Option<String>,
    name: String,
    volume_percent: Option<u32>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawItem {
    id: Option<String>,
    uri: String,
    name: String,
    duration_ms: u32,
    explicit: bool,
    /// Present on tracks.
    artists: Vec<RawNamed>,
    album: Option<RawAlbum>,
    /// Present on podcast episodes instead of `artists`/`album`.
    show: Option<RawNamed>,
    images: Vec<RawImage>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawNamed {
    uri: String,
    name: String,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawAlbum {
    uri: String,
    name: String,
    images: Vec<RawImage>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RawImage {
    url: String,
    width: Option<u32>,
}

fn widest(images: &[RawImage]) -> Option<String> {
    images
        .iter()
        .max_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone())
}

impl RawItem {
    fn into_wire(self) -> wire::Track {
        // Episodes carry `show` and their own images; tracks carry artists
        // and an album. Handle both so podcasts do not blank the bar.
        let (artists, album, cover) = match self.show {
            Some(show) => (
                vec![wire::ArtistRef {
                    uri: show.uri.clone(),
                    name: show.name.clone(),
                }],
                None,
                widest(&self.images),
            ),
            None => {
                let cover = self.album.as_ref().and_then(|a| widest(&a.images));
                let album = self.album.map(|a| wire::AlbumRef {
                    uri: a.uri,
                    name: a.name,
                    cover_url: cover.clone(),
                });
                (
                    self.artists
                        .into_iter()
                        .map(|a| wire::ArtistRef {
                            uri: a.uri,
                            name: a.name,
                        })
                        .collect(),
                    album,
                    cover,
                )
            }
        };

        wire::Track {
            id: self.id.unwrap_or_default(),
            uri: self.uri,
            name: self.name,
            artists,
            album,
            duration_ms: self.duration_ms,
            explicit: self.explicit,
            saved: false,
            cover_url: cover,
            added_at: None,
        }
    }
}

/// Playback state as reported by the account, wherever it is happening.
#[derive(Debug, Clone)]
pub struct RemotePlayback {
    pub device_name: String,
    pub device_id: Option<String>,
    pub is_playing: bool,
    pub progress_ms: u32,
    pub volume_percent: Option<u32>,
    pub shuffle: bool,
    pub track: Option<wire::Track>,
}

pub struct WebClient {
    client: AuthCodeSpotify,
    /// Used for the endpoints we parse ourselves rather than via rspotify.
    http: reqwest::Client,
    /// Spotify removed `country` from the user profile, so relevance is
    /// resolved server-side from the access token instead.
    market: Option<Market>,
}

impl WebClient {
    /// Build a client from an already-obtained access token.
    ///
    /// The token comes from the same OAuth flow that authenticated the
    /// streaming session, so there is no second login.
    pub fn new(access_token: &str, refresh_token: &str, expires_in_secs: u64) -> Self {
        let token = Token {
            access_token: access_token.to_string(),
            expires_in: chrono::Duration::from_std(StdDuration::from_secs(expires_in_secs))
                .unwrap_or_else(|_| chrono::Duration::seconds(3600)),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::seconds(expires_in_secs as i64)),
            refresh_token: Some(refresh_token.to_string()),
            scopes: HashSet::new(),
        };
        Self {
            client: AuthCodeSpotify::from_token(token),
            http: reqwest::Client::new(),
            market: Some(Market::FromToken),
        }
    }

    /// Replace the access token after a refresh, keeping the same client.
    pub async fn update_token(&self, access_token: &str, expires_in_secs: u64) -> Result<()> {
        let mut guard = self
            .client
            .token
            .lock()
            .await
            // LockError carries no Display impl worth surfacing.
            .map_err(|_| anyhow::anyhow!("rspotify token lock was poisoned"))?;
        if let Some(token) = guard.as_mut() {
            token.access_token = access_token.to_string();
            token.expires_at =
                Some(chrono::Utc::now() + chrono::Duration::seconds(expires_in_secs as i64));
        }
        Ok(())
    }

    /// Fetch the display profile.
    ///
    /// `premium` is left false here on purpose: Spotify removed the `product`
    /// field from this endpoint. The daemon fills it in from the librespot
    /// session's user attributes, which remain authoritative.
    pub async fn load_profile(&mut self) -> Result<wire::AuthState> {
        let me = retrying("fetching user profile", || self.client.me()).await?;

        Ok(wire::AuthState {
            logged_in: true,
            username: Some(me.id.id().to_string()),
            display_name: me.display_name.clone(),
            avatar_url: me
                .images
                .as_ref()
                .and_then(|imgs| imgs.iter().max_by_key(|i| i.width.unwrap_or(0)))
                .map(|i| i.url.clone()),
            premium: false,
            // Both are decided by the daemon, which knows the token situation.
            browsing_ready: false,
            web_client_configured: false,
        })
    }

    // -- search ---------------------------------------------------------

    /// One round trip covering all four result categories.
    pub async fn search(&self, query: &str, limit: u32) -> Result<wire::SearchResults> {
        let results = retrying("searching", || {
            self.client.search_multiple(
                query,
                [
                    SearchType::Track,
                    SearchType::Album,
                    SearchType::Artist,
                    SearchType::Playlist,
                ],
                self.market,
                None,
                Some(limit.min(50)),
                Some(0),
            )
        })
        .await?;

        // Every category is optional; a query matching nothing in one bucket
        // simply omits it.
        Ok(wire::SearchResults {
            tracks: results
                .tracks
                .map(|p| p.items.iter().map(convert::full_track).collect())
                .unwrap_or_default(),
            albums: results
                .albums
                .map(|p| p.items.iter().map(convert::simplified_album).collect())
                .unwrap_or_default(),
            artists: results
                .artists
                .map(|p| p.items.iter().map(convert::full_artist).collect())
                .unwrap_or_default(),
            playlists: results
                .playlists
                .map(|p| p.items.iter().map(convert::simplified_playlist).collect())
                .unwrap_or_default(),
        })
    }

    // -- library --------------------------------------------------------

    pub async fn playlists(&self, offset: u32, limit: u32) -> Result<(Vec<wire::Playlist>, u32)> {
        let page = retrying("listing playlists", || {
            self.client
                .current_user_playlists_manual(Some(limit.min(50)), Some(offset))
        })
        .await?;
        Ok((
            page.items.iter().map(convert::simplified_playlist).collect(),
            page.total,
        ))
    }

    pub async fn playlist_tracks(
        &self,
        id: &str,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<wire::Track>, u32)> {
        let pid = PlaylistId::from_id_or_uri(id).context("parsing playlist id")?;
        let page = retrying("listing playlist tracks", || {
            self.client.playlist_items_manual(
                pid.clone(),
                None,
                self.market,
                Some(limit.min(50)),
                Some(offset),
            )
        })
        .await?;
        Ok((
            page.items.iter().filter_map(convert::playlist_item).collect(),
            page.total,
        ))
    }

    pub async fn saved_tracks(&self, offset: u32, limit: u32) -> Result<(Vec<wire::Track>, u32)> {
        let page = retrying("listing saved tracks", || {
            self.client
                .current_user_saved_tracks_manual(self.market, Some(limit.min(50)), Some(offset))
        })
        .await?;
        let tracks = page
            .items
            .iter()
            .map(|s| {
                let mut t = convert::full_track(&s.track);
                t.saved = true; // by definition, these are saved
                t
            })
            .collect();
        Ok((tracks, page.total))
    }

    pub async fn saved_albums(&self, offset: u32, limit: u32) -> Result<(Vec<wire::Album>, u32)> {
        let page = retrying("listing saved albums", || {
            self.client
                .current_user_saved_albums_manual(self.market, Some(limit.min(50)), Some(offset))
        })
        .await?;
        Ok((
            page.items.iter().map(|s| convert::full_album(&s.album)).collect(),
            page.total,
        ))
    }

    /// Mark the "liked" heart state for a batch of tracks.
    ///
    /// Only entries with a parseable track id are queried; episodes and local
    /// files are left at their default and skipped in lockstep so results
    /// cannot slide onto the wrong row.
    pub async fn annotate_saved(&self, tracks: &mut [wire::Track]) -> Result<()> {
        let ids: Vec<TrackId<'static>> = tracks
            .iter()
            .filter_map(|t| TrackId::from_id_or_uri(&t.uri).ok().map(|i| i.into_static()))
            .collect();
        if ids.is_empty() {
            return Ok(());
        }

        // The endpoint caps at 50 ids per call.
        let mut flags = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(50) {
            let got = self
                .client
                .library_contains(chunk.iter().cloned().map(LibraryId::Track))
                .await
                .context("checking saved state")?;
            flags.extend(got);
        }

        let mut it = flags.into_iter();
        for track in tracks.iter_mut() {
            if TrackId::from_id_or_uri(&track.uri).is_ok() {
                track.saved = it.next().unwrap_or(false);
            }
        }
        Ok(())
    }

    pub async fn set_saved(&self, uri: &str, saved: bool) -> Result<()> {
        let id = LibraryId::Track(TrackId::from_id_or_uri(uri).context("parsing track id")?);
        if saved {
            self.client.library_add([id]).await.context("saving track")?;
        } else {
            self.client
                .library_remove([id])
                .await
                .context("unsaving track")?;
        }
        Ok(())
    }

    // -- catalogue ------------------------------------------------------

    pub async fn album(&self, id: &str) -> Result<wire::Album> {
        let aid = AlbumId::from_id_or_uri(id).context("parsing album id")?;
        let album = retrying("fetching album", || {
            self.client.album(aid.clone(), self.market)
        })
        .await?;
        Ok(convert::full_album(&album))
    }

    /// Artist page: profile plus discography, fetched concurrently.
    ///
    /// There is no top-tracks section because Spotify withdrew that endpoint;
    /// `Artist::top_tracks` stays empty and the UI omits the block rather than
    /// rendering an empty shelf.
    pub async fn artist(&self, id: &str) -> Result<wire::Artist> {
        let aid = ArtistId::from_id_or_uri(id)
            .context("parsing artist id")?
            .into_static();

        let (profile, albums) = tokio::try_join!(
            self.client.artist(aid.clone()),
            self.client.artist_albums_manual(
                aid.clone(),
                [AlbumType::Album, AlbumType::Single],
                self.market,
                Some(50),
                Some(0),
            ),
        )
        .context("fetching artist page")?;

        let mut artist = convert::full_artist(&profile);
        artist.albums = albums.items.iter().map(convert::simplified_album).collect();

        // Top tracks work again on a grandfathered app; on a newly registered
        // one this 403s, so a failure just leaves the shelf out.
        #[derive(Default, serde::Deserialize)]
        #[serde(default)]
        struct TopTracks {
            tracks: Vec<RawItem>,
        }
        if let Ok(Some(top)) = self
            .raw_get::<TopTracks>(
                &format!("/artists/{}/top-tracks?market=from_token", artist.id),
                "reading artist top tracks",
            )
            .await
        {
            artist.top_tracks = top.tracks.into_iter().map(RawItem::into_wire).collect();
        }

        Ok(artist)
    }

    // -- connect --------------------------------------------------------

    /// Every Connect endpoint on the account, including this daemon.
    pub async fn devices(&self, self_name: &str) -> Result<Vec<wire::Device>> {
        let devices = retrying("listing devices", || self.client.device()).await?;
        Ok(devices
            .into_iter()
            .map(|d| wire::Device {
                is_self: d.name == self_name,
                id: d.id.unwrap_or_default(),
                name: d.name,
                device_type: format!("{:?}", d._type).to_lowercase(),
                is_active: d.is_active,
                volume_percent: d.volume_percent,
            })
            .collect())
    }

    /// The current access token, for the endpoints we parse ourselves.
    async fn access_token(&self) -> Result<String> {
        let guard = self
            .client
            .token
            .lock()
            .await
            .map_err(|_| anyhow!("rspotify token lock was poisoned"))?;
        guard
            .as_ref()
            .map(|t| t.access_token.clone())
            .ok_or_else(|| anyhow!("no access token available"))
    }

    /// GET a Web API path and deserialise it with our own models.
    ///
    /// Several player endpoints omit fields that rspotify's `FullTrack`
    /// requires (`external_ids`, `available_markets`, `popularity`). Because
    /// `PlayableItem` is `#[serde(untagged)]` with an `Unknown` fallback, that
    /// silently turns real tracks into unknowns — or hard-fails the whole
    /// response. Reading only the fields we use avoids both.
    async fn raw_get<T: serde::de::DeserializeOwned + Default>(
        &self,
        path: &str,
        what: &str,
    ) -> Result<Option<T>> {
        let token = self.access_token().await?;
        let response = self
            .http
            .get(format!("https://api.spotify.com/v1{path}"))
            .bearer_auth(token)
            .send()
            .await
            .with_context(|| what.to_string())?;

        // 204 means "nothing playing"; an empty body is not an error.
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(anyhow!("Spotify is rate limiting this account"));
        }
        if !response.status().is_success() {
            return Err(anyhow!("{what} failed: {}", response.status()));
        }

        Ok(Some(
            response
                .json::<T>()
                .await
                .with_context(|| format!("parsing {what}"))?,
        ))
    }

    /// Human name for a context URI, for the queue's "Next from" heading.
    pub async fn context_name(&self, uri: &str) -> Option<String> {
        let (_, kind, id) = {
            let mut parts = uri.split(':');
            (parts.next()?, parts.next()?.to_string(), parts.next()?.to_string())
        };

        match kind.as_str() {
            "playlist" => {
                let v: serde_json::Value = self
                    .raw_get(&format!("/playlists/{id}?fields=name"), "reading context")
                    .await
                    .ok()??;
                v.get("name").and_then(|n| n.as_str()).map(str::to_string)
            }
            "album" => self.album(&id).await.ok().map(|a| a.name),
            "artist" => self.artist(&id).await.ok().map(|a| format!("{} radio", a.name)),
            _ => None,
        }
    }

    /// What the account is playing right now, on whichever device owns it.
    ///
    /// This is what lets Rustify show your phone's track when the phone is the
    /// active device — it reflects the *account*, not this process.
    ///
    /// Parsed by hand rather than through rspotify's model. `PlayableItem` is
    /// `#[serde(untagged)]` with an `Unknown(Value)` fallback, so *any* field
    /// mismatch inside `FullTrack` silently degrades a perfectly good track
    /// into "unknown" — which showed up here as a playing device with a null
    /// track. Reading only the handful of fields we need makes that
    /// impossible.
    pub async fn current_playback(&self) -> Result<Option<RemotePlayback>> {
        let token = self.access_token().await?;

        let response = self
            .http
            .get("https://api.spotify.com/v1/me/player?additional_types=track,episode")
            .bearer_auth(token)
            .send()
            .await
            .context("reading current playback")?;

        // 204 is Spotify's "nothing is playing anywhere".
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(anyhow!("rate limited while reading playback"));
        }
        if !response.status().is_success() {
            return Err(anyhow!("playback request failed: {}", response.status()));
        }

        let raw: RawPlayer = response
            .json()
            .await
            .context("parsing the current playback response")?;

        Ok(Some(RemotePlayback {
            device_name: raw.device.name,
            device_id: raw.device.id,
            is_playing: raw.is_playing,
            progress_ms: raw.progress_ms.unwrap_or(0),
            volume_percent: raw.device.volume_percent,
            shuffle: raw.shuffle_state,
            track: raw.item.map(|i| i.into_wire()),
        }))
    }

    /// Recently played tracks, newest first, de-duplicated.
    ///
    /// Spotify returns one entry per play, so a track on repeat would
    /// otherwise fill the whole shelf.
    pub async fn recently_played(&self, limit: u32) -> Result<Vec<wire::Track>> {
        let raw: Option<RawRecent> = self
            .raw_get(
                &format!("/me/player/recently-played?limit={}", limit.min(50)),
                "reading recently played",
            )
            .await?;

        let mut seen = HashSet::new();
        Ok(raw
            .unwrap_or_default()
            .items
            .into_iter()
            .filter_map(|p| p.track)
            .map(RawItem::into_wire)
            .filter(|t| seen.insert(t.uri.clone()))
            .collect())
    }

    pub async fn top_artists(&self, limit: u32) -> Result<Vec<wire::Artist>> {
        let page = retrying("reading top artists", || {
            self.client.current_user_top_artists_manual(
                Some(rspotify::model::TimeRange::MediumTerm),
                Some(limit.min(50)),
                Some(0),
            )
        })
        .await?;
        Ok(page.items.iter().map(convert::full_artist).collect())
    }

    pub async fn top_tracks(&self, limit: u32) -> Result<Vec<wire::Track>> {
        let page = retrying("reading top tracks", || {
            self.client.current_user_top_tracks_manual(
                Some(rspotify::model::TimeRange::MediumTerm),
                Some(limit.min(50)),
                Some(0),
            )
        })
        .await?;
        Ok(page.items.iter().map(convert::full_track).collect())
    }

    /// Name and artwork for a playlist, by uri or id.
    pub async fn playlist_meta(&self, uri: &str) -> Result<Option<wire::Playlist>> {
        #[derive(Default, serde::Deserialize)]
        #[serde(default)]
        struct RawPlaylist {
            id: String,
            uri: String,
            name: String,
            images: Vec<RawImage>,
        }

        let id = uri.rsplit(':').next().unwrap_or(uri);
        let raw: Option<RawPlaylist> = self
            .raw_get(
                &format!("/playlists/{id}?fields=id,uri,name,images"),
                "reading playlist",
            )
            .await?;

        Ok(raw.map(|p| wire::Playlist {
            cover_url: widest(&p.images),
            id: p.id,
            uri: p.uri,
            name: p.name,
            owner: String::new(),
            description: None,
            total_tracks: 0,
        }))
    }

    /// Recently released albums from Spotify's browse feed.
    ///
    /// Only reachable on apps registered before Spotify's November 2024 cull;
    /// newer ones get 403. Callers treat failure as "hide the shelf".
    pub async fn new_releases(&self, limit: u32) -> Result<Vec<wire::Album>> {
        #[derive(Default, serde::Deserialize)]
        #[serde(default)]
        struct Envelope {
            albums: Page,
        }
        #[derive(Default, serde::Deserialize)]
        #[serde(default)]
        struct Page {
            items: Vec<RawAlbumItem>,
        }
        #[derive(Default, serde::Deserialize)]
        #[serde(default)]
        struct RawAlbumItem {
            id: String,
            uri: String,
            name: String,
            release_date: Option<String>,
            total_tracks: u32,
            images: Vec<RawImage>,
            artists: Vec<RawNamed>,
        }

        let raw: Option<Envelope> = self
            .raw_get(
                &format!("/browse/new-releases?limit={}", limit.min(50)),
                "reading new releases",
            )
            .await?;

        Ok(raw
            .unwrap_or_default()
            .albums
            .items
            .into_iter()
            .map(|a| wire::Album {
                cover_url: widest(&a.images),
                artists: a
                    .artists
                    .into_iter()
                    .map(|n| wire::ArtistRef {
                        uri: n.uri,
                        name: n.name,
                    })
                    .collect(),
                id: a.id,
                uri: a.uri,
                name: a.name,
                release_date: a.release_date,
                total_tracks: a.total_tracks,
                tracks: Vec::new(),
            })
            .collect())
    }

    /// The genre grid.
    pub async fn categories(&self) -> Result<Vec<wire::Category>> {
        #[derive(Default, serde::Deserialize)]
        #[serde(default)]
        struct Envelope {
            categories: Page,
        }
        #[derive(Default, serde::Deserialize)]
        #[serde(default)]
        struct Page {
            items: Vec<RawCategory>,
        }
        #[derive(Default, serde::Deserialize)]
        #[serde(default)]
        struct RawCategory {
            id: String,
            name: String,
            icons: Vec<RawImage>,
        }

        let raw: Option<Envelope> = self
            .raw_get("/browse/categories?limit=50", "reading categories")
            .await?;

        Ok(raw
            .unwrap_or_default()
            .categories
            .items
            .into_iter()
            .map(|c| wire::Category {
                icon_url: widest(&c.icons),
                id: c.id,
                name: c.name,
            })
            .collect())
    }

    /// The account's up-next list, plus the context it is playing from.
    pub async fn queue(&self) -> Result<(Vec<wire::Track>, Option<String>)> {
        let raw: Option<RawQueue> = self.raw_get("/me/player/queue", "reading the queue").await?;
        let items = raw
            .unwrap_or_default()
            .queue
            .into_iter()
            .map(RawItem::into_wire)
            .collect();

        // The Web API does not distinguish user-queued items from the ones
        // coming next in the context, so we label the section with the
        // context name rather than inventing Spotify's two-section split.
        let context = match self
            .raw_get::<RawPlayer>("/me/player", "reading playback context")
            .await
        {
            Ok(Some(p)) => match p.context {
                Some(c) if !c.uri.is_empty() => self.context_name(&c.uri).await,
                _ => None,
            },
            _ => None,
        };

        Ok((items, context))
    }

    /// Append a track to whatever device is currently active.
    pub async fn add_to_queue(&self, uri: &str) -> Result<()> {
        let id = TrackId::from_id_or_uri(uri)
            .context("parsing track id")?
            .into_static();
        retrying("adding to the queue", || {
            self.client.add_item_to_queue(rspotify::model::PlayableId::Track(id.clone()), None)
        })
        .await
    }

    pub async fn transfer_playback(&self, device_id: &str, play: bool) -> Result<()> {
        self.client
            .transfer_playback(device_id, Some(play))
            .await
            .context("transferring playback")
    }
}
