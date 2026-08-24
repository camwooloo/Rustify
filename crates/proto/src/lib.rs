//! Wire protocol shared by the daemon and every front-end.
//!
//! Transport is newline-delimited JSON over loopback TCP. That choice is
//! deliberate: it is trivially debuggable (`nc 127.0.0.1 4381`), has no
//! platform-specific behaviour to get wrong on Windows, and keeps the UI a
//! genuinely optional process — which is the whole point of this design.
//!
//! Field naming is camelCase throughout so the JSON drops straight into the
//! webview without a translation layer.

use serde::{Deserialize, Serialize};

/// Loopback port the daemon listens on.
pub const DEFAULT_PORT: u16 = 4381;

/// Bumped whenever the protocol changes shape. The daemon refuses clients that
/// disagree, so a stale UI fails loudly instead of misbehaving subtly.
pub const PROTOCOL_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// Client -> daemon
// ---------------------------------------------------------------------------

/// A request envelope.
///
/// The command is **nested**, not flattened. Flattening put the envelope's
/// `id` in the same object as the command's fields, so a command carrying its
/// own `id` (a playlist, album or artist id) collided with the correlation id
/// and was silently overwritten. Nesting makes that class of bug impossible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Correlates a reply with its request. Events carry no id.
    pub id: u64,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum Command {
    // -- lifecycle ------------------------------------------------------
    Hello { protocol: u32 },
    GetState,
    Shutdown,
    /// Tell the daemon whether a window is actually on screen. Polling
    /// Spotify for remote playback is pointless when nobody can see it.
    SetUiVisible { visible: bool },

    // -- auth -----------------------------------------------------------
    /// Kick off the interactive OAuth PKCE flow. Replies immediately with the
    /// URL to open; completion arrives later as an `AuthChanged` event.
    Login,
    Logout,
    GetSettings,
    #[serde(rename_all = "camelCase")]
    SetSettings(Settings),
    /// Delete the on-disk audio cache.
    ClearCache,
    /// Open a link in the system browser. The webview cannot navigate away
    /// from the app, and its CSP forbids external origins.
    OpenExternal { url: String },

    /// Register the user's own Spotify app id for Web API access, then start
    /// the second OAuth flow that browsing needs.
    #[serde(rename_all = "camelCase")]
    SetWebClientId { client_id: String },

    // -- transport ------------------------------------------------------
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    #[serde(rename_all = "camelCase")]
    Seek { position_ms: u32 },
    /// 0..=65535, matching librespot's internal volume range.
    SetVolume { volume: u16 },
    SetShuffle { enabled: bool },
    SetRepeat { mode: RepeatMode },

    /// Play an album/playlist/artist URI, optionally starting at an index.
    #[serde(rename_all = "camelCase")]
    LoadContext {
        uri: String,
        #[serde(default = "default_true")]
        start_playing: bool,
        #[serde(default)]
        index: Option<u32>,
        #[serde(default)]
        shuffle: bool,
    },
    /// Play a bare list of track URIs with no surrounding context.
    #[serde(rename_all = "camelCase")]
    LoadTracks {
        uris: Vec<String>,
        #[serde(default = "default_true")]
        start_playing: bool,
        /// Where in `uris` to start. Without it the list plays from the top,
        /// which is wrong for a list someone clicked halfway down.
        #[serde(default)]
        index: Option<u32>,
    },

    // -- web api --------------------------------------------------------
    #[serde(rename_all = "camelCase")]
    Search {
        query: String,
        #[serde(default = "default_limit")]
        limit: u32,
    },
    #[serde(rename_all = "camelCase")]
    GetPlaylists {
        #[serde(default)]
        offset: u32,
        #[serde(default = "default_limit")]
        limit: u32,
    },
    #[serde(rename_all = "camelCase")]
    GetPlaylistTracks {
        id: String,
        #[serde(default)]
        offset: u32,
        #[serde(default = "default_limit")]
        limit: u32,
    },
    #[serde(rename_all = "camelCase")]
    GetSavedTracks {
        #[serde(default)]
        offset: u32,
        #[serde(default = "default_limit")]
        limit: u32,
    },
    #[serde(rename_all = "camelCase")]
    GetSavedAlbums {
        #[serde(default)]
        offset: u32,
        #[serde(default = "default_limit")]
        limit: u32,
    },
    /// Create a playlist owned by the signed-in user.
    CreatePlaylist { name: String },
    /// Append tracks to a playlist.
    #[serde(rename_all = "camelCase")]
    AddToPlaylist { playlist_id: String, uris: Vec<String> },
    /// Remove every occurrence of these tracks from a playlist.
    #[serde(rename_all = "camelCase")]
    RemoveFromPlaylist { playlist_id: String, uris: Vec<String> },
    /// Set a playlist's description.
    #[serde(rename_all = "camelCase")]
    DescribePlaylist {
        playlist_id: String,
        description: String,
    },
    /// Move one track within a playlist.
    #[serde(rename_all = "camelCase")]
    ReorderPlaylist {
        playlist_id: String,
        from: u32,
        to: u32,
    },
    /// Take a playlist off the library shelf.
    #[serde(rename_all = "camelCase")]
    UnfollowPlaylist { playlist_id: String },
    /// Rename a playlist.
    #[serde(rename_all = "camelCase")]
    RenamePlaylist { playlist_id: String, name: String },

    GetAlbum { id: String },
    GetArtist { id: String },
    SetSaved { uri: String, saved: bool },

    // -- spotify connect ------------------------------------------------
    ListDevices,
    #[serde(rename_all = "camelCase")]
    TransferPlayback {
        device_id: String,
        #[serde(default = "default_true")]
        play: bool,
    },

    /// Up-next list from the account's active playback.
    GetQueue,
    /// Append a track to the active device's queue.
    AddToQueue { uri: String },
    /// Recently played tracks, newest first.
    #[serde(rename_all = "camelCase")]
    GetRecentlyPlayed {
        #[serde(default = "default_small")]
        limit: u32,
    },
    /// The listener's most-played artists.
    #[serde(rename_all = "camelCase")]
    GetTopArtists {
        #[serde(default = "default_small")]
        limit: u32,
    },
    /// Albums released recently, from Spotify's browse feed.
    #[serde(rename_all = "camelCase")]
    GetNewReleases {
        #[serde(default = "default_small")]
        limit: u32,
    },
    /// Browse categories (the genre grid).
    GetCategories,
    /// Personalised radio stations, one per top artist.
    #[serde(rename_all = "camelCase")]
    GetStations {
        #[serde(default = "default_stations")]
        limit: u32,
    },
    /// Start a radio station seeded from a track, artist or album.
    #[serde(rename_all = "camelCase")]
    StartRadio { seed_uri: String },
    /// The listener's most-played tracks.
    #[serde(rename_all = "camelCase")]
    GetTopTracks {
        #[serde(default = "default_small")]
        limit: u32,
    },
    /// Time-synced lyrics for a track, via librespot's internal endpoint.
    #[serde(rename_all = "camelCase")]
    GetLyrics { track_uri: String },

    // -- jam / group session (experimental) -----------------------------
    JamStatus,
    /// Create a session hosted on this device.
    JamCreate,
    /// Join via a `https://open.spotify.com/socialsession/...` link or a raw token.
    JamJoin { link: String },
    JamLeave,
}

