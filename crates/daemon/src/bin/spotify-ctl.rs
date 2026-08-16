//! `rustify-ctl` — command-line control and diagnostics for the daemon.
//!
//! Speaks the same IPC protocol as the window, which makes it the fastest way
//! to test the daemon without a GUI in the way. It is also genuinely useful on
//! its own: bind `rustify-ctl toggle` to a media key and you can control
//! playback with no window open at all.
//!
//! ```text
//! rustify-ctl status
//! rustify-ctl login
//! rustify-ctl play spotify:album:0JGOiO34nwfUdDrD612dOp
//! rustify-ctl listen          # stream live events; great for testing
//! ```

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use spotify_proto::{Command, Frame, Request, RepeatMode, DEFAULT_PORT, PROTOCOL_VERSION};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

const USAGE: &str = "\
rustify-ctl — control the Rustify daemon

USAGE:
    rustify-ctl <command> [args]

PLAYBACK
    status                  Show current player state
    play [uri]              Resume, or start a context/track URI
    pause                   Pause
    toggle                  Play/pause
    next | prev             Skip
    seek <seconds>          Seek within the current track
    volume <0-100>          Set volume
    shuffle <on|off>
    repeat <off|context|track>

ACCOUNT
    login                   Start the browser sign-in flow
    logout                  Sign out and clear the cached token

BROWSE
    search <query>          Search the catalogue
    playlists               List your playlists
    liked                   List saved tracks

CONNECT
    devices                 List Spotify Connect devices
    transfer <device-id>    Move playback to a device

JAM (experimental)
    jam status | create | leave
    jam join <link>

DIAGNOSTICS
    listen                  Stream live events until interrupted
    ping                    Check the daemon is reachable
    raw <json>              Send a raw protocol command

ENV
    SPOTIFY_RUST_PORT       Daemon port (default 4381)
