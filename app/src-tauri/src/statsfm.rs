//! stats.fm listening history.
//!
//! stats.fm records a Spotify account's streams over years and answers for
//! them publicly: minutes played, top tracks, artists and albums by range,
//! and the last things heard. Rustify shows that on its own page rather than
//! sending people to a browser, and every row carries the Spotify id the API
//! hands back, so anything on the page can be played from it.
//!
//! No credentials are involved. These are the public endpoints behind a
//! stats.fm profile page, and a profile that has its stats set to private
//! simply refuses them — which is reported as an empty section rather than
//! worked around.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

const API: &str = "https://api.stats.fm/api/v1";
const UA: &str = "Rustify";

/// What a range means to stats.fm. Anything else is refused by the API.
fn valid_range(range: &str) -> &str {
    match range {
        "weeks" | "months" | "lifetime" => range,
        _ => "weeks",
    }
}

#[derive(Debug, Serialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub image: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Entry {
    /// Spotify uri, when stats.fm knows one. Absent for anything that has no
    /// Spotify counterpart, which is why playing from this page is offered
    /// per row rather than for the whole list.
    pub uri: Option<String>,
    pub name: String,
    pub sub: String,
    pub image: Option<String>,
    pub streams: u64,
    pub minutes: u64,
}

#[derive(Debug, Serialize)]
pub struct Overview {
    pub account: Account,
    pub range: String,
    pub minutes: u64,
    pub streams: u64,
    pub tracks: Vec<Entry>,
    pub artists: Vec<Entry>,
    pub albums: Vec<Entry>,
    pub recent: Vec<Entry>,
    /// Sections the profile keeps private, named so the page can say which
    /// rather than showing an unexplained gap.
    pub private: Vec<String>,
}

#[derive(Deserialize)]
struct Wrapper<T> {
    item: T,
}

#[derive(Deserialize)]
struct Items<T> {
    items: T,
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .context("building the stats.fm client")
}

/// Walk `path`, stepping into the first element of any array on the way.
///
/// stats.fm nests artwork under a list of albums and ids under a list per
/// service, so a path crosses arrays it does not name. An array is entered
/// rather than consumed: the key still has to be applied to what is inside.
fn dig<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = value;
    for key in path {
        if let Value::Array(items) = cur {
            cur = items.first()?;
        }
        cur = cur.get(key)?;
    }
    Some(cur)
}

fn text(value: &Value, path: &[&str]) -> Option<String> {
    dig(value, path)?.as_str().map(str::to_string)
}