fn default_true() -> bool {
    true
}
fn default_limit() -> u32 {
    50
}
/// Shelf-sized page: enough to fill a row without a long wait.
fn default_small() -> u32 {
    12
}
/// Each station costs two round trips, so keep the row short.
fn default_stations() -> u32 {
    6
}

// ---------------------------------------------------------------------------
// Daemon -> client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Frame {
    /// Successful reply to `Request { id }`.
    Reply { id: u64, payload: Payload },
    /// Failed reply. `message` is already human-readable.
    Error { id: u64, message: String },
    /// Unsolicited state change.
    Event(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "payload", rename_all = "camelCase")]
pub enum Payload {
    /// Generic "it worked, nothing to say".
    Ok,
    Hello {
        protocol: u32,
        version: String,
    },
    State(Box<PlayerState>),
    #[serde(rename_all = "camelCase")]
    LoginStarted {
        /// Open this in a browser to complete the flow.
        auth_url: String,
    },
    SearchResults(Box<SearchResults>),
    Playlists { items: Vec<Playlist>, total: u32 },
    Tracks { items: Vec<Track>, total: u32 },
    Albums { items: Vec<Album>, total: u32 },
    Album(Box<Album>),
    Playlist(Box<Playlist>),
    Artist(Box<Artist>),
    Devices { items: Vec<Device> },
    Jam(Box<JamState>),
    #[serde(rename_all = "camelCase")]
    Settings(Box<SettingsView>),
    #[serde(rename_all = "camelCase")]
    Queue {
        items: Vec<Track>,
        /// Name of the playlist/album the queue is playing from, if known.
        #[serde(default)]
        context_name: Option<String>,
    },
    Lyrics(Box<Lyrics>),
    Artists { items: Vec<Artist> },
    Categories { items: Vec<Category> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum Event {
    /// Full state snapshot. Sent on connect and after any structural change.
    State(Box<PlayerState>),
    /// High-frequency position tick, kept separate so the UI can throttle it
    /// independently of real state changes.
    #[serde(rename_all = "camelCase")]
    Position { position_ms: u32, playing: bool },
    Volume { volume: u16 },
    TrackChanged(Box<Track>),
    AuthChanged(Box<AuthState>),
    Devices { items: Vec<Device> },
    Jam(Box<JamState>),
    /// Non-fatal problem worth surfacing in the UI.
    Notice { message: String, severity: Severity },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

// ---------------------------------------------------------------------------
// State model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    pub auth: AuthState,
    pub track: Option<Track>,
    pub playing: bool,
    pub position_ms: u32,
    pub volume: u16,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    /// True while this device is the active Connect endpoint.
    pub active: bool,
    pub device_name: String,
    pub jam: Option<JamState>,
    /// Name of whichever device is actually playing, when it is not this one.
    ///
    /// Rustify mirrors the account's playback rather than only its own audio
    /// output, so opening the window while your phone is playing shows the
    /// phone's track instead of an empty bar.
    #[serde(default)]
    pub remote_device: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthState {
    pub logged_in: bool,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    /// Streaming needs Premium; the UI warns rather than failing mysteriously.
    pub premium: bool,
    /// True once the Web API half is authorised. Playback works without it;
    /// search, playlists and library do not.
    #[serde(default)]
    pub browsing_ready: bool,
    /// True when a Web API client id is configured but not yet authorised.
    #[serde(default)]
    pub web_client_configured: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepeatMode {
    #[default]
    Off,
    Context,
    Track,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub uri: String,
    pub id: String,
    pub name: String,
    pub artists: Vec<ArtistRef>,
    pub album: Option<AlbumRef>,
    pub duration_ms: u32,
    pub explicit: bool,
    #[serde(default)]
    pub saved: bool,
    /// Largest available cover, or `None` for local files.
    pub cover_url: Option<String>,
    /// ISO date this track was added to the playlist it came from.
    #[serde(default)]
    pub added_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistRef {
    pub uri: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumRef {
    pub uri: String,
    pub name: String,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub uri: String,
    pub id: String,
    pub name: String,
    pub artists: Vec<ArtistRef>,
    pub cover_url: Option<String>,
    pub release_date: Option<String>,
    pub total_tracks: u32,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artist {
    pub uri: String,
    pub id: String,
    pub name: String,
    pub image_url: Option<String>,
    pub followers: u64,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub top_tracks: Vec<Track>,
    #[serde(default)]
    pub albums: Vec<Album>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    pub uri: String,
    pub id: String,
    pub name: String,
    pub owner: String,
    pub description: Option<String>,
    pub cover_url: Option<String>,
    pub total_tracks: u32,
}

/// A browse category, e.g. "Pop" or "Charts".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Category {
    pub id: String,
    pub name: String,
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResults {
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    pub playlists: Vec<Playlist>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub is_active: bool,
    pub volume_percent: Option<u32>,
    /// True when the device is this daemon.
    pub is_self: bool,
}

/// Settings the user can change. Only things this app genuinely controls;
/// account-level options (explicit filter, Canvas, activity sharing) live on
/// Spotify's servers and are deliberately absent rather than faked.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// 96, 160 or 320 kbps.
    pub bitrate: u32,
    /// Even out loudness between tracks.
    pub normalise: bool,
    /// Keep playing similar music when a context ends.
    pub autoplay: bool,
    /// Name shown in everyone else's Spotify device list.
    pub device_name: String,
    /// Cache decoded audio on disk.
    pub cache_audio: bool,
    /// Show what is playing on Discord. Off until asked for: a status that
    /// appears without being asked for is a surprise, not a feature.
    #[serde(default)]
    pub discord_presence: bool,
    /// Run the equaliser. Off means the audio is untouched, not flattened.
    #[serde(default)]
    pub equaliser: bool,
    /// Gain per band in decibels, low to high. Five bands, matching the ones
    /// Spotify's mobile equaliser uses.
    #[serde(default)]
    pub equaliser_gains: Vec<f32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bitrate: 320,
            normalise: true,
            autoplay: true,
            device_name: String::new(),
            cache_audio: true,
            discord_presence: false,
            equaliser: false,
            equaliser_gains: vec![0.0; 5],
        }
    }
}

/// Settings plus the read-only facts the settings page displays.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    pub settings: Settings,
    pub cache_bytes: u64,
    pub config_path: String,
    pub log_path: String,
    pub daemon_version: String,
    /// True when a change needs the daemon restarted to take effect.
    pub restart_required: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lyrics {
    pub lines: Vec<LyricLine>,
    /// True when lines carry real timestamps and can be highlighted in sync.
    pub synced: bool,
    /// Attribution required by the lyrics provider.
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricLine {
    pub time_ms: u32,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Jam
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JamState {
    pub active: bool,
    pub session_id: Option<String>,
    /// Shareable link, present when we are the host.
    pub join_url: Option<String>,
    pub is_host: bool,
    pub participants: Vec<JamParticipant>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JamParticipant {
    pub id: String,
    pub display_name: String,
    pub image_url: Option<String>,
    pub is_host: bool,
    pub is_listening: bool,
}

// ---------------------------------------------------------------------------
// Framing helpers
// ---------------------------------------------------------------------------

/// Encode a frame as one protocol line, newline included.
pub fn encode_line<T: Serialize>(value: &T) -> serde_json::Result<String> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_through_their_tagged_form() {
        let req = Request {
            id: 7,
            command: Command::Seek { position_ms: 1234 },
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"cmd\":\"seek\""));
        assert!(line.contains("\"positionMs\":1234"));

        let back: Request = serde_json::from_str(&line).unwrap();
        assert!(matches!(
            back.command,
            Command::Seek { position_ms: 1234 }
        ));
        assert_eq!(back.id, 7);
    }

    /// Regression: the envelope id must never shadow a command's own `id`.
    /// Flattening these into one object silently replaced the playlist id
    /// with the correlation counter, so every playlist click hung.
    #[test]
    fn an_id_carrying_command_survives_the_envelope() {
        let req = Request {
            id: 42,
            command: Command::GetPlaylistTracks {
                id: "7sZbq8QGyMnhKPcLJvCUFD".into(),
                offset: 0,
                limit: 50,
            },
        };
        let line = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&line).unwrap();

        assert_eq!(back.id, 42);
        match back.command {
            Command::GetPlaylistTracks { id, .. } => {
                assert_eq!(id, "7sZbq8QGyMnhKPcLJvCUFD")
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn load_context_defaults_let_the_ui_omit_optional_fields() {
        let cmd: Command =
            serde_json::from_str(r#"{"cmd":"loadContext","uri":"spotify:album:x"}"#).unwrap();
        match cmd {
            Command::LoadContext {
                start_playing,
                index,
                shuffle,
                ..
            } => {
                assert!(start_playing, "playback should start by default");
                assert_eq!(index, None);
                assert!(!shuffle);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn frames_are_distinguishable_by_kind() {
        let ev = Frame::Event(Event::Position {
            position_ms: 500,
            playing: true,
        });
        let line = encode_line(&ev).unwrap();
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"kind\":\"event\""));
        assert!(line.contains("\"event\":\"position\""));
    }
}
