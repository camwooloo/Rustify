//! Shared daemon state and the command handlers that mutate it.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use spotify_jam::JamClient;
use spotify_player_core::{auth, auth::Profile, Engine, EngineConfig, StoredToken};
use spotify_proto::{AuthState, Command, Event, Payload, PlayerState, Severity};
use spotify_web::WebClient;
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::{debug, info, warn};

/// The Jam operations reachable over IPC.
enum JamOp {
    Status,
    Create,
    Join(String),
    Leave,
}

/// Everything the daemon owns. Sub-systems are `Option` because the daemon
/// starts and serves clients *before* login, so the UI can render a sign-in
/// screen instead of failing to connect.
pub struct Daemon {
    pub state: Arc<RwLock<PlayerState>>,
    pub events: broadcast::Sender<Event>,
    engine: RwLock<Option<Arc<Engine>>>,
    web: RwLock<Option<WebClient>>,
    jam: RwLock<Option<JamClient>>,
    /// Serialises login so two clients cannot race the callback port. Held
    /// via an owned guard because it must survive into the spawned task that
    /// waits for the browser redirect.
    login: Arc<Mutex<()>>,
    /// Set when the Web API half still needs its own OAuth round trip.
    web_client_id: RwLock<Option<String>>,
    /// Authorization URL of the flow currently awaiting a browser redirect.
    /// Retrying `Login` re-opens this rather than being refused — a browser
    /// that failed to open must never lock the user out.
    pending_login: RwLock<Option<String>>,
    config: EngineConfig,
}

impl Daemon {
    pub fn new(config: EngineConfig) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            state: Arc::new(RwLock::new(PlayerState {
                device_name: config.device_name.clone(),
                ..Default::default()
            })),
            events,
            engine: RwLock::new(None),
            web: RwLock::new(None),
            jam: RwLock::new(None),
            login: Arc::new(Mutex::new(())),
            pending_login: RwLock::new(None),
            web_client_id: RwLock::new(None),
            config,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub async fn snapshot(&self) -> PlayerState {
        self.state.read().await.clone()
    }

    fn notify(&self, message: impl Into<String>, severity: Severity) {
        let _ = self.events.send(Event::Notice {
            message: message.into(),
            severity,
        });
    }

    /// Bring up playback, the Web API client, and Jam for a valid token.
    ///
    /// Safe to call repeatedly; an existing engine is torn down first so a
    /// re-login does not leave two Connect devices advertised.
    pub async fn boot(self: &Arc<Self>, token: &StoredToken) -> Result<()> {
        if let Some(old) = self.engine.write().await.take() {
            let _ = old.shutdown();
        }

        // Ordering matters. The session must exist so Jam can register its
        // dealer listener, and that registration must happen before the
        // session connects — which `Engine::start` triggers via Spirc.
        let session = spotify_player_core::new_session(self.config.cache_audio);
        let jam = JamClient::new(session.clone());
        let jam_updates = jam.subscribe();

        let engine = Engine::start(
            self.config.clone(),
            token,
            self.events.clone(),
            self.state.clone(),
            session,
        )
        .await
        .context("starting playback engine")?;

        // Premium status comes from the librespot session, not the Web API:
        // Spotify removed `product` from the profile endpoint.
        let premium = engine
            .session()
            .get_user_attribute("type")
            .map(|t| t.eq_ignore_ascii_case("premium"))
            .unwrap_or(false);

        // Browsing runs on a *separate* client id. The keymaster id used for
        // streaming is permanently 429-ed on api.spotify.com because every
        // librespot-derived project shares its rate-limit budget.
        let configured = crate::config::load().web_client_id;
        *self.web_client_id.write().await =
            (!configured.is_empty()).then(|| configured.clone());

        let web_token = match configured.is_empty() {
            true => None,
            false => auth::current_token(&Profile::web(configured.clone())).await,
        };

        let mut auth_state = AuthState {
            logged_in: true,
            premium,
            web_client_configured: !configured.is_empty(),
            ..Default::default()
        };

        let mut web_client = None;
        if let Some(wt) = web_token {
            let mut client = WebClient::new(
                &wt.access_token,
                &wt.refresh_token,
                wt.expires_at_unix.saturating_sub(now_unix()).max(60),
            );
            match client.load_profile().await {
                Ok(profile) => {
                    auth_state.username = profile.username;
                    auth_state.display_name = profile.display_name;
                    auth_state.avatar_url = profile.avatar_url;
                    auth_state.browsing_ready = true;
                }
                Err(e) => warn!("could not load profile: {e:#}"),
            }
            web_client = Some(client);
        }

        if !premium {
            self.notify(
                "This account isn't Premium. Browsing works, but playback will not.",
                Severity::Warning,
            );
        }

        self.spawn_jam_listener(jam_updates);

        {
            let mut s = self.state.write().await;
            s.auth = auth_state.clone();
            s.device_name = engine.device_name().to_string();
        }

        *self.engine.write().await = Some(engine);
        *self.web.write().await = web_client;
        *self.jam.write().await = Some(jam);

        let _ = self.events.send(Event::AuthChanged(Box::new(auth_state)));
        let _ = self
            .events
            .send(Event::State(Box::new(self.snapshot().await)));

        info!("daemon ready");
        Ok(())
    }