/// Turn one API row into a page row.
///
/// `kind` is the key holding the thing itself — stats.fm nests it under
/// `track`, `artist` or `album` depending on which list this is.
fn entry(row: &Value, kind: &str) -> Option<Entry> {
    let item = row.get(kind).unwrap_or(row);

    let name = text(item, &["name"])?;
    let spotify_id = dig(item, &["externalIds", "spotify"])
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str());

    let prefix = match kind {
        "artist" => "artist",
        "album" => "album",
        _ => "track",
    };

    // Artists carry their own image; tracks and albums carry artwork on the
    // album, and a track's album is a list.
    let image = text(item, &["image"]).or_else(|| text(item, &["albums", "image"]));

    let sub = match kind {
        "artist" => dig(item, &["genres"])
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("Artist")
            .to_string(),
        _ => dig(item, &["artists"])
            .and_then(|v| v.as_array())
            .map(|artists| {
                artists
                    .iter()
                    .filter_map(|a| a.get("name").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Album".to_string()),
    };

    let played_ms = row.get("playedMs").and_then(Value::as_u64).unwrap_or(0);

    Some(Entry {
        uri: spotify_id.map(|id| format!("spotify:{prefix}:{id}")),
        name,
        sub,
        image,
        streams: row.get("streams").and_then(Value::as_u64).unwrap_or(0),
        minutes: played_ms / 60_000,
    })
}

async fn list(
    client: &reqwest::Client,
    user: &str,
    path: &str,
    kind: &str,
    limit: usize,
) -> std::result::Result<Vec<Entry>, ()> {
    let url = format!("{API}/users/{user}/{path}&limit={limit}");
    let response = client.get(&url).send().await.map_err(|_| ())?;

    // A private section answers 403. That is an answer, not a failure.
    if !response.status().is_success() {
        debug!(%url, status = %response.status(), "stats.fm refused a section");
        return Err(());
    }

    let body: Items<Vec<Value>> = response.json().await.map_err(|_| ())?;
    Ok(body.items.iter().filter_map(|row| entry(row, kind)).collect())
}

/// Look up profiles by name, so nobody has to know their own stats.fm id.
pub async fn search(query: &str) -> Result<Vec<Account>> {
    #[derive(Deserialize)]
    struct Users {
        #[serde(default)]
        users: Vec<Value>,
    }

    let client = client()?;
    let url = format!("{API}/search?query={}&type=user&limit=8", urlencode(query));
    let body: Items<Users> = client
        .get(&url)
        .send()
        .await
        .context("searching stats.fm")?
        .error_for_status()
        .context("stats.fm refused the search")?
        .json()
        .await
        .context("reading the search results")?;

    Ok(body
        .items
        .users
        .iter()
        .filter_map(|u| {
            Some(Account {
                id: text(u, &["customId"]).or_else(|| text(u, &["id"]))?,
                name: text(u, &["displayName"]).unwrap_or_else(|| "Unknown".into()),
                image: text(u, &["image"]),
            })
        })
        .collect())
}

/// Everything the stats page shows, in one round of requests.
pub async fn overview(user: &str, range: &str) -> Result<Overview> {
    let range = valid_range(range).to_string();
    let client = client()?;

    let profile: Wrapper<Value> = client
        .get(format!("{API}/users/{user}"))
        .send()
        .await
        .context("asking stats.fm for the profile")?
        .error_for_status()
        .context("no stats.fm profile by that name")?
        .json()
        .await
        .context("reading the profile")?;

    let account = Account {
        id: text(&profile.item, &["customId"])
            .or_else(|| text(&profile.item, &["id"]))
            .unwrap_or_else(|| user.to_string()),
        name: text(&profile.item, &["displayName"]).unwrap_or_else(|| user.to_string()),
        image: text(&profile.item, &["image"]),
    };

    let stats = client
        .get(format!("{API}/users/{user}/streams/stats?range={range}"))
        .send()
        .await
        .ok();

    let (minutes, streams) = match stats {
        Some(r) if r.status().is_success() => {
            let body: Items<Value> = r.json().await.unwrap_or(Items { items: Value::Null });
            (
                body.items
                    .get("durationMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    / 60_000,
                body.items.get("count").and_then(Value::as_u64).unwrap_or(0),
            )
        }
        _ => (0, 0),
    };

    // Bound outside the join: a temporary formatted inside it would not live
    // as long as the future that borrows it.
    let (top_tracks, top_artists, top_albums) = (
        format!("top/tracks?range={range}"),
        format!("top/artists?range={range}"),
        format!("top/albums?range={range}"),
    );

    let (tracks, artists, albums, recent) = tokio::join!(
        list(&client, user, &top_tracks, "track", 10),
        list(&client, user, &top_artists, "artist", 10),
        list(&client, user, &top_albums, "album", 10),
        list(&client, user, "streams/recent?", "track", 12),
    );

    let mut private = Vec::new();
    let mut section = |name: &str, r: std::result::Result<Vec<Entry>, ()>| match r {
        Ok(items) => items,
        Err(()) => {
            private.push(name.to_string());
            Vec::new()
        }
    };

    let tracks = section("tracks", tracks);
    let artists = section("artists", artists);
    let albums = section("albums", albums);
    let recent = section("recent", recent);

    Ok(Overview {
        account,
        range,
        minutes,
        streams,
        tracks,
        artists,
        albums,
        recent,
        private,
    })
}

/// Percent-encode a query. Names contain spaces and the odd symbol, and this
/// is the only user text that reaches a URL here.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{entry, urlencode, valid_range};
    use serde_json::json;

    #[test]
    fn ranges_outside_the_three_fall_back() {
        assert_eq!(valid_range("lifetime"), "lifetime");
        assert_eq!(valid_range("../../etc"), "weeks");
    }

    #[test]
    fn query_text_is_encoded() {
        assert_eq!(urlencode("cam wooloo"), "cam+wooloo");
        assert_eq!(urlencode("a/b?c"), "a%2Fb%3Fc");
    }

    #[test]
    fn a_top_track_row_becomes_a_playable_entry() {
        let row = json!({
            "streams": 12,
            "playedMs": 2_676_156,
            "track": {
                "name": "Church",
                "externalIds": { "spotify": ["4cOdK2wGLETKBW3PvgPWqQ"] },
                "albums": [{ "image": "https://i.scdn.co/image/abc" }],
                "artists": [{ "name": "Chase Atlantic" }, { "name": "Guest" }],
            }
        });

        let e = entry(&row, "track").expect("a track entry");
        assert_eq!(e.name, "Church");
        assert_eq!(e.sub, "Chase Atlantic, Guest");
        assert_eq!(e.uri.as_deref(), Some("spotify:track:4cOdK2wGLETKBW3PvgPWqQ"));
        assert_eq!(e.minutes, 44);
        assert_eq!(e.image.as_deref(), Some("https://i.scdn.co/image/abc"));
    }

    /// Against the live API, so the shape this parses is the shape it is
    /// served — the rest of these tests only prove it is self-consistent.
    ///
    /// Ignored by default: the suite should not fail because a network is
    /// missing. Run with `cargo test -p spotify-rust-app -- --ignored`.
    #[tokio::test]
    #[ignore = "hits the stats.fm API"]
    async fn a_public_profile_reads_end_to_end() {
        // stats.fm's own founder, whose profile is public by design.
        let view = super::overview("sjoerdgaatwakawaka", "lifetime")
            .await
            .expect("a profile");

        assert!(view.minutes > 0, "lifetime minutes should not be zero");
        assert!(!view.tracks.is_empty(), "a public profile has top tracks");

        let top = &view.tracks[0];
        assert!(!top.name.is_empty());
        assert!(
            top.uri.as_deref().is_some_and(|u| u.starts_with("spotify:track:")),
            "a top track should be playable, got {:?}",
            top.uri
        );
    }

    #[tokio::test]
    #[ignore = "hits the stats.fm API"]
    async fn search_finds_profiles_by_name() {
        let found = super::search("sjoerd").await.expect("results");
        assert!(!found.is_empty());
        assert!(found.iter().all(|a| !a.id.is_empty()));
    }

    #[test]
    fn a_row_with_no_spotify_id_is_still_shown() {
        let row = json!({ "streams": 3, "artist": { "name": "Someone", "genres": ["pop"] } });
        let e = entry(&row, "artist").expect("an artist entry");
        assert_eq!(e.sub, "pop");
        assert!(e.uri.is_none());
    }
}
