# spotify-rust

A Spotify desktop client in Rust. Built around a split that the official app
cannot offer: **the player and the window are separate processes.**

```
┌──────────────────────────┐
│  spotify-rust.exe        │   Tauri window (WebView2)
│  UI only, no state       │   closeable at any time
└───────────┬──────────────┘
            │  newline-delimited JSON over 127.0.0.1:4381
┌───────────▼──────────────┐
│  spotifyd-rs.exe         │   librespot playback
│  ~15 MB RSS, no GPU      │   Spotify Connect endpoint
│  keeps playing when the  │   Web API + Jam
│  window is closed        │
└──────────────────────────┘
```

Close the window before launching a game. Music keeps playing from a process
that has never created a GPU context, never composited a frame, and never
loaded Chromium.

## Quick start

```bash
cargo build --release
```

Then start the daemon and the app:

```bash
./target/release/spotifyd-rs.exe
```

```bash
./target/release/spotify-rust.exe
```

The app starts the daemon automatically if it is not already running, so in
normal use you only launch `spotify-rust.exe`. Click **Log in**; your browser
opens for Spotify's OAuth flow and the token is cached, so this is a one-time
step.

**Spotify Premium is required for playback.** Browsing works on a free account;
audio does not. This is a librespot constraint, not a bug.

### Second step: a Client ID for browsing

Playback works immediately after signing in. **Search, playlists and your
library need one extra step**, and there is no way around it:

