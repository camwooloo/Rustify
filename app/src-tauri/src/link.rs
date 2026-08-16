//! The UI's connection to the daemon.
//!
//! The window is a *view* over a process that outlives it. This module keeps
//! that relationship honest: it reconnects on its own, starts the daemon if it
//! is not already running, and never assumes the daemon's lifetime is tied to
//! the window's.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::{mpsc, oneshot, Mutex},
};
use tracing::{debug, info, warn};

/// Event name the webview listens on for daemon pushes.
pub const EVENT_CHANNEL: &str = "daemon-event";
/// Emitted when the link goes up or down, so the UI can show a banner.
pub const STATUS_CHANNEL: &str = "daemon-status";

pub struct DaemonLink {
    port: u16,
    next_id: AtomicU64,
    outbound: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    connected: AtomicBool,
}

impl DaemonLink {
    pub fn new(port: u16) -> Arc<Self> {
        Arc::new(Self {
            port,
            // Ids start at 1; 0 is reserved for daemon-side parse errors that
            // cannot be correlated with a request.
            next_id: AtomicU64::new(1),
            outbound: Mutex::new(None),
            pending: Arc::new(Mutex::new(HashMap::new())),
            connected: AtomicBool::new(false),
        })
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Send a command and await its reply.
    pub async fn call(&self, command: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        if !command.is_object() {
            return Err("command must be a JSON object".to_string());
        }

        // Nested, never merged: injecting `id` alongside the command's own
        // fields used to overwrite playlist/album/artist ids.
        let line = format!("{}\n", json!({ "id": id, "command": command }));

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        {
            let guard = self.outbound.lock().await;
            let sender = guard
                .as_ref()
                .ok_or_else(|| "not connected to the daemon".to_string())?;
            sender.send(line).map_err(|_| {
                "the daemon connection dropped while sending".to_string()
            })?;
        }

        // Bound the wait so a lost reply cannot wedge the UI forever.
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the daemon connection dropped".to_string()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err("the daemon did not reply in time".to_string())
            }
        }
    }

    /// Fail every in-flight request. Called when the socket drops so callers
    /// get an error instead of hanging until their timeout.
    async fn fail_pending(&self, reason: &str) {
        let mut pending = self.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(reason.to_string()));
        }
    }
}

/// Maintain the connection for the life of the app.
pub async fn run(link: Arc<DaemonLink>, app: AppHandle) {
    let mut backoff = Duration::from_millis(250);
    let mut failures: u32 = 0;

    loop {
        match TcpStream::connect(("127.0.0.1", link.port)).await {
            Ok(stream) => {
                backoff = Duration::from_millis(250);
                failures = 0;
                link.connected.store(true, Ordering::Relaxed);
                let _ = app.emit(STATUS_CHANNEL, json!({ "connected": true }));
                info!("connected to daemon on port {}", link.port);

                if let Err(e) = pump(&link, stream, &app).await {
                    debug!("daemon link closed: {e:#}");
                }

                link.connected.store(false, Ordering::Relaxed);
                *link.outbound.lock().await = None;
                link.fail_pending("the daemon connection dropped").await;
                let _ = app.emit(STATUS_CHANNEL, json!({ "connected": false }));
            }
            Err(e) => {
                failures += 1;

                // The first failure is the normal cold-start case. Retrying
                // periodically after that is what makes a player that died —
                // or failed to start once — come back on its own, instead of
                // waiting for the user to restart Rustify.
                if failures == 1 || failures % 10 == 0 {
                    match spawn_daemon() {
                        Ok(()) => info!("started the daemon"),
                        Err(e) => {
                            // A missing binary will not fix itself, and
                            // retrying forever would leave the UI saying
                            // "connecting" indefinitely. Report it instead.
                            warn!("could not start the daemon: {e:#}");
                            let _ = app.emit(
                                STATUS_CHANNEL,
                                json!({ "connected": false, "fatal": format!("{e:#}") }),
                            );
                        }
                    }
                } else {
                    debug!("daemon not reachable: {e}");
                }
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

/// Shuttle frames in both directions until the socket closes.
async fn pump(link: &Arc<DaemonLink>, stream: TcpStream, app: &AppHandle) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (read_half, mut write_half) = stream.into_split();

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    *link.outbound.lock().await = Some(tx);

    let writer = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let frame: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!("unparseable frame from daemon: {e}");
                continue;
            }
        };

        match frame.get("kind").and_then(Value::as_str) {
            Some("reply") => {
                if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = link.pending.lock().await.remove(&id) {
                        let payload = frame.get("payload").cloned().unwrap_or(Value::Null);
                        let _ = tx.send(Ok(payload));
                    }
                }
            }
            Some("error") => {
                let message = frame
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown daemon error")
                    .to_string();
                match frame.get("id").and_then(Value::as_u64) {
                    // id 0 means the daemon could not correlate it; surface it
                    // as a notice rather than dropping it silently.
                    Some(0) | None => {
                        let _ = app.emit(
                            EVENT_CHANNEL,
                            json!({ "event": "notice", "message": message, "severity": "error" }),
                        );
                    }
                    Some(id) => {
                        if let Some(tx) = link.pending.lock().await.remove(&id) {
                            let _ = tx.send(Err(message));
                        }
                    }
                }
            }
            Some("event") => {
                // The media overlay follows the same events the UI does.
                #[cfg(windows)]
                crate::smtc::apply_event(app, &frame);

                // Forward verbatim; the daemon's wire shape is the UI's model.
                let _ = app.emit(EVENT_CHANNEL, frame);
            }
            other => warn!(?other, "unknown frame kind from daemon"),
        }
    }

    writer.abort();
    Err(anyhow!("daemon closed the connection"))
}

/// Launch the daemon as a detached sibling process.
///
/// Detached on purpose: closing the window must not stop playback. That is the
/// whole point of the split, so it is enforced here rather than left to chance.
fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("could not locate the app directory"))?;

    let name = if cfg!(windows) { "rustifyd.exe" } else { "rustifyd" };

    // Next to the executable in development; under resources/ when installed
    // from the bundle. Checking both keeps one code path for both.
    let daemon = [dir.join(name), dir.join("resources").join(name)]
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| dir.join(name));

    if !daemon.exists() {
        return Err(anyhow!(
            "the player component (rustifyd.exe) is missing from {} — an              update may have failed to replace it. Reinstall Rustify, making              sure it is not running first.",
            daemon.display()
        ));
    }

    let mut cmd = std::process::Command::new(&daemon);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NO_WINDOW: survives the app exiting, and
        // never flashes a console window.
        cmd.creation_flags(0x0000_0008 | 0x0800_0000);
    }

    cmd.spawn()?;
    Ok(())
}
