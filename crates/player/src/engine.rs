//! The playback engine: librespot session + player + Spirc, wrapped in a
//! small state machine that emits wire events.
//!
//! Everything here is deliberately GPU-free and UI-free. This is the process
//! that keeps running while you play a game.

use std::sync::Arc;

use anyhow::{Context, Result};
use librespot_connect::{ConnectConfig, LoadContextOptions, LoadRequest, LoadRequestOptions, Options, PlayingTrack, Spirc};
use librespot_core::{
    authentication::Credentials, cache::Cache, config::DeviceType, Session, SessionConfig,
};
use librespot_playback::{
    audio_backend,
    config::{Bitrate, PlayerConfig, VolumeCtrl},
    mixer::{softmixer::SoftMixer, Mixer, MixerConfig},
    player::{Player, PlayerEvent},
};
use spotify_proto::{Event, PlayerState, RepeatMode, Severity, Track};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info, warn};

use crate::{auth::StoredToken, convert::track_from_audio_item};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Name shown in everyone else's Spotify device list.
    pub device_name: String,
    pub device_type: DeviceType,
    pub bitrate: Bitrate,
    pub initial_volume: u16,
    /// Cache decoded audio to disk; cuts repeat network traffic considerably.
    pub cache_audio: bool,
    pub normalisation: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            device_name: "Rustify".to_string(),
            device_type: DeviceType::Computer,
            bitrate: Bitrate::Bitrate320,
            initial_volume: u16::MAX / 2,
            cache_audio: true,
            normalisation: true,
        }
    }
}

pub struct Engine {
    session: Session,
    spirc: Spirc,
    mixer: Arc<dyn Mixer>,
    state: Arc<RwLock<PlayerState>>,
    events: broadcast::Sender<Event>,
    config: EngineConfig,
}

/// Create an unconnected session.
///
/// Returned unconnected on purpose. Every dealer listener — Spirc's and ours —
/// must be registered *before* the session connects, or `listen_for` fails
/// with "Builder wasn't available". Handing the caller an unconnected session
/// makes that ordering explicit and lets it register its own subscriptions
/// before [`Engine::start`] runs.
pub fn new_session(cache_audio: bool) -> Session {
    let cache = build_cache(cache_audio)
        .map_err(|e| warn!("running without an on-disk cache: {e:#}"))
        .ok();

    Session::new(SessionConfig::default(), cache)
}