    /// Forward live Jam updates from the dealer into the event stream.
    fn spawn_jam_listener(
        self: &Arc<Self>,
        stream: anyhow::Result<impl futures_util::Stream<Item = spotify_proto::JamState> + Send + 'static>,
    ) {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                warn!("jam live updates unavailable: {e:#}");
                return;
            }
        };
        let this = self.clone();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            let mut stream = Box::pin(stream);
            while let Some(update) = stream.next().await {
                {
                    let mut s = this.state.write().await;
                    s.jam = update.active.then(|| update.clone());
                }
                let _ = this.events.send(Event::Jam(Box::new(update)));
            }
            warn!("jam update stream ended");
        });
    }

    /// Borrow the Web API client, explaining precisely what is missing.
    ///
    /// "not signed in" would be a lie here: playback may be working fine and
    /// only the browsing half is unconfigured.
    async fn require_web(&self) -> Result<tokio::sync::RwLockReadGuard<'_, Option<WebClient>>> {
        let guard = self.web.read().await;
        if guard.is_some() {
            return Ok(guard);
        }
        Err(match self.web_client_id.read().await.is_some() {
            true => anyhow!("browsing is not authorised yet — finish the browsing sign-in"),
            false => anyhow!(
                "browsing needs your own Spotify app — add a Client ID in the app \
                 to enable search, playlists and your library (playback works without it)"
            ),
        })
    }

    /// Mirror whatever the account is playing, wherever that is.
    ///
    /// Rustify is one device among several. When playback lives on a phone or
    /// the official desktop app, this keeps the window showing the real track
    /// instead of an empty bar. Skipped entirely while this device is the
    /// active endpoint, where librespot's own events are authoritative and
    /// far more precise.
    pub async fn poll_remote(self: &Arc<Self>) {
        if self.state.read().await.active {
            return;
        }

        let remote = {
            let guard = self.web.read().await;
            let Some(web) = guard.as_ref() else { return };
            match web.current_playback().await {
                Ok(v) => v,
                Err(e) => {
                    debug!("could not read remote playback: {e:#}");
                    return;
                }
            }
        };

        let mut changed_track = None;
        {
            let mut s = self.state.write().await;

            // Re-check: playback may have moved to us mid-request.
            if s.active {
                return;
            }

            match remote {
                Some(r) => {
                    let new_uri = r.track.as_ref().map(|t| t.uri.clone());
                    let old_uri = s.track.as_ref().map(|t| t.uri.clone());
                    if new_uri != old_uri {
                        changed_track = r.track.clone();
                    }
                    s.track = r.track;
                    s.playing = r.is_playing;
                    s.position_ms = r.progress_ms;
                    s.shuffle = r.shuffle;
                    s.remote_device = Some(r.device_name);
                    if let Some(vol) = r.volume_percent {
                        s.volume = ((vol.min(100) as f32 / 100.0) * u16::MAX as f32) as u16;
                    }
                }
                None => {
                    // Nothing playing anywhere.
                    s.playing = false;
                    s.remote_device = None;
                }
            }
        }

        if let Some(track) = changed_track {
            let _ = self.events.send(Event::TrackChanged(Box::new(track)));
        }
        let _ = self
            .events
            .send(Event::State(Box::new(self.snapshot().await)));
    }

    /// Current settings plus the read-only facts the settings page shows.
    async fn settings_view(&self) -> spotify_proto::SettingsView {
        let cfg = crate::config::load();
        spotify_proto::SettingsView {
            cache_bytes: crate::config::audio_cache_bytes(),
            config_path: crate::config::path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            log_path: crate::config::log_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            // A running engine was built from whatever was on disk at startup.
            restart_required: self.engine.read().await.is_some(),
            settings: cfg.settings,
        }
    }

    async fn engine(&self) -> Result<Arc<Engine>> {
        self.engine
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("not signed in yet"))
    }

    // -- command dispatch ------------------------------------------------

    pub async fn handle(self: &Arc<Self>, command: Command) -> Result<Payload> {
        use Command::*;
        match command {
            Hello { protocol } => {
                if protocol != spotify_proto::PROTOCOL_VERSION {
                    return Err(anyhow!(
                        "protocol mismatch: client speaks v{protocol}, daemon speaks v{}",
                        spotify_proto::PROTOCOL_VERSION
                    ));
                }
                Ok(Payload::Hello {
                    protocol: spotify_proto::PROTOCOL_VERSION,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                })
            }

            GetState => Ok(Payload::State(Box::new(self.snapshot().await))),

            Shutdown => {
                if let Ok(engine) = self.engine().await {
                    let _ = engine.shutdown();
                }
                Ok(Payload::Ok)
            }

            // -- auth ------------------------------------------------------
            Login => self.start_login(Profile::streaming()).await,

            SetWebClientId { client_id } => {
                let client_id = client_id.trim().to_string();

                // Empty means "use the app Rustify ships with". Without this
                // there was no way back: anyone who had entered their own id
                // was stuck with it even after being added to Rustify's app.
                let reset = client_id.is_empty();

                if !reset
                    && (client_id.len() != 32
                        || !client_id.chars().all(|c| c.is_ascii_hexdigit()))
                {
                    // Spotify client ids are 32 hex characters; catching this
                    // here beats a confusing "invalid client" page in the
                    // browser.
                    return Err(anyhow!(
                        "that doesn't look like a Client ID (expected 32 hex characters)"
                    ));
                }

                // Merge, so setting a client id never wipes saved settings.
                // Storing empty lets the bundled default keep applying.
                let mut cfg = crate::config::load();
                cfg.web_client_id = client_id.clone();
                crate::config::save(&cfg).context("saving the client id")?;

                // The cached token belongs to whichever app was in use before,
                // so it has to go or the old one would keep being used.
                let effective = crate::config::load().web_client_id;
                auth::clear_web_token().ok();

                let client_id = effective;
                *self.web_client_id.write().await = Some(client_id.clone());

                {
                    let mut st = self.state.write().await;
                    st.auth.web_client_configured = true;
                }

                // Authorising the Web API is a second, separate OAuth round
                // trip: different client id, different scopes, different cache.
                self.start_login(Profile::web(client_id)).await
            }

            Logout => {
                if let Some(engine) = self.engine.write().await.take() {
                    let _ = engine.shutdown();
                }
                *self.web.write().await = None;
                *self.jam.write().await = None;
                *self.pending_login.write().await = None;
                auth::clear_token().ok();

                let mut s = self.state.write().await;
                *s = PlayerState {
                    device_name: self.config.device_name.clone(),
                    ..Default::default()
                };
                let snapshot = s.clone();
                drop(s);

                let _ = self.events.send(Event::State(Box::new(snapshot)));
                Ok(Payload::Ok)
            }

            // -- transport -------------------------------------------------
            Play => self.engine().await?.play().map(|_| Payload::Ok),
            Pause => self.engine().await?.pause().map(|_| Payload::Ok),
            PlayPause => self.engine().await?.play_pause().map(|_| Payload::Ok),
            Next => self.engine().await?.next().map(|_| Payload::Ok),
            Previous => self.engine().await?.previous().map(|_| Payload::Ok),
            Seek { position_ms } => self.engine().await?.seek(position_ms).map(|_| Payload::Ok),
            SetVolume { volume } => self
                .engine()
                .await?
                .set_volume(volume)
                .map(|_| Payload::Ok),
            SetShuffle { enabled } => self
                .engine()
                .await?
                .set_shuffle(enabled)
                .map(|_| Payload::Ok),
            SetRepeat { mode } => self.engine().await?.set_repeat(mode).map(|_| Payload::Ok),

            LoadContext {
                uri,
                start_playing,
                index,
                shuffle,
            } => {
                let engine = self.engine().await?;
                // Take over playback first, otherwise a load aimed at an idle
                // device is silently dropped.
                let _ = engine.activate();
                engine
                    .load_context(uri, start_playing, index, shuffle)
                    .map(|_| Payload::Ok)
            }

            LoadTracks {
                uris,
                start_playing,
            } => {
                let engine = self.engine().await?;
                let _ = engine.activate();
                engine.load_tracks(uris, start_playing).map(|_| Payload::Ok)
            }

            // -- web api ---------------------------------------------------
            Search { query, limit } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                let mut results = web.search(&query, limit).await?;
                // Heart state is not part of the search response.
                let _ = web.annotate_saved(&mut results.tracks).await;
                Ok(Payload::SearchResults(Box::new(results)))
            }

            GetPlaylists { offset, limit } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                let (items, total) = web.playlists(offset, limit).await?;
                Ok(Payload::Playlists { items, total })
            }

            GetPlaylistTracks { id, offset, limit } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                let (mut items, total) = web.playlist_tracks(&id, offset, limit).await?;
                let _ = web.annotate_saved(&mut items).await;
                Ok(Payload::Tracks { items, total })
            }

            GetSavedTracks { offset, limit } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                let (items, total) = web.saved_tracks(offset, limit).await?;
                Ok(Payload::Tracks { items, total })
            }

            GetSavedAlbums { offset, limit } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                let (items, total) = web.saved_albums(offset, limit).await?;
                Ok(Payload::Albums { items, total })
            }

            GetAlbum { id } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                let mut album = web.album(&id).await?;
                let _ = web.annotate_saved(&mut album.tracks).await;
                Ok(Payload::Album(Box::new(album)))
            }

            GetArtist { id } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                Ok(Payload::Artist(Box::new(web.artist(&id).await?)))
            }

            SetSaved { uri, saved } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                web.set_saved(&uri, saved).await?;

                // Keep the now-playing heart in sync if it is the same track.
                let mut s = self.state.write().await;
                if let Some(track) = s.track.as_mut() {
                    if track.uri == uri {
                        track.saved = saved;
                    }
                }
                Ok(Payload::Ok)
            }

            // -- connect ---------------------------------------------------
            ListDevices => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                let name = self.state.read().await.device_name.clone();
                let items = web.devices(&name).await?;
                let _ = self.events.send(Event::Devices {
                    items: items.clone(),
                });
                Ok(Payload::Devices { items })
            }

            TransferPlayback { device_id, play } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                web.transfer_playback(&device_id, play).await?;
                Ok(Payload::Ok)
            }

            GetQueue => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                let (items, context_name) = web.queue().await?;
                Ok(Payload::Queue {
                    items,
                    context_name,
                })
            }

            GetSettings => Ok(Payload::Settings(Box::new(self.settings_view().await))),

            SetSettings(mut settings) => {
                settings.bitrate = match settings.bitrate {
                    96 | 160 | 320 => settings.bitrate,
                    other => return Err(anyhow!("unsupported bitrate: {other}")),
                };
                if settings.device_name.trim().is_empty() {
                    settings.device_name = self.config.device_name.clone();
                }

                let mut cfg = crate::config::load();
                cfg.settings = settings;
                crate::config::save(&cfg).context("saving settings")?;

                // Playback options are baked into the librespot session at
                // startup, so they take effect when the daemon next starts
                // rather than mid-track.
                self.notify(
                    "Settings saved. Playback options apply when the player restarts.",
                    Severity::Info,
                );
                Ok(Payload::Settings(Box::new(self.settings_view().await)))
            }

            OpenExternal { url } => {
                // Only ever hand https links to the OS: this command is
                // reachable from the webview, so it must not be able to launch
                // arbitrary schemes such as file: or a custom protocol.
                if !url.starts_with("https://") {
                    return Err(anyhow!("refusing to open a non-https link"));
                }
                open_browser(&url);
                Ok(Payload::Ok)
            }

            ClearCache => {
                let removed = crate::config::clear_audio_cache()?;
                self.notify(
                    format!("Cleared {:.0} MB of cached audio.", removed as f64 / 1_048_576.0),
                    Severity::Info,
                );
                Ok(Payload::Settings(Box::new(self.settings_view().await)))
            }

            AddToQueue { uri } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                web.add_to_queue(&uri).await?;
                Ok(Payload::Ok)
            }

            GetRecentlyPlayed { limit } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                Ok(Payload::Tracks {
                    items: web.recently_played(limit).await?,
                    total: 0,
                })
            }

            GetTopArtists { limit } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                Ok(Payload::Artists {
                    items: web.top_artists(limit).await?,
                })
            }

            GetNewReleases { limit } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                Ok(Payload::Albums {
                    items: web.new_releases(limit).await?,
                    total: 0,
                })
            }

            StartRadio { seed_uri } => {
                let engine = self.engine().await?;
                let uri = spotify_jam::social::radio_for(engine.session(), &seed_uri)
                    .await
                    .map_err(|e| anyhow!("{e:#} (radio uses a private Spotify API)"))?
                    .ok_or_else(|| anyhow!("Spotify returned no radio for that"))?;

                let _ = engine.activate();
                engine.load_context(uri, true, None, false)?;
                Ok(Payload::Ok)
            }

            GetStations { limit } => {
                let engine = self.engine().await?;
                let artists = {
                    let web = self.require_web().await?;
                    let web = web.as_ref().expect("checked by require_web");
                    web.top_artists(limit.min(8)).await?
                };

                // One radio per top artist. Spotify's own "Made for you"
                // shelves come from a home feed with no public API, so this
                // builds equivalent stations from data we can actually reach.
                //
                // The card is described from the *seed artist*, not the
                // generated playlist: those live under 37i9dQZF1E8… ids that
                // the Web API returns 404 for, even on a grandfathered app.
                // The uri still plays fine as a context.
                let mut items = Vec::new();
                for artist in artists {
                    let Ok(Some(uri)) =
                        spotify_jam::social::radio_for(engine.session(), &artist.uri).await
                    else {
                        continue;
                    };
                    items.push(spotify_proto::Playlist {
                        id: uri.rsplit(':').next().unwrap_or_default().to_string(),
                        uri,
                        name: format!("{} Radio", artist.name),
                        owner: format!("With {} and more", artist.name),
                        description: None,
                        cover_url: artist.image_url.clone(),
                        total_tracks: 0,
                    });
                }
                Ok(Payload::Playlists {
                    total: items.len() as u32,
                    items,
                })
            }

            GetCategories => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                Ok(Payload::Categories {
                    items: web.categories().await?,
                })
            }

            GetTopTracks { limit } => {
                let web = self.require_web().await?;
                let web = web.as_ref().expect("checked by require_web");
                Ok(Payload::Tracks {
                    items: web.top_tracks(limit).await?,
                    total: 0,
                })
            }

            GetLyrics { track_uri } => {
                let engine = self.engine().await?;
                let lyrics = spotify_jam::social::lyrics(engine.session(), &track_uri)
                    .await
                    .map_err(|e| anyhow!("{e:#} (lyrics use a private Spotify API)"))?;
                Ok(Payload::Lyrics(Box::new(lyrics)))
            }

            // -- jam -------------------------------------------------------
            JamStatus => self.jam_op(JamOp::Status).await,
            JamCreate => self.jam_op(JamOp::Create).await,
            JamJoin { link } => self.jam_op(JamOp::Join(link)).await,
            JamLeave => self.jam_op(JamOp::Leave).await,
        }
    }

    /// Run a Jam operation and fold the result into shared state.
    ///
    /// Modelled as an enum rather than a closure: the borrow of the client
    /// must not outlive the read guard, and an enum keeps that obvious.
    async fn jam_op(self: &Arc<Self>, op: JamOp) -> Result<Payload> {
        let jam = self.jam.read().await;
        let jam = jam.as_ref().ok_or_else(|| anyhow!("not signed in yet"))?;

        let result = match &op {
            JamOp::Status => jam.current().await,
            JamOp::Create => jam.create().await,
            JamOp::Join(link) => jam.join(link).await,
            JamOp::Leave => jam.leave().await,
        };

        let state = result.map_err(|e| {
            // Jam is undocumented; make that explicit rather than presenting a
            // schema change as a generic failure.
            anyhow!("{e:#} (Jam uses a private Spotify API and may have changed)")
        })?;

        {
            let mut s = self.state.write().await;
            s.jam = state.active.then(|| state.clone());
        }
        let _ = self.events.send(Event::Jam(Box::new(state.clone())));
        Ok(Payload::Jam(Box::new(state)))
    }

    /// Begin interactive login, returning the URL immediately.
    ///
    /// Idempotent while a flow is outstanding: clicking "Log in" again
    /// re-opens the same URL instead of erroring. The previous behaviour
    /// locked the user out for the full timeout if the browser failed to open.
    async fn start_login(self: &Arc<Self>, profile: Profile) -> Result<Payload> {
        if let Some(auth_url) = self.pending_login.read().await.clone() {
            open_browser(&auth_url);
            return Ok(Payload::LoginStarted { auth_url });
        }

        let guard = self
            .login
            .clone()
            .try_lock_owned()
            .map_err(|_| anyhow!("a login is already starting, try again in a moment"))?;

        let is_web = profile.cache_file == Profile::web(String::new()).cache_file;
        let pending = auth::begin_login(profile).await?;
        let auth_url = pending.auth_url.clone();
        *self.pending_login.write().await = Some(auth_url.clone());

        // Opening the browser is the daemon's job: a WebView2 cannot do it,
        // and the UI may not even be running.
        open_browser(&auth_url);

        let this = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let result = pending.wait(std::time::Duration::from_secs(300)).await;

            // Clear before reporting, so a retry after failure works at once.
            *this.pending_login.write().await = None;

            match result {
                Ok(token) => {
                    // Either flow completing means re-running boot: it wires
                    // whichever halves now have valid tokens.
                    let token = match is_web {
                        // The web flow does not produce streaming credentials,
                        // so reuse the cached streaming token.
                        true => match auth::current_token(&Profile::streaming()).await {
                            Some(t) => t,
                            None => {
                                this.notify(
                                    "Browsing is authorised. Sign in to Spotify to enable playback.",
                                    Severity::Info,
                                );
                                return;
                            }
                        },
                        false => token,
                    };

                    if let Err(e) = this.boot(&token).await {
                        warn!("login succeeded but startup failed: {e:#}");
                        this.notify(format!("Sign-in failed: {e}"), Severity::Error);
                    }
                }
                Err(e) => {
                    warn!("login failed: {e:#}");
                    this.notify(format!("Sign-in failed: {e}"), Severity::Error);
                }
            }
        });

        Ok(Payload::LoginStarted { auth_url })
    }
}

/// Best-effort: hand the URL to the system browser.
///
/// Never fatal. If it fails the UI still shows the link for copying, which is
/// the only reason sign-in can proceed at all on a locked-down desktop.
fn open_browser(url: &str) {
    if let Err(e) = open::that_detached(url) {
        warn!("could not open a browser automatically: {e}");
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