1. Open [developer.spotify.com/dashboard](https://developer.spotify.com/dashboard)
2. **Create app** — any name
3. Redirect URI, exactly: `http://127.0.0.1:4382/login`
4. Tick **Web API**, save, copy the **Client ID**
5. Paste it into the app when prompted

Why: streaming authenticates with librespot's shared "keymaster" client id,
which is the only id that can request the scopes the streaming session needs.
But Spotify rate-limits `api.spotify.com` **per client id**, and that id is
shared by every librespot-derived project in existence. Its budget is
permanently exhausted — a first request from a cold process returns 429
immediately. Browsing therefore needs an id that belongs to you.

The two halves are authorised separately and cached separately
(`token.json` and `token-web.json`), so one never invalidates the other. You
can skip this entirely and use the app as a Connect target controlled from
your phone.

You can also set the id without the UI:

```bash
SPOTIFY_RUST_CLIENT_ID=<your-client-id> ./target/release/spotifyd-rs.exe
```

## Testing it

`spotify-ctl` speaks the same IPC protocol as the window, so you can exercise
every layer without a GUI in the way. Run the daemon in one terminal and
`spotify-ctl` in another.

```bash
./target/release/spotifyd-rs.exe
```

Work through these in order — each one proves a specific layer, so a failure
tells you exactly which part is broken.

| # | Command | Proves |
| --- | --- | --- |
| 1 | `spotify-ctl ping` | Daemon is up, IPC works |
| 2 | `spotify-ctl login` | OAuth PKCE flow, token cache |
| 3 | `spotify-ctl status` | Session established; confirms Premium |
| 4 | `spotify-ctl search <thing>` | Web API layer |
| 5 | `spotify-ctl play <uri>` | **Audio decode and output** |
| 6 | `spotify-ctl devices` | Connect device list |
| 7 | Open Spotify on your phone | This daemon appears as a device |
| 8 | `spotify-ctl jam create` | Jam (experimental) |
| 9 | `spotify-ctl listen` | Live event stream |

Step 5 is the real milestone: it is the only path that exercises the access
point handshake, the audio key exchange, Vorbis decode, and WASAPI output all
at once. Step 7 is the satisfying one — control this app from your phone.

`spotify-ctl listen` is the best debugging tool here. Leave it running in a
spare terminal and drive playback from the app or your phone; every state
change streams past. If something looks wrong in the UI, this shows whether
the daemon or the UI is at fault.

For deeper detail:

```bash
SPOTIFY_RUST_LOG=debug,librespot=debug ./target/release/spotifyd-rs.exe
```

### Testing the performance claim

The point of this project is that closing the window costs nothing. Measure it
rather than trusting it. Use PresentMon or CapFrameX (not an in-game FPS
counter — you want **1% low** frame times, which is where stutter shows up and
average FPS hides it).

Run the same benchmark scene three times:

1. Nothing else running — your baseline
2. Official Spotify running and playing
3. `spotifyd-rs.exe` running and playing, window closed

Run 3 should be indistinguishable from run 1. If runs 1 and 2 also match, the
stutter was never Spotify and it is worth looking at drivers, background
processes, or thermals instead.

### Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `SPOTIFY_RUST_PORT` | `4381` | IPC port (loopback only) |
| `SPOTIFY_RUST_DEVICE_NAME` | `<hostname> (spotify-rust)` | Name in the Connect device list |
| `SPOTIFY_RUST_LOG` | `info,librespot=warn` | `tracing` filter |

## Layout

| Path | Role |
| --- | --- |
| `crates/proto` | Wire protocol. The single definition of every command, event, and model. |
| `crates/player` | OAuth (PKCE), librespot session, playback, Spirc/Connect. |
| `crates/web` | Spotify Web API: search, library, catalogue, device list. |
| `crates/jam` | Group Session over the private `social-connect` API. Experimental. |
| `crates/daemon` | Binaries: `spotifyd-rs` (owns all state, serves IPC) and `spotify-ctl`. |
| `app/src-tauri` | Window. A thin, disposable view. |
| `app/ui` | HTML/CSS/JS front end. No framework, no build step. |

There are two OAuth identities, for the reason described above: the keymaster
id for streaming, and your own app id for the Web API. `crates/player/src/auth.rs`
models this as a `Profile`, so each has its own scopes and its own token cache.

## Working on the UI

The front end can be developed in a plain browser with no Rust build at all.
Serve `app/ui` with any static server and stub `window.__TAURI__` — see the
mock harness pattern in the project history. Because the daemon owns all
state, the UI has no logic to duplicate.

## What works

- Full-quality playback (320 kbps with Premium), gapless, with normalisation
- Spotify Connect: this app appears in the device list on your phone, and can
  hand playback off to any other device on the account
- Search, playlists, saved tracks and albums, album and artist pages
- Liking and unliking tracks
- Transport, shuffle, three-state repeat, volume, seek
- On-disk audio cache (capped at 4 GB) so repeat listens skip the network

## What does not, and why

These are not shortcuts taken during the build. They are limits imposed from
outside, listed so you know where the walls are.

**Spotify removed these Web API endpoints.** Nothing in this codebase can
bring them back:

- Artist **top tracks** — the artist page shows discography only
- Artist follower counts and genres
- Recommendations, radio, "Made For You", related artists
- Audio features and analysis, 30-second preview URLs
- `country` and `product` on the user profile — market now resolves from the
  token, and Premium status is read from the librespot session instead

**Jam is a private API.** It has no public documentation or support. Every
endpoint in `crates/jam` was derived from observing official clients, so:

- Joining an existing Jam is the better-understood path
- Hosting works but is the more likely half to break
- Responses parse permissively; a schema change degrades to missing data in
  the UI rather than crashing
- When it breaks, `crates/jam/src/lib.rs` is the only file to revisit

**Not implemented:** free-tier ad-supported playback (librespot does not
support it), podcast video, audiobooks, lyrics, and Canvas.

## Note on the lockfile

`Cargo.lock` pins `vergen` to 9.0.6. Upstream `vergen 9.1.0` moved to
`vergen-lib 9.1.0` while `vergen-gitcl 1.0` still requires `vergen-lib 0.1`,
and librespot-core's build script fails to compile with the combination cargo
picks by default. Do not run `cargo update -p vergen` without re-testing the
build.

## Legal

This uses reverse-engineered Spotify protocols via librespot, which is against
Spotify's Terms of Service, and reproduces Spotify's visual design. It is a
personal project. Do not distribute it, and do not present it as a Spotify
product.