";

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(verb) = args.first().map(String::as_str) else {
        print!("{USAGE}");
        return Ok(());
    };

    if matches!(verb, "-h" | "--help" | "help") {
        print!("{USAGE}");
        return Ok(());
    }

    let port: u16 = std::env::var("SPOTIFY_RUST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let mut conn = Connection::open(port).await.with_context(|| {
        format!("could not reach the daemon on 127.0.0.1:{port} — is rustifyd running?")
    })?;

    // Fail fast and clearly on a version skew rather than misbehaving later.
    conn.request(Command::Hello {
        protocol: PROTOCOL_VERSION,
    })
    .await
    .context("handshake failed")?;

    let arg = |i: usize| args.get(i).map(String::as_str);

    match verb {
        "ping" => println!("daemon is up on port {port}"),

        "status" => print_status(&conn.request(Command::GetState).await?),

        "login" => {
            let reply = conn.request(Command::Login).await?;
            match reply.get("authUrl").and_then(Value::as_str) {
                Some(url) => {
                    println!("Open this URL to sign in:\n\n  {url}\n");
                    println!("Waiting for the daemon to finish sign-in (Ctrl-C to stop)...");
                    // The token arrives asynchronously, so watch for the event.
                    conn.watch(Some("authChanged")).await?;
                }
                None => println!("{reply:#}"),
            }
        }
        "logout" => {
            conn.request(Command::Logout).await?;
            println!("signed out");
        }

        "play" => match arg(1) {
            Some(uri) => {
                let cmd = if uri.contains(":track:") {
                    Command::LoadTracks {
                        uris: vec![uri.to_string()],
                        start_playing: true,
                    }
                } else {
                    Command::LoadContext {
                        uri: uri.to_string(),
                        start_playing: true,
                        index: None,
                        shuffle: false,
                    }
                };
                conn.request(cmd).await?;
                println!("playing {uri}");
            }
            None => {
                conn.request(Command::Play).await?;
                println!("playing");
            }
        },
        "pause" => {
            conn.request(Command::Pause).await?;
            println!("paused");
        }
        "toggle" => {
            conn.request(Command::PlayPause).await?;
            println!("toggled");
        }
        "next" => {
            conn.request(Command::Next).await?;
            println!("skipped");
        }
        "prev" | "previous" => {
            conn.request(Command::Previous).await?;
            println!("back");
        }

        "seek" => {
            let secs: f64 = arg(1)
                .ok_or_else(|| anyhow!("usage: rustify-ctl seek <seconds>"))?
                .parse()
                .context("seconds must be a number")?;
            conn.request(Command::Seek {
                position_ms: (secs * 1000.0).max(0.0) as u32,
            })
            .await?;
            println!("seeked to {secs}s");
        }

        "volume" => {
            let pct: f64 = arg(1)
                .ok_or_else(|| anyhow!("usage: rustify-ctl volume <0-100>"))?
                .parse()
                .context("volume must be a number")?;
            if !(0.0..=100.0).contains(&pct) {
                bail!("volume must be between 0 and 100");
            }
            conn.request(Command::SetVolume {
                volume: (pct / 100.0 * u16::MAX as f64) as u16,
            })
            .await?;
            println!("volume {pct}%");
        }

        "shuffle" => {
            let enabled = match arg(1) {
                Some("on" | "true" | "1") => true,
                Some("off" | "false" | "0") => false,
                _ => bail!("usage: rustify-ctl shuffle <on|off>"),
            };
            conn.request(Command::SetShuffle { enabled }).await?;
            println!("shuffle {}", if enabled { "on" } else { "off" });
        }

        "repeat" => {
            let mode = match arg(1) {
                Some("off") => RepeatMode::Off,
                Some("context" | "all") => RepeatMode::Context,
                Some("track" | "one") => RepeatMode::Track,
                _ => bail!("usage: rustify-ctl repeat <off|context|track>"),
            };
            conn.request(Command::SetRepeat { mode }).await?;
            println!("repeat {mode:?}");
        }

        "search" => {
            let query = args[1..].join(" ");
            if query.is_empty() {
                bail!("usage: rustify-ctl search <query>");
            }
            let res = conn.request(Command::Search { query, limit: 10 }).await?;
            print_search(&res);
        }

        "playlists" => {
            let res = conn
                .request(Command::GetPlaylists {
                    offset: 0,
                    limit: 50,
                })
                .await?;
            for p in res.get("items").and_then(Value::as_array).into_iter().flatten() {
                println!(
                    "{:<40}  {:>4} tracks  {}",
                    truncate(str_of(p, "name"), 40),
                    p.get("totalTracks").and_then(Value::as_u64).unwrap_or(0),
                    str_of(p, "uri")
                );
            }
        }

        "liked" => {
            let res = conn
                .request(Command::GetSavedTracks {
                    offset: 0,
                    limit: 50,
                })
                .await?;
            print_tracks(res.get("items"));
        }

        "devices" => {
            let res = conn.request(Command::ListDevices).await?;
            for d in res.get("items").and_then(Value::as_array).into_iter().flatten() {
                let active = d.get("isActive").and_then(Value::as_bool).unwrap_or(false);
                let is_self = d.get("isSelf").and_then(Value::as_bool).unwrap_or(false);
                println!(
                    "{} {:<32} {:<12} {}",
                    if active { "*" } else { " " },
                    truncate(str_of(d, "name"), 32),
                    str_of(d, "deviceType"),
                    if is_self { "(this daemon)" } else { "" }
                );
            }
            println!("\n* = currently active");
        }

        "transfer" => {
            let id = arg(1).ok_or_else(|| anyhow!("usage: rustify-ctl transfer <device-id>"))?;
            conn.request(Command::TransferPlayback {
                device_id: id.to_string(),
                play: true,
            })
            .await?;
            println!("playback transferred");
        }

        "jam" => {
            let res = match arg(1) {
                None | Some("status") => conn.request(Command::JamStatus).await?,
                Some("create" | "start") => conn.request(Command::JamCreate).await?,
                Some("leave") => conn.request(Command::JamLeave).await?,
                Some("join") => {
                    let link =
                        arg(2).ok_or_else(|| anyhow!("usage: rustify-ctl jam join <link>"))?;
                    conn.request(Command::JamJoin {
                        link: link.to_string(),
                    })
                    .await?
                }
                Some(other) => bail!("unknown jam subcommand: {other}"),
            };
            print_jam(&res);
        }

        "listen" => {
            println!("streaming events (Ctrl-C to stop)...\n");
            conn.watch(None).await?;
        }

        "raw" => {
            let json = arg(1).ok_or_else(|| anyhow!("usage: rustify-ctl raw '<json>'"))?;
            let value: Value = serde_json::from_str(json).context("argument must be JSON")?;
            println!("{:#}", conn.request_raw(value).await?);
        }

        other => {
            eprintln!("unknown command: {other}\n");
            print!("{USAGE}");
            std::process::exit(2);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------- transport

struct Connection {
    reader: tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    writer: tokio::net::tcp::OwnedWriteHalf,
    next_id: u64,
}

impl Connection {
    async fn open(port: u16) -> Result<Self> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await?;
        stream.set_nodelay(true).ok();
        let (r, w) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(r).lines(),
            writer: w,
            next_id: 1,
        })
    }

    async fn request(&mut self, command: Command) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::to_string(&Request { id, command })?;
        self.send_and_await(id, line).await
    }

    /// Send a hand-written command object, for `raw`.
    async fn request_raw(&mut self, value: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        if !value.is_object() {
            return Err(anyhow!("command must be a JSON object"));
        }
        let envelope = serde_json::json!({ "id": id, "command": value });
        self.send_and_await(id, envelope.to_string()).await
    }

    async fn send_and_await(&mut self, id: u64, line: String) -> Result<Value> {
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;

        // Events interleave with replies, so read until ours comes back.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let next = tokio::time::timeout_at(deadline, self.reader.next_line())
                .await
                .map_err(|_| anyhow!("the daemon did not reply within 30s"))??;

            let Some(text) = next else {
                bail!("the daemon closed the connection");
            };
            if text.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<Frame>(&text) {
                Ok(Frame::Reply { id: got, payload }) if got == id => {
                    return Ok(serde_json::to_value(payload)?)
                }
                Ok(Frame::Error { id: got, message }) if got == id => bail!("{message}"),
                // Daemon-side parse failures come back with id 0.
                Ok(Frame::Error { id: 0, message }) => bail!("{message}"),
                _ => continue,
            }
        }
    }

    /// Print events as they arrive. Stops after `until` is seen, if given.
    async fn watch(&mut self, until: Option<&str>) -> Result<()> {
        while let Some(text) = self.reader.next_line().await? {
            if text.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if value.get("kind").and_then(Value::as_str) != Some("event") {
                continue;
            }

            let name = value.get("event").and_then(Value::as_str).unwrap_or("?");
            match name {
                // Too frequent to dump in full; one readable line instead.
                "position" => println!(
                    "position  {:>8}  {}",
                    fmt_ms(value.get("positionMs").and_then(Value::as_u64).unwrap_or(0)),
                    if value.get("playing").and_then(Value::as_bool) == Some(true) {
                        "playing"
                    } else {
                        "paused"
                    }
                ),
                "state" => {
                    println!("state");
                    print_status(&value);
                }
                _ => println!("{name:<14}{value:#}"),
            }

            if Some(name) == until {
                return Ok(());
            }
        }
        bail!("the daemon closed the connection")
    }
}