impl Engine {
    /// Bring up playback and the Connect endpoint on a session from
    /// [`new_session`]. Connecting happens inside `Spirc::new`.
    pub async fn start(
        config: EngineConfig,
        token: &StoredToken,
        events: broadcast::Sender<Event>,
        state: Arc<RwLock<PlayerState>>,
        session: Session,
    ) -> Result<Arc<Self>> {
        let mixer = Arc::new(
            SoftMixer::open(MixerConfig {
                volume_ctrl: VolumeCtrl::Log(VolumeCtrl::DEFAULT_DB_RANGE),
                ..Default::default()
            })
            .context("opening software mixer")?,
        );
        mixer.set_volume(config.initial_volume);

        let player_config = PlayerConfig {
            bitrate: config.bitrate,
            normalisation: config.normalisation,
            ..Default::default()
        };

        // `find(None)` yields the first compiled-in backend, which is rodio
        // (cpal -> WASAPI on Windows).
        let backend = audio_backend::find(None).context("no audio backend compiled in")?;

        let player = Player::new(
            player_config,
            session.clone(),
            mixer.get_soft_volume(),
            move || backend(None, librespot_playback::config::AudioFormat::default()),
        );

        let player_events = player.get_player_event_channel();

        let connect_config = ConnectConfig {
            name: config.device_name.clone(),
            device_type: config.device_type,
            initial_volume: config.initial_volume,
            ..Default::default()
        };

        // The OAuth access token doubles as session credentials, which is why
        // there is only one login for both streaming and the Web API. Spirc
        // performs the actual connection.
        let (spirc, spirc_task) = Spirc::new(
            connect_config,
            session.clone(),
            Credentials::with_access_token(token.access_token.clone()),
            player,
            mixer.clone() as Arc<dyn Mixer>,
        )
        .await
        .context("starting Spotify Connect endpoint")?;

        info!(user = %session.username(), "session established");

        // Spirc owns the control loop; it must run for the device to stay
        // visible to other clients.
        tokio::spawn(spirc_task);

        {
            let mut s = state.write().await;
            s.device_name = config.device_name.clone();
            s.volume = config.initial_volume;
        }

        let engine = Arc::new(Self {
            session,
            spirc,
            mixer,
            state,
            events,
            config,
        });

        tokio::spawn(pump_player_events(
            player_events,
            engine.state.clone(),
            engine.events.clone(),
        ));

        Ok(engine)
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn device_name(&self) -> &str {
        &self.config.device_name
    }

    pub async fn snapshot(&self) -> PlayerState {
        self.state.read().await.clone()
    }

    // -- transport ------------------------------------------------------

    pub fn play(&self) -> Result<()> {
        self.spirc.play().context("play")
    }
    pub fn pause(&self) -> Result<()> {
        self.spirc.pause().context("pause")
    }
    pub fn play_pause(&self) -> Result<()> {
        self.spirc.play_pause().context("play/pause")
    }
    pub fn next(&self) -> Result<()> {
        self.spirc.next().context("next")
    }
    pub fn previous(&self) -> Result<()> {
        self.spirc.prev().context("previous")
    }
    pub fn seek(&self, position_ms: u32) -> Result<()> {
        self.spirc.set_position_ms(position_ms).context("seek")
    }

    pub fn set_volume(&self, volume: u16) -> Result<()> {
        self.mixer.set_volume(volume);
        self.spirc.set_volume(volume).context("set volume")
    }

    pub fn set_shuffle(&self, enabled: bool) -> Result<()> {
        self.spirc.shuffle(enabled).context("set shuffle")
    }

    pub fn set_repeat(&self, mode: RepeatMode) -> Result<()> {
        // librespot models the two repeat axes separately; translate the
        // single three-state control the UI presents.
        match mode {
            RepeatMode::Off => {
                self.spirc.repeat_track(false).context("repeat track off")?;
                self.spirc.repeat(false).context("repeat off")
            }
            RepeatMode::Context => {
                self.spirc.repeat_track(false).context("repeat track off")?;
                self.spirc.repeat(true).context("repeat context")
            }
            RepeatMode::Track => self.spirc.repeat_track(true).context("repeat track"),
        }
    }

    /// Play a playlist/album/artist URI.
    pub fn load_context(
        &self,
        uri: String,
        start_playing: bool,
        index: Option<u32>,
        shuffle: bool,
    ) -> Result<()> {
        let options = LoadRequestOptions {
            start_playing,
            seek_to: 0,
            context_options: Some(LoadContextOptions::Options(Options {
                shuffle,
                repeat: false,
                repeat_track: false,
            })),
            playing_track: index.map(PlayingTrack::Index),
        };
        self.spirc
            .load(LoadRequest::from_context_uri(uri, options))
            .context("loading context")
    }

    /// Play a bare list of track URIs.
    /// Play a list of tracks, optionally starting partway down it.
    ///
    /// Without `index` the whole list is loaded but playback starts at the
    /// first track, so clicking the tenth song in a list would play the
    /// first. Passing only the clicked track instead is what made a song
    /// play alone and stop: the queue held nothing to go on to.
    pub fn load_tracks(
        &self,
        uris: Vec<String>,
        start_playing: bool,
        index: Option<u32>,
    ) -> Result<()> {
        let options = LoadRequestOptions {
            start_playing,
            playing_track: index.map(PlayingTrack::Index),
            ..Default::default()
        };
        self.spirc
            .load(LoadRequest::from_tracks(uris, options))
            .context("loading tracks")
    }

    /// Become the active Connect device.
    pub fn activate(&self) -> Result<()> {
        self.spirc.activate().context("activate device")
    }

    /// Hand playback off / go idle.
    pub fn disconnect(&self, pause: bool) -> Result<()> {
        self.spirc.disconnect(pause).context("disconnect device")
    }

    pub fn shutdown(&self) -> Result<()> {
        self.spirc.shutdown().context("shutdown")
    }
}

fn build_cache(cache_audio: bool) -> Result<Cache> {
    let dirs = directories::ProjectDirs::from("dev", "spotify-rust", "spotify-rust")
        .context("locating cache directory")?;
    let base = dirs.cache_dir().to_path_buf();
    let creds = base.join("credentials");
    let audio = base.join("audio");

    Cache::new(
        Some(creds.as_path()),
        Some(base.as_path()),
        cache_audio.then_some(audio.as_path()),
        // Cap the audio cache so it cannot quietly eat a disk.
        Some(4 * 1024 * 1024 * 1024),
    )
    .context("opening cache")
}

/// Translate librespot's player events into wire events and keep the shared
/// state snapshot current.
async fn pump_player_events(
    mut rx: librespot_playback::player::PlayerEventChannel,
    state: Arc<RwLock<PlayerState>>,
    events: broadcast::Sender<Event>,
) {
    // A send error just means no UI is attached; that is a normal, expected
    // state for this daemon, so it must never be treated as fatal.
    macro_rules! emit {
        ($ev:expr) => {
            let _ = events.send($ev);
        };
    }

    while let Some(event) = rx.recv().await {
        match event {
            PlayerEvent::TrackChanged { audio_item } => {
                let track: Track = track_from_audio_item(&audio_item);
                {
                    let mut s = state.write().await;
                    s.track = Some(track.clone());
                    s.position_ms = 0;
                }
                emit!(Event::TrackChanged(Box::new(track)));
            }

            PlayerEvent::Playing {
                position_ms,
                ..
            } => {
                {
                    let mut s = state.write().await;
                    s.playing = true;
                    s.position_ms = position_ms;
                    s.active = true;
                }
                emit!(Event::Position {
                    position_ms,
                    playing: true
                });
            }

            PlayerEvent::Paused { position_ms, .. } => {
                {
                    let mut s = state.write().await;
                    s.playing = false;
                    s.position_ms = position_ms;
                }
                emit!(Event::Position {
                    position_ms,
                    playing: false
                });
            }

            PlayerEvent::Stopped { .. } => {
                {
                    let mut s = state.write().await;
                    s.playing = false;
                    s.active = false;
                }
                let snapshot = state.read().await.clone();
                emit!(Event::State(Box::new(snapshot)));
            }

            PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::Seeked { position_ms, .. }
            | PlayerEvent::PositionChanged { position_ms, .. } => {
                let playing = {
                    let mut s = state.write().await;
                    s.position_ms = position_ms;
                    s.playing
                };
                emit!(Event::Position {
                    position_ms,
                    playing
                });
            }

            PlayerEvent::VolumeChanged { volume } => {
                {
                    let mut s = state.write().await;
                    s.volume = volume;
                }
                emit!(Event::Volume { volume });
            }

            PlayerEvent::ShuffleChanged { shuffle } => {
                {
                    let mut s = state.write().await;
                    s.shuffle = shuffle;
                }
                let snapshot = state.read().await.clone();
                emit!(Event::State(Box::new(snapshot)));
            }

            PlayerEvent::RepeatChanged { context, track } => {
                {
                    let mut s = state.write().await;
                    s.repeat = match (track, context) {
                        (true, _) => RepeatMode::Track,
                        (false, true) => RepeatMode::Context,
                        (false, false) => RepeatMode::Off,
                    };
                }
                let snapshot = state.read().await.clone();
                emit!(Event::State(Box::new(snapshot)));
            }

            PlayerEvent::Unavailable { track_id, .. } => {
                warn!(?track_id, "track unavailable in this market");
                emit!(Event::Notice {
                    message: "That track isn't available here.".to_string(),
                    severity: Severity::Warning,
                });
            }

            PlayerEvent::SessionDisconnected { .. } => {
                {
                    let mut s = state.write().await;
                    s.active = false;
                    s.playing = false;
                }
                emit!(Event::Notice {
                    message: "Playback moved to another device.".to_string(),
                    severity: Severity::Info,
                });
            }

            other => debug!(?other, "unhandled player event"),
        }
    }

    warn!("player event channel closed");
}
