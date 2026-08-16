//! Newline-delimited JSON server over loopback TCP.

use std::sync::Arc;

use anyhow::{Context, Result};
use spotify_proto::{encode_line, Event, Frame, Request};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use tracing::{debug, info, warn};

use crate::state::Daemon;

/// Claim the IPC port.
///
/// Called before anything else so a duplicate player exits immediately,
/// without first opening a Spotify session and registering a Connect device.
/// Doing that work before binding meant every redundant launch produced a
/// burst of network traffic and a device that appeared and vanished.
pub async fn bind(port: u16) -> Result<TcpListener> {
    // Loopback only. This binds no external interface by design: the daemon
    // holds a live Spotify session and must not be reachable off-machine.
    TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("another Rustify player already has port {port}"))
}

pub async fn serve(daemon: Arc<Daemon>, listener: TcpListener) -> Result<()> {
    let port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or_default();

    info!("IPC listening on 127.0.0.1:{port}");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("accept failed: {e}");
                continue;
            }
        };
        debug!(%peer, "client connected");
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(daemon, stream).await {
                debug!(%peer, "client disconnected: {e:#}");
            }
        });
    }
}

async fn handle_client(daemon: Arc<Daemon>, stream: TcpStream) -> Result<()> {
    // Interactive control traffic: latency matters far more than packing.
    stream.set_nodelay(true).ok();

    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let mut events = daemon.subscribe();

    // Send the current state immediately so a freshly-opened UI paints
    // without having to ask.
    let hello = Frame::Event(Event::State(Box::new(daemon.snapshot().await)));
    write_half.write_all(encode_line(&hello)?.as_bytes()).await?;

    loop {
        tokio::select! {
            // Client -> daemon
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                if line.trim().is_empty() {
                    continue;
                }

                // Recover the id before parsing the command, so even a
                // malformed request gets a correlated reply. Without this the
                // caller's promise is orphaned and hangs until it times out.
                let id = serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|v| v.get("id").and_then(serde_json::Value::as_u64))
                    .unwrap_or(0);

                let frame = match serde_json::from_str::<Request>(&line) {
                    Ok(req) => match daemon.handle(req.command).await {
                        Ok(payload) => Frame::Reply { id: req.id, payload },
                        // `{:#}` unrolls the anyhow context chain, which is
                        // what makes these messages diagnosable in the UI.
                        Err(e) => Frame::Error {
                            id: req.id,
                            message: format!("{e:#}"),
                        },
                    },
                    Err(e) => Frame::Error {
                        id,
                        message: format!("malformed request: {e}"),
                    },
                };

                write_half.write_all(encode_line(&frame)?.as_bytes()).await?;
            }

            // Daemon -> client
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        let frame = Frame::Event(event);
                        write_half.write_all(encode_line(&frame)?.as_bytes()).await?;
                    }
                    // A slow client that fell behind: resync rather than drop it.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("client lagged {n} events; resyncing");
                        let frame = Frame::Event(Event::State(Box::new(daemon.snapshot().await)));
                        write_half.write_all(encode_line(&frame)?.as_bytes()).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(())
}