// ------------------------------------------------------------------ output

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
}

fn fmt_ms(ms: u64) -> String {
    format!("{}:{:02}", ms / 60000, (ms / 1000) % 60)
}

fn artists(track: &Value) -> String {
    track
        .get("artists")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|x| str_of(x, "name"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn print_status(state: &Value) {
    let auth = state.get("auth").cloned().unwrap_or(Value::Null);
    let logged_in = auth.get("loggedIn").and_then(Value::as_bool).unwrap_or(false);

    if !logged_in {
        println!("not signed in — run: rustify-ctl login");
        return;
    }

    println!(
        "account   {} {}",
        str_of(&auth, "displayName"),
        if auth.get("premium").and_then(Value::as_bool) == Some(true) {
            "(Premium)"
        } else {
            "(NOT Premium — playback will not work)"
        }
    );
    println!("device    {}", str_of(state, "deviceName"));

    match state.get("track") {
        Some(t) if !t.is_null() => {
            let pos = state.get("positionMs").and_then(Value::as_u64).unwrap_or(0);
            let dur = t.get("durationMs").and_then(Value::as_u64).unwrap_or(0);
            println!(
                "{}  {} — {}",
                if state.get("playing").and_then(Value::as_bool) == Some(true) {
                    "playing  "
                } else {
                    "paused   "
                },
                str_of(t, "name"),
                artists(t)
            );
            println!("          {} / {}", fmt_ms(pos), fmt_ms(dur));
        }
        _ => println!("idle      nothing loaded"),
    }

    println!(
        "shuffle   {}   repeat {}",
        state.get("shuffle").and_then(Value::as_bool).unwrap_or(false),
        str_of(state, "repeat")
    );

    if let Some(jam) = state.get("jam").filter(|j| !j.is_null()) {
        println!("jam       active, {} participants",
            jam.get("participants").and_then(Value::as_array).map_or(0, Vec::len));
    }
}

fn print_tracks(items: Option<&Value>) {
    for t in items.and_then(Value::as_array).into_iter().flatten() {
        println!(
            "{:<38}  {:<26}  {:>6}  {}",
            truncate(str_of(t, "name"), 38),
            truncate(&artists(t), 26),
            fmt_ms(t.get("durationMs").and_then(Value::as_u64).unwrap_or(0)),
            str_of(t, "uri")
        );
    }
}

fn print_search(res: &Value) {
    if let Some(tracks) = res.get("tracks").filter(|t| !t.as_array().is_none_or(Vec::is_empty)) {
        println!("TRACKS");
        print_tracks(Some(tracks));
    }
    for (label, key) in [("ALBUMS", "albums"), ("ARTISTS", "artists"), ("PLAYLISTS", "playlists")] {
        let items = res.get(key).and_then(Value::as_array);
        if items.is_none_or(Vec::is_empty) {
            continue;
        }
        println!("\n{label}");
        for it in items.into_iter().flatten() {
            println!(
                "{:<38}  {}",
                truncate(str_of(it, "name"), 38),
                str_of(it, "uri")
            );
        }
    }
}

fn print_jam(jam: &Value) {
    if jam.get("active").and_then(Value::as_bool) != Some(true) {
        println!("no active jam");
        return;
    }
    println!(
        "jam active{}",
        if jam.get("isHost").and_then(Value::as_bool) == Some(true) {
            " (you are the host)"
        } else {
            ""
        }
    );
    if let Some(url) = jam.get("joinUrl").and_then(Value::as_str) {
        println!("share: {url}");
    }
    for p in jam.get("participants").and_then(Value::as_array).into_iter().flatten() {
        println!(
            "  {} {}",
            str_of(p, "displayName"),
            if p.get("isHost").and_then(Value::as_bool) == Some(true) {
                "(host)"
            } else {
                ""
            }
        );
    }
}
