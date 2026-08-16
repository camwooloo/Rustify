/* UI glue.
 *
 * Kept deliberately thin. The daemon owns all playback state; this file
 * renders whatever it is told and forwards intent back. There is no local
 * model to drift out of sync, no client-side playback logic, and no
 * framework — the DOM below is the whole view layer.
 */

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

/* Native decorations are disabled, so the titlebar buttons drive the window
   directly. The drag region itself is declarative (data-tauri-drag-region). */
const appWindow = window.__TAURI__.window.getCurrentWindow();
document.getElementById("win-min").onclick = () => appWindow.minimize();
document.getElementById("win-max").onclick = () => appWindow.toggleMaximize();
// Hides rather than quits: the player keeps going and the tray icon stays.
// Quit properly from the tray menu.
document.getElementById("win-close").onclick = () => appWindow.hide();

/* Dragging is driven explicitly rather than with data-tauri-drag-region.
   The bar is a grid whose children cover it completely, so the attribute on
   the header never received the mousedown and the window would not move.
   Asking the window to drag on mousedown works wherever the pointer is,
   as long as it is not on something interactive. */
document.getElementById("titlebar").addEventListener("mousedown", (e) => {
  if (e.button !== 0) return;
  if (e.target.closest("button, input, a, [contenteditable]")) return;
  appWindow.startDragging();
});

// Double-clicking a titlebar maximises, as everywhere else in Windows.
document.getElementById("titlebar").addEventListener("dblclick", (e) => {
  if (e.target.closest("button, input")) return;
  appWindow.toggleMaximize();
});

/** True once the daemon socket is up. */
let connected = false;
let resolveConnected;
const connectedOnce = new Promise((r) => (resolveConnected = r));

/** Send a command to the daemon. Errors surface as toasts, never silently.
 *
 * "Not connected" is the exception: on a cold start the window is up before
 * the daemon is, and shouting about it produced a pile of red toasts during
 * what is really just normal startup.
 */
async function call(command) {
  try {
    return await invoke("call", { command });
  } catch (e) {
    const message = String(e);
    if (!/not connected|connection dropped/i.test(message)) {
      toast(message, "error");
    }
    throw e;
  }
}

/* ---------------------------------------------------------------- state */

/** Mirror of the daemon's PlayerState. Never mutated except from events. */
let state = null;
/** True while the user drags the seek bar, so ticks don't fight the pointer. */
let scrubbing = false;
let lastVolume = 32768;

const view = { name: "home", param: null };
/** True while the queue rail is open. */
let rail = false;
/** Cached lyrics for the current track, so scrolling does not refetch. */
let lyricsCache = { uri: null, data: null };
const history = [];
let historyIndex = -1;

/* ------------------------------------------------------------- helpers */

const $ = (sel) => document.querySelector(sel);
const el = (tag, props = {}, children = []) => {
  const node = Object.assign(document.createElement(tag), props);
  for (const child of [].concat(children)) {
    if (child != null) node.append(child);
  }
  return node;
};

/** Escape user/catalogue text before it reaches innerHTML. */
const esc = (s) =>
  String(s ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]
  );

const icon = (name, cls = "") =>
  `<svg class="${cls}"><use href="#i-${name}"/></svg>`;

function fmtTime(ms) {
  if (!Number.isFinite(ms) || ms < 0) ms = 0;
  const total = Math.floor(ms / 1000);
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}

const artistNames = (t) => (t.artists || []).map((a) => a.name).join(", ");

/** Placeholder when art is missing, so layout never jumps. */
const art = (url, cls = "") =>
  url
    ? `<img class="${cls}" src="${esc(url)}" alt="" loading="lazy">`
    : `<div class="thumb ${cls}"></div>`;

function toast(message, severity = "info") {
  const node = el("div", { className: `toast ${severity}`, textContent: message });
  $("#toasts").append(node);
  setTimeout(() => node.remove(), 4000);
}

/* --------------------------------------------------------- navigation */

function navigate(name, param = null, push = true) {
  view.name = name;
  view.param = param;
  if (push) {
    history.splice(historyIndex + 1);
    history.push({ name, param });
    historyIndex = history.length - 1;
  }
  updateNavButtons();
  syncNav();
  render();
}

function updateNavButtons() {
  $("#nav-back").disabled = historyIndex <= 0;
  $("#nav-fwd").disabled = historyIndex >= history.length - 1;
}

$("#nav-back").onclick = () => {
  if (historyIndex > 0) {
    historyIndex--;
    const h = history[historyIndex];
    navigate(h.name, h.param, false);
  }
};
$("#nav-fwd").onclick = () => {
  if (historyIndex < history.length - 1) {
    historyIndex++;
    const h = history[historyIndex];
    navigate(h.name, h.param, false);
  }
};

document.querySelectorAll("[data-nav]").forEach((btn) => {
  btn.onclick = () => navigate(btn.dataset.nav);
});

$("#btn-menu").onclick = () => navigate("settings");
$("#btn-home").onclick = () => navigate("home");
$("#btn-browse").onclick = () => navigate("browse");

$("#btn-account").onclick = () =>
  toast(
    state?.auth?.displayName
      ? `Signed in as ${state.auth.displayName}`
      : "Not signed in"
  );

// The bell shows what shipped in this version, with any recent notice
// underneath — the same thing the post-update popup says.
const notices = [];
$("#btn-notices").onclick = () => {
  const open = $("#popovers").querySelector("[data-whatsnew]");
  if (open) {
    open.remove();
    return;
  }
  closePopovers();

  const entry = CHANGELOG[0];
  const pop = el("div", { className: "popover top" });
  pop.dataset.whatsnew = "1";
  pop.style.width = "min(420px, 80vw)";
  pop.innerHTML = `
    <h3>What's new <span class="experimental" style="background:rgba(var(--accent-rgb),.16);
      border-color:rgba(var(--accent-rgb),.4);color:var(--accent)">v${esc(entry.v)}</span></h3>
    <p class="hint">${esc(entry.d)}</p>
    <ul style="margin:0;padding-left:18px">
      ${entry.notes
        .slice(0, 6)
        .map(
          (n) =>
            `<li style="color:var(--text-dim);font-size:12.5px;line-height:1.6;margin-bottom:5px">${esc(n)}</li>`
        )
        .join("")}
    </ul>
    ${
      notices.length
        ? `<p class="hint" style="margin-top:12px">Last message: ${esc(notices.at(-1))}</p>`
        : ""
    }`;
  $("#popovers").append(pop);
};

/** Keep the sidebar and the home button in step with the current view. */
function syncNav() {
  document
    .querySelectorAll("[data-nav]")
    .forEach((b) => b.classList.toggle("active", b.dataset.nav === view.name));
  $("#btn-home").classList.toggle("on", view.name === "home");
}

/* ------------------------------------------------------------ rendering */

async function render() {
  const content = $("#content");

  if (!state?.auth?.loggedIn) return renderSignedOut(content);

  // Playback works without the Web API, but every browsing view needs it.
  // Show the one-time setup instead of a screen full of errors.
  if (!state.auth.browsingReady) return renderBrowsingSetup(content);

  switch (view.name) {
    case "home":
      return renderHome(content);
    case "search":
      $("#search-input").focus();
      return renderSearch(content, view.param);
    case "playlist":
      return renderPlaylist(content, view.param);
    case "album":
      return renderAlbum(content, view.param);
    case "artist":
      return renderArtist(content, view.param);
    case "liked":
      return renderLiked(content);
    case "lyrics":
      return renderLyrics(content);
    case "settings":
      return renderSettings(content);
    case "browse":
      return renderBrowse(content);
    default:
      content.innerHTML = "";
  }
}

function renderSignedOut(content) {
  content.innerHTML = `
    <div class="center-note">
      <h2>Sign in to Spotify</h2>
      <p>Playback needs a Spotify Premium account.<br>
         Your browser will open to complete sign-in.</p>
      <button class="pill accent" id="do-login">Log in</button>
    </div>`;

  $("#do-login").onclick = async () => {
    // The daemon opens the browser: a WebView2 cannot, and window.open is
    // blocked by this app's CSP. The link is shown regardless so sign-in is
    // still possible if opening the browser fails.
    const res = await call({ cmd: "login" });
    if (!res?.authUrl) return;

    content.innerHTML = `
      <div class="center-note">
        <h2>Waiting for your browser</h2>
        <p>A browser tab should have opened. If it didn't,<br>
           copy this link and open it yourself:</p>
        <div class="jam-input" style="width:min(560px,80vw)">
          <input id="auth-url" readonly value="${esc(res.authUrl)}">
          <button class="pill" id="auth-copy">Copy</button>
        </div>
        <button class="pill accent" id="auth-retry">Open browser again</button>
      </div>`;

    $("#auth-url").onclick = (e) => e.target.select();

    $("#auth-copy").onclick = async () => {
      const field = $("#auth-url");
      try {
        await navigator.clipboard.writeText(field.value);
      } catch {
        // Clipboard access can be denied; selecting lets the user copy manually.
        field.select();
      }
      toast("Link copied");
    };

    // Safe to click repeatedly: the daemon re-opens the same pending flow.
    $("#auth-retry").onclick = () => call({ cmd: "login" });
  };
}

/** One-time setup for the Web API half. */
function renderBrowsingSetup(content) {
  const configured = state?.auth?.webClientConfigured;

  content.innerHTML = `
    <div class="center-note">
      <h2>One more step to browse</h2>
      <p style="max-width:56ch">
        Playback is working. Search, playlists and your library need Spotify to
        recognise you on Rustify's app &mdash; and Spotify caps that app at
        <strong>5 listeners</strong>, added by hand.
      </p>
      <p style="max-width:56ch;color:var(--text-muted)">
        Either ask <strong>camwooloo</strong> to add your Spotify account, then
        press Retry &mdash; or use your own free Spotify app below.
      </p>

      <div class="jam-input" style="width:min(520px,80vw)">
        <button class="pill accent" id="browse-retry">Retry</button>
        ${
          configured
            ? `<button class="pill" id="use-bundled">Use Rustify's app</button>`
            : ""
        }
      </div>

      <details style="max-width:56ch;text-align:left;margin-top:6px">
        <summary style="cursor:pointer;font-weight:700">Use my own Spotify app</summary>
        <ol style="line-height:2;color:var(--text-muted);padding-left:20px">
          <li>Open <strong>developer.spotify.com/dashboard</strong> and click
              <strong>Create app</strong> (any name)</li>
          <li>Paste this into <strong>Redirect URIs</strong>, then
              <strong>click Add</strong> &mdash; typing it alone is not enough
              and is the usual reason sign-in fails with
              <em>&ldquo;redirect_uri: Not matching configuration&rdquo;</em>:
            <div class="jam-input" style="margin:6px 0">
              <input id="redirect-uri" readonly
                     value="http://127.0.0.1:4382/login">
              <button class="pill" id="copy-redirect">Copy</button>
            </div>
          </li>
          <li>Tick <strong>Web API</strong>, agree to the terms, save, then copy
              the Client ID</li>
        </ol>
        <p style="color:var(--text-muted);font-size:12.5px">
          Note: an app registered today cannot reach Browse, New releases or an
          artist's Popular tracks &mdash; Spotify withdrew those for new apps.
          Everything else works.
        </p>
        <div class="jam-input">
          <input id="client-id" placeholder="Paste your Client ID"
                 spellcheck="false" autocomplete="off">
          <button class="pill accent" id="client-save">Save</button>
        </div>
      </details>

      ${
        configured
          ? `<p style="color:var(--text-muted)">A Client ID is saved but not yet
             authorised. Saving again reopens the browser.</p>`
          : ""
      }
      <button class="pill" id="skip-browsing">Skip &mdash; just use playback</button>
    </div>`;

  $("#copy-redirect").onclick = async () => {
    const field = $("#redirect-uri");
    try {
      await navigator.clipboard.writeText(field.value);
      toast("Redirect URI copied \u2014 remember to click Add in Spotify");
    } catch {
      field.select();
    }
  };

  // Retrying uses whatever app is configured, which is all someone needs
  // once they have been added to its listener list.
  $("#browse-retry").onclick = () => call({ cmd: "login" }).catch(() => {});

  // Clears a custom Client ID and falls back to the app Rustify ships with.
  const bundled = $("#use-bundled");
  if (bundled) {
    bundled.onclick = () =>
      call({ cmd: "setWebClientId", clientId: "" }).catch(() => {});
  }


  const submit = async () => {
    const clientId = $("#client-id").value.trim();
    if (!clientId) return;
    const res = await call({ cmd: "setWebClientId", clientId });
    if (!res?.authUrl) return;

    content.innerHTML = `
      <div class="center-note">
        <h2>Authorising browsing</h2>
        <p>Approve the app in your browser. If no tab opened, use this link:</p>
        <div class="jam-input" style="width:min(560px,80vw)">
          <input id="auth-url" readonly value="${esc(res.authUrl)}">
          <button class="pill" id="auth-copy">Copy</button>
        </div>
      </div>`;

    $("#auth-url").onclick = (e) => e.target.select();
    $("#auth-copy").onclick = async () => {
      try {
        await navigator.clipboard.writeText($("#auth-url").value);
      } catch {
        $("#auth-url").select();
      }
      toast("Link copied");
    };
  };

  $("#client-save").onclick = submit;
  $("#client-id").onkeydown = (e) => {
    if (e.key === "Enter") submit();
  };

  // Playback-only is a legitimate way to use this app; do not trap anyone here.
  $("#skip-browsing").onclick = () => {
    content.innerHTML = `
      <div class="center-note">
        <h2>Playback only</h2>
        <p style="max-width:48ch">Control this device from the Spotify app on
        your phone or desktop &mdash; it appears there as
        <strong>${esc(state?.deviceName || "this computer")}</strong>.</p>
        <button class="pill accent" id="setup-again">Set up browsing</button>
      </div>`;
    $("#setup-again").onclick = () => renderBrowsingSetup($("#content"));
  };
}

function skeletonGrid(n = 8) {
  return `<div class="grid">${Array.from(
    { length: n },
    () => `<div class="card"><div class="thumb skeleton"></div>
      <div class="skeleton" style="height:14px;margin-bottom:8px"></div>
      <div class="skeleton" style="height:12px;width:60%"></div></div>`
  ).join("")}</div>`;
}

function cardGrid(items, kind) {
  if (!items.length) return `<p style="color:var(--text-muted)">Nothing here yet.</p>`;
  return `<div class="grid">${items
    .map((it) => {
      const sub =
        kind === "artist"
          ? "Artist"
          : kind === "playlist"
            ? esc(it.owner || "")
            : artistNames(it) || "";
      return `<button class="card ${kind === "artist" ? "round" : ""}"
                style="--wash-h:${hueOf(it.id || it.uri || it.name)}"
                data-open="${kind}" data-id="${esc(it.id)}" data-uri="${esc(it.uri)}">
          ${art(it.coverUrl || it.imageUrl)}
          <div class="title">${esc(it.name)}</div>
          <div class="sub">${sub}</div>
          <span class="play">${icon("play")}</span>
        </button>`;
    })
    .join("")}</div>`;
}

/** Time-synced lyrics for whatever is playing. */
async function renderLyrics(content) {
  const track = state?.track;
  if (!track) {
    content.innerHTML = `<div class="center-note"><h2>Nothing playing</h2>
      <p>Start a track to see its lyrics.</p></div>`;
    return;
  }

  if (lyricsCache.uri !== track.uri) {
    content.innerHTML = `<div class="lyrics">${Array.from(
      { length: 8 },
      () => `<div class="skeleton" style="height:30px;margin:14px 0;width:${
        45 + Math.round(Math.random() * 40)
      }%"></div>`
    ).join("")}</div>`;

    try {
      lyricsCache = {
        uri: track.uri,
        data: await call({ cmd: "getLyrics", trackUri: track.uri }),
      };
    } catch (e) {
      return renderError(content, String(e));
    }
  }

  const lyrics = lyricsCache.data;
  if (!lyrics?.lines?.length) {
    content.innerHTML = `<div class="center-note"><h2>No lyrics</h2>
      <p>Spotify doesn't have lyrics for<br><strong>${esc(track.name)}</strong>.</p></div>`;
    return;
  }

  content.innerHTML = `
    <h2 class="section-title">${esc(track.name)}
      <span style="color:var(--text-mute);font-weight:600"> · ${esc(artistNames(track))}</span>
    </h2>
    <div class="lyrics" id="lyrics">
      ${lyrics.lines
        .map(
          (l, i) =>
            `<div class="lyric-line" data-i="${i}" data-t="${l.timeMs}">${esc(
              l.text || "\u266a"
            )}</div>`
        )
        .join("")}
      ${
        lyrics.provider
          ? `<p style="color:var(--text-mute);font-size:12px;margin-top:28px">
             Lyrics by ${esc(lyrics.provider)}</p>`
          : ""
      }
    </div>`;

  // Only synced lyrics can be seeked; unsynced ones have no timestamps.
  if (lyrics.synced) {
    content.querySelectorAll(".lyric-line").forEach((line) => {
      line.onclick = () =>
        call({ cmd: "seek", positionMs: Number(line.dataset.t) });
    });
  }
  highlightLyrics();
}

/** Move the active line as playback advances. */
function highlightLyrics() {
  if (view.name !== "lyrics" || !lyricsCache.data?.synced) return;

  const pos = state?.positionMs ?? 0;
  const lines = [...document.querySelectorAll(".lyric-line")];
  if (!lines.length) return;

  let activeIndex = -1;
  for (let i = 0; i < lines.length; i++) {
    if (Number(lines[i].dataset.t) <= pos) activeIndex = i;
    else break;
  }

  lines.forEach((line, i) => {
    line.classList.toggle("active", i === activeIndex);
    line.classList.toggle("passed", i < activeIndex);
  });

  const active = lines[activeIndex];
  if (active && active !== highlightLyrics.last) {
    highlightLyrics.last = active;
    active.scrollIntoView({ block: "center", behavior: "smooth" });
  }
}

/* -------------------------------------------------------- changelog */

/** Newest first. The top entry is what the bell and the post-update note
 *  show, and the only one expanded by default in Settings. */
const CHANGELOG = [
  {
    v: "0.3.0",
    d: "16 Aug 2026",
    notes: [
      "Playlist editing: right-click any song to add it to a playlist, create one on the spot, or remove it from a playlist you own",
      "Rustify restarts the player on its own if it stops, instead of waiting for you to reopen the app",
      "Fixed the whole class of layout bugs caused by unsized icons — the same fault behind the oversized song rows and Liked Songs tile",
    ],
  },
  {
    v: "0.2.7",
    d: "16 Aug 2026",
    notes: [
      "The ‘Starting the player’ strip now disappears once the player is up, instead of sitting there permanently",
      "Starting a Jam works while you are already in someone else’s — Rustify leaves theirs first, and the panel now names whose Jam you are in",
      "Fixed Jam failing outright when Spotify returned both of two session flags",
    ],
  },
  {
    v: "0.2.6",
    d: "16 Aug 2026",
    notes: [
      "Closing the window now keeps Rustify in the tray instead of quitting, so you can still skip and pause while it plays",
      "Launching Rustify again brings the existing window back rather than starting a second copy",
      "Fixed song rows rendering enormously tall in both themes",
    ],
  },
  {
    v: "0.2.5",
    d: "16 Aug 2026",
    notes: [
      "Updating no longer breaks the player: the installer now stops it first, which is why the previous update reported an error about rustifyd.exe and left Rustify unable to play",
      "Update checks now happen even when the player cannot start — previously the people an update would fix were the ones never offered it",
      "A missing player is reported plainly instead of pretending it will reconnect",
    ],
  },
  {
    v: "0.2.4",
    d: "16 Aug 2026",
    notes: [
      "Added a way back: if you entered your own Client ID you can now switch to Rustify's app in one click, which previously was impossible without editing files by hand",
      "Corrected the listener limit shown during setup — Spotify allows 5, not 25",
    ],
  },
  {
    v: "0.2.3",
    d: "16 Aug 2026",
    notes: [
      "Clearer setup when browsing is not yet available: a Retry button, a copyable redirect address, and a warning that Spotify discards it unless you click Add",
    ],
  },
  {
    v: "0.2.2",
    d: "16 Aug 2026",
    notes: [
      "You can now paste Spotify's short Jam links (spotify.link) to join — they are resolved to the real invite first",
      "Only Spotify's own link hosts are ever followed, so a pasted link cannot point Rustify somewhere else",
    ],
  },
  {
    v: "0.2.1",
    d: "16 Aug 2026",
    notes: [
      "New Look: an optional redesigned interface, switchable from the top of Settings",
      "Jam works again — creating a session was using the wrong request method, and the share link was an internal address rather than one you can send to a friend",
      "Fixed the updater comparing against the wrong version number, which would have offered an update you already had",
    ],
  },
  {
    v: "0.2.0",
    d: "16 Aug 2026",
    notes: [
      "New look: one unified top bar with search, a custom title bar you can drag, and a tray icon so you can skip tracks with the window closed",
      "Home rebuilt \u2014 quick picks, personalised radio stations, your top artists, on repeat, new releases and your library",
      "Browse all: the full Spotify genre grid is back",
      "Right-click anywhere for play, queue, radio, Jam and copy link",
      "Time-synced lyrics that follow the track and seek when you click a line",
      "Queue and Recently played in a resizable side panel, showing whatever device is actually playing",
      "Settings with audio quality, normalisation, cache controls and this changelog",
      "Rustify now mirrors playback from your phone or the official app",
    ],
  },
  {
    v: "0.1.0",
    d: "15 Aug 2026",
    notes: [
      "First build: librespot playback, Spotify Connect, search, playlists and library",
      "Player runs as a separate process so music keeps going with the window closed",
    ],
  },
];

const APP_VERSION = CHANGELOG[0].v;

function changelogHtml() {
  return CHANGELOG.map(
    (e, i) => `<details class="patch" ${i === 0 ? "open" : ""}>
      <summary>
        <span class="ver">v${esc(e.v)}</span>
        <span class="date">${esc(e.d)}</span>
        ${i === 0 ? `<span class="latest">Latest</span>` : ""}
      </summary>
      <ul>${e.notes.map((n) => `<li>${esc(n)}</li>`).join("")}</ul>
    </details>`
  ).join("");
}

/** Bottom-centre bar offering an update. */
function showUpdateBar(info) {
  if (!info || document.getElementById("updbar")) return;

  const bar = el("div", { className: "updbar", id: "updbar" });
  bar.innerHTML = `
    <span class="upd-i">${icon("browse")}</span>
    <span class="upd-t">Update <b>v${esc(info.version)}</b> is available</span>
    <button class="upd-go">Update now</button>
    <button class="upd-x" title="Later">${icon("x")}</button>`;
  document.body.append(bar);

  bar.querySelector(".upd-x").onclick = () => bar.remove();
  bar.querySelector(".upd-go").onclick = async () => {
    bar.classList.add("busy");
    bar.querySelector(".upd-go").textContent = "Downloading\u2026";
    try {
      await invoke("apply_update", { url: info.url });
    } catch (e) {
      bar.classList.remove("busy");
      bar.querySelector(".upd-go").textContent = "Update now";
      toast(`Update failed: ${e}`, "error");
    }
  };
}

/** After an update lands, say what changed. */
function showWhatsNew() {
  const seen = localStorage.getItem("rustify.version");
  localStorage.setItem("rustify.version", APP_VERSION);
  if (!seen || seen === APP_VERSION) return;

  const entry = CHANGELOG[0];
  const pop = el("div", { className: "popover top" });
  pop.style.cssText =
    "left:50%;right:auto;top:64px;transform:translateX(-50%);width:min(560px,86vw)";
  pop.innerHTML = `
    <h3>What's new in v${esc(entry.v)}</h3>
    <p class="hint">${esc(entry.d)}</p>
    <ul style="margin:0;padding-left:18px">
      ${entry.notes
        .map(
          (n) =>
            `<li style="color:var(--text-dim);font-size:13px;line-height:1.6;margin-bottom:5px">${esc(n)}</li>`
        )
        .join("")}
    </ul>
    <div class="jam-input"><button class="pill accent" id="whats-ok">Got it</button></div>`;
  $("#popovers").append(pop);
  pop.querySelector("#whats-ok").onclick = () => pop.remove();
}

/* --------------------------------------------------------- settings */

const fmtMB = (bytes) =>
  bytes > 1_073_741_824
    ? `${(bytes / 1_073_741_824).toFixed(1)} GB`
    : `${Math.round(bytes / 1_048_576)} MB`;

function setRow(label, desc, control) {
  return `<div class="set-row">
      <div class="label"><b>${esc(label)}</b>${desc ? `<p>${desc}</p>` : ""}</div>
      <div class="control">${control}</div>
    </div>`;
}

const toggleHtml = (id, on) =>
  `<button class="toggle ${on ? "on" : ""}" data-toggle="${id}"
     role="switch" aria-checked="${on}"></button>`;

async function renderSettings(content) {
  content.innerHTML = `<h1 class="greeting">Settings</h1>
    <div class="skeleton" style="height:80px"></div>`;

  let view;
  try {
    view = await call({ cmd: "getSettings" });
  } catch (e) {
    return renderError(content, String(e));
  }

  const st = view.settings;
  const zoom = Number(localStorage.getItem("rustify.zoom") || 100);

  content.innerHTML = `
    <h1 class="greeting">Settings</h1>
    <div class="settings">

      <div class="newlook-row">
        <div class="label">
          <b>New Look</b>
          <p>A redesigned interface: flat sidebar, segmented tabs, and a
             floating player bar with the controls on the left. Based on the
             Spotify Redesign community concept. Switches instantly.</p>
        </div>
        <div class="control">${toggleHtml("newLook", newLookOn())}</div>
      </div>

      <div class="set-group">Audio quality</div>
      ${setRow(
        "Streaming quality",
        "Higher quality uses more bandwidth. Premium is required for 320 kbps.",
        `<select data-set="bitrate">
           ${[96, 160, 320]
             .map(
               (b) =>
                 `<option value="${b}" ${b === st.bitrate ? "selected" : ""}>${
                   b === 320 ? "Very high (320 kbps)" : b === 160 ? "High (160 kbps)" : "Normal (96 kbps)"
                 }</option>`
             )
             .join("")}
         </select>`
      )}
      ${setRow(
        "Normalise volume",
        "Even out the loudness between tracks.",
        toggleHtml("normalise", st.normalise)
      )}

      <div class="set-group">Playback</div>
      ${setRow(
        "Autoplay",
        "Keep playing something similar when a playlist or album ends.",
        toggleHtml("autoplay", st.autoplay)
      )}

      <div class="set-group">Device</div>
      ${setRow(
        "Device name",
        "How this computer appears in Spotify Connect on your other devices.",
        `<input id="set-device" value="${esc(st.deviceName)}" spellcheck="false"
           style="padding:8px 12px;border-radius:10px;border:1px solid var(--stroke);
                  background:var(--glass-2);color:var(--text);outline:none;width:220px">`
      )}

      <div class="set-group">Display</div>
      ${setRow(
        "Zoom level",
        "Scales the whole interface. Applies immediately.",
        `<select data-zoom>
           ${[80, 90, 100, 110, 120, 130]
             .map((z) => `<option value="${z}" ${z === zoom ? "selected" : ""}>${z}%</option>`)
             .join("")}
         </select>`
      )}

      <div class="set-group">Storage</div>
      ${setRow(
        "Cache audio on disk",
        "Repeat listens skip the network. Capped at 4 GB.",
        toggleHtml("cacheAudio", st.cacheAudio)
      )}
      ${setRow(
        "Cached audio",
        `Currently using <strong>${fmtMB(view.cacheBytes)}</strong>.`,
        `<button class="pill" id="set-clear-cache">Clear cache</button>`
      )}

      <div class="set-group">What's new</div>
      ${changelogHtml()}

      <div class="set-group">About</div>
      ${setRow(
        "Rustify",
        `Rustify v${esc(APP_VERSION)} \u00b7 daemon ${esc(view.daemonVersion)}.
         Made by <strong>camwooloo</strong>.`,
        `<button class="pill" id="set-site">camwooloo.com</button>`
      )}
      <div class="set-note">
        <div class="set-mono">config &nbsp;${esc(view.configPath)}</div>
        <div class="set-mono">log &nbsp;&nbsp;&nbsp;&nbsp;${esc(view.logPath)}</div>
      </div>

      <div class="set-note">
        <strong>Not shown here on purpose.</strong> Explicit-content filtering,
        Canvas, listening-activity sharing and crossfade are account or
        server-side settings that Spotify does not expose to third-party apps
        &mdash; a toggle for them here would do nothing. Change those in the
        official client.
      </div>
    </div>`;

  // -- wiring ---------------------------------------------------------
  const collect = () => ({
    bitrate: Number(content.querySelector('[data-set="bitrate"]').value),
    normalise: content.querySelector('[data-toggle="normalise"]').classList.contains("on"),
    autoplay: content.querySelector('[data-toggle="autoplay"]').classList.contains("on"),
    cacheAudio: content.querySelector('[data-toggle="cacheAudio"]').classList.contains("on"),
    // newLook is deliberately absent: it is a UI preference, not playback.
    deviceName: content.querySelector("#set-device").value.trim(),
  });

  const push = async () => {
    try {
      await call({ cmd: "setSettings", ...collect() });
    } catch {
      /* the daemon reports the reason itself */
    }
  };

  content.querySelectorAll("[data-toggle]").forEach((b) => {
    b.onclick = () => {
      b.classList.toggle("on");
      const on = b.classList.contains("on");
      b.setAttribute("aria-checked", on);

      // This one is a local theme flag, not a daemon setting.
      if (b.dataset.toggle === "newLook") {
        applyNewLook(on);
        return;
      }
      push();
    };
  });

  content.querySelector('[data-set="bitrate"]').onchange = push;
  content.querySelector("#set-device").onchange = push;

  content.querySelector("[data-zoom]").onchange = (e) => {
    applyZoom(Number(e.target.value));
  };

  content.querySelector("#set-site").onclick = () =>
    call({ cmd: "openExternal", url: "https://camwooloo.com" });

  content.querySelector("#set-clear-cache").onclick = async () => {
    await call({ cmd: "clearCache" });
    render();
  };
}

/** Switch between the default skin and the New Look.
 *
 * Purely presentational, so it lives in the UI rather than the daemon: it is
 * a class on <body> that a second stylesheet keys off, which means switching
 * is instant and the default theme is never touched.
 */
function applyNewLook(on) {
  localStorage.setItem("rustify.newLook", on ? "1" : "0");
  document.body.classList.toggle("newlook", !!on);
  // Card art in the New Look carries a coloured cap derived from the item,
  // so anything already on screen needs repainting.
  render();
}

const newLookOn = () => localStorage.getItem("rustify.newLook") === "1";

/** Interface scale. Kept in the UI because it is purely presentational. */
function applyZoom(percent) {
  localStorage.setItem("rustify.zoom", String(percent));
  document.documentElement.style.fontSize = `${(percent / 100) * 14}px`;
  document.body.style.zoom = `${percent}%`;
}

applyZoom(Number(localStorage.getItem("rustify.zoom") || 100));
document.body.classList.toggle("newlook", newLookOn());

/** Render a failure the user can act on, instead of an empty screen. */
function renderError(content, message) {
  content.innerHTML = `
    <div class="center-note">
      <h2>Couldn't load that</h2>
      <p style="max-width:46ch">${esc(message)}</p>
      <button class="pill accent" id="retry">Try again</button>
    </div>`;
  $("#retry").onclick = () => render();
}

function greeting() {
  const h = new Date().getHours();
  if (h < 12) return "Good morning";
  if (h < 18) return "Good afternoon";
  return "Good evening";
}

/** A shelf of cards with an optional "Show all", omitted when empty. */
function shelf(title, items, kind, opts = {}) {
  if (!items?.length) return "";
  const limited = opts.limit ? items.slice(0, opts.limit) : items;
  return `<div class="shelf-head">
      <h2>${esc(title)}</h2>
      ${opts.note ? `<span class="more">${esc(opts.note)}</span>` : ""}
      <span class="spacer"></span>
      ${
        opts.showAll && items.length > limited.length
          ? `<button class="show-all" data-all="${esc(opts.showAll)}">Show all</button>`
          : ""
      }
    </div>
    ${cardGrid(limited, kind)}`;
}

/** Spotify-made playlists sitting in the listener's own library.
 *
 * Daily Mixes and radio stations cannot be requested: `/recommendations` and
 * `featured-playlists` are gone from the Web API entirely. But the ones the
 * listener already follows are ordinary playlists, so the shelves can be
 * rebuilt from the library rather than faked.
 */
function partitionPlaylists(all) {
  const dailyMixes = [];
  const madeFor = [];
  const stations = [];
  const mixes = [];
  const mine = [];

  for (const p of all) {
    const name = (p.name || "").trim();
    const lower = name.toLowerCase();
    // Ownership is the reliable signal. Matching on the name alone put any
    // playlist merely *containing* those words into "Made for you".
    const bySpotify = (p.owner || "").trim().toLowerCase() === "spotify";

    if (bySpotify && /^daily mix\s*\d*$/.test(lower)) dailyMixes.push(p);
    else if (bySpotify && /\bradio\b/.test(lower)) stations.push(p);
    else if (
      bySpotify &&
      /discover weekly|release radar|on repeat|repeat rewind|time capsule|your top songs/.test(lower)
    )
      madeFor.push(p);
    else if (bySpotify && /\bmix\b/.test(lower)) mixes.push(p);
    else mine.push(p);
  }
  return { dailyMixes, madeFor, stations, mixes, mine };
}

async function renderHome(content) {
  content.innerHTML = `<h1 class="greeting">${greeting()}</h1>${skeletonGrid(8)}`;

  // Every shelf is independent, so one dead endpoint must not take the page
  // with it. Only a total wipeout is reported as an error.
  const [recent, playlists, albums, topArtists, topTracks, newReleases, stationsRes] =
    await Promise.allSettled([
      call({ cmd: "getRecentlyPlayed", limit: 8 }),
      call({ cmd: "getPlaylists", limit: 50 }),
      call({ cmd: "getSavedAlbums", limit: 20 }),
      call({ cmd: "getTopArtists", limit: 12 }),
      call({ cmd: "getTopTracks", limit: 12 }),
      call({ cmd: "getNewReleases", limit: 12 }),
      call({ cmd: "getStations", limit: 6 }),
    ]);

  const results = [recent, playlists, albums, topArtists, topTracks];
  if (results.every((r) => r.status === "rejected")) {
    return renderError(content, String(playlists.reason));
  }

  const val = (r) => (r.status === "fulfilled" ? r.value : null);
  const name = state?.auth?.displayName || "you";
  const { dailyMixes, madeFor, stations, mixes, mine } = partitionPlaylists(
    val(playlists)?.items ?? []
  );

  // Quick picks: the wide tiles across the top of the real home page.
  const quick = (val(recent)?.items ?? []).slice(0, 8);
  const quickHtml = quick.length
    ? `<div class="quick-grid">${quick
        .map(
          (t) => `<button class="quick" data-track="${esc(t.uri)}">
            ${art(t.coverUrl)}
            <div class="title">${esc(t.name)}</div>
          </button>`
        )
        .join("")}</div>`
    : "";

  content.innerHTML = `
    <h1 class="greeting">${greeting()}</h1>
    ${quickHtml}
    ${shelf(`Made for ${name}`, val(stationsRes)?.items ?? [], "playlist", {
      note: "Stations from your top artists",
      limit: 6,
    })}
    ${shelf("Your daily mixes", dailyMixes, "playlist", { limit: 6 })}
    ${shelf("Recommended stations", stations, "playlist", {
      note: "From your library",
      limit: 6,
      showAll: "playlists",
    })}
    ${shelf(`More for ${name}`, madeFor, "playlist", { limit: 6, showAll: "playlists" })}
    ${shelf("Your top mixes", mixes, "playlist", { limit: 6, showAll: "playlists" })}
    ${shelf("Your top artists", val(topArtists)?.items ?? [], "artist", {
      note: "Last 6 months",
      limit: 6,
    })}
    ${shelf("On repeat", val(topTracks)?.items ?? [], "track", { limit: 6 })}
    ${shelf("New releases", val(newReleases)?.items ?? [], "album", { limit: 6 })}
    ${shelf("Your playlists", mine, "playlist", { limit: 6, showAll: "playlists" })}
    ${shelf("Your albums", val(albums)?.items ?? [], "album", { limit: 6 })}`;

  content.querySelectorAll("[data-track]").forEach((b) => {
    b.onclick = () =>
      call({ cmd: "loadTracks", uris: [b.dataset.track], startPlaying: true });
  });
  content.querySelectorAll("[data-all]").forEach((b) => {
    b.onclick = () => navigate("browse");
  });
  wireCards(content);
}

/** The genre grid behind the search bar's browse button. */
async function renderBrowse(content) {
  content.innerHTML = `<h1 class="greeting">Browse all</h1>${skeletonGrid(9)}`;

  let items = [];
  try {
    items = (await call({ cmd: "getCategories" }))?.items ?? [];
  } catch (e) {
    return renderError(
      content,
      `${e}\n\nBrowse categories need a Spotify app registered before ` +
        `November 2024. Newer apps get 403 for this endpoint.`
    );
  }

  if (!items.length) {
    content.innerHTML = `<div class="center-note"><h2>Nothing to browse</h2>
      <p>Spotify returned no categories for this account.</p></div>`;
    return;
  }

  // Each tile gets a stable colour from its id, mirroring Spotify's own
  // coloured category cards.
  content.innerHTML = `
    <h1 class="greeting">Browse all</h1>
    <div class="cat-grid">
      ${items
        .map(
          (c) => `<button class="cat" data-cat="${esc(c.name)}"
              style="--cat-h:${hueOf(c.id)}">
            <span class="cat-name">${esc(c.name)}</span>
            ${c.iconUrl ? `<img src="${esc(c.iconUrl)}" alt="">` : ""}
          </button>`
        )
        .join("")}
    </div>`;

  // Category playlists are a 404 even on a grandfathered app, so a tile runs
  // a search for the genre instead of opening a category page that cannot
  // be populated.
  content.querySelectorAll("[data-cat]").forEach((b) => {
    b.onclick = () => {
      $("#search-input").value = b.dataset.cat;
      navigate("search", b.dataset.cat);
    };
  });
}

/** Render a failure the user can act on, instead of an empty screen. */
function renderError(content, message) {
  content.innerHTML = `
    <div class="center-note">
      <h2>Couldn't load that</h2>
      <p style="max-width:46ch">${esc(message)}</p>
      <button class="pill accent" id="retry">Try again</button>
    </div>`;
  $("#retry").onclick = () => render();
}

function greeting() {
  const h = new Date().getHours();
  if (h < 12) return "Good morning";
  if (h < 18) return "Good afternoon";
  return "Good evening";
}

/** A shelf of cards, omitted entirely when it has nothing to show. */
function shelf(title, items, kind, note) {
  if (!items?.length) return "";
  return `<div class="shelf-head"><h2>${esc(title)}</h2>
      ${note ? `<span class="more">${esc(note)}</span>` : ""}</div>
    ${cardGrid(items, kind)}`;
}

async function renderHome(content) {
  content.innerHTML = `<h1 class="greeting">${greeting()}</h1>${skeletonGrid(8)}`;

  // Every shelf is independent, so one failing endpoint must not take the
  // page down with it. Only a total wipeout is reported as an error.
  const [recent, playlists, albums, topArtists, topTracks] =
    await Promise.allSettled([
      call({ cmd: "getRecentlyPlayed", limit: 8 }),
      call({ cmd: "getPlaylists", limit: 12 }),
      call({ cmd: "getSavedAlbums", limit: 12 }),
      call({ cmd: "getTopArtists", limit: 12 }),
      call({ cmd: "getTopTracks", limit: 12 }),
    ]);

  const results = [recent, playlists, albums, topArtists, topTracks];
  if (results.every((r) => r.status === "rejected")) {
    return renderError(content, String(playlists.reason));
  }

  const val = (r) => (r.status === "fulfilled" ? r.value : null);
  const quick = val(recent)?.items ?? [];

  const quickHtml = quick.length
    ? `<div class="quick-grid">${quick
        .map(
          (t) => `<button class="quick" data-track="${esc(t.uri)}">
            ${art(t.coverUrl)}
            <div class="title">${esc(t.name)}</div>
          </button>`
        )
        .join("")}</div>`
    : "";

  content.innerHTML = `
    <h1 class="greeting">${greeting()}</h1>
    ${quickHtml}
    ${shelf("Your top artists", val(topArtists)?.items ?? [], "artist", "Last 6 months")}
    ${shelf("On repeat", val(topTracks)?.items ?? [], "track", "Your top tracks")}
    ${shelf("Your playlists", val(playlists)?.items ?? [], "playlist")}
    ${shelf("Your albums", val(albums)?.items ?? [], "album")}`;

  content.querySelectorAll("[data-track]").forEach((b) => {
    b.onclick = () =>
      call({ cmd: "loadTracks", uris: [b.dataset.track], startPlaying: true });
  });
  wireCards(content);
}

let searchTimer = null;
$("#search-input").addEventListener("input", (e) => {
  const q = e.target.value.trim();
  // The field is always visible now, so typing has to bring the view along.
  if (q && view.name !== "search") navigate("search", q);
  clearTimeout(searchTimer);
  // Debounced: one request per pause in typing, not per keystroke.
  searchTimer = setTimeout(() => {
    view.param = q;
    renderSearch($("#content"), q);
  }, 250);
});

async function renderSearch(content, query) {
  if (!query) {
    content.innerHTML = `<h2 class="section-title">Search Spotify</h2>
      <p style="color:var(--text-muted)">Find songs, albums, artists and playlists.</p>`;
    return;
  }
  content.innerHTML = skeletonGrid(6);
  let res;
  try {
    res = await call({ cmd: "search", query, limit: 20 });
  } catch (e) {
    return renderError(content, String(e));
  }

  content.innerHTML = `
    ${res.tracks?.length ? `<h2 class="section-title">Songs</h2>${trackTable(res.tracks)}` : ""}
    ${res.artists?.length ? `<h2 class="section-title">Artists</h2>${cardGrid(res.artists, "artist")}` : ""}
    ${res.albums?.length ? `<h2 class="section-title">Albums</h2>${cardGrid(res.albums, "album")}` : ""}
    ${res.playlists?.length ? `<h2 class="section-title">Playlists</h2>${cardGrid(res.playlists, "playlist")}` : ""}`;
  wireCards(content);
  wireTracks(content, res.tracks || []);
}

function trackTable(tracks) {
  const rows = tracks
    .map((t, i) => {
      const current = state?.track?.uri && state.track.uri === t.uri;
      return `<div class="track-row ${current ? "current" : ""}" data-play="${i}">
        <div class="idx">${i + 1}<span class="idx-play">${icon("play")}</span></div>
        <div class="cell-title">
          ${art(t.coverUrl)}
          <div style="min-width:0">
            <div class="name">${esc(t.name)}</div>
            <div class="artist">${esc(artistNames(t))}</div>
          </div>
        </div>
        <div class="album">${esc(t.album?.name || "")}</div>
        <div class="added">${esc(t.addedAt || "")}</div>
        <button class="heart ${t.saved ? "on" : ""}" data-save="${i}" title="Save">
          ${icon(t.saved ? "heart" : "heart-o")}
        </button>
        <div class="dur">${fmtTime(t.durationMs)}</div>
      </div>`;
    })
    .join("");

  return `<div class="track-head">
      <div style="text-align:right">#</div>
      <div>Title</div>
      <div>Album</div>
      <div class="col-added">Date added</div>
      <div></div>
      <div style="text-align:right">Time</div>
    </div>${rows}`;
}

function wireCards(root) {
  root.querySelectorAll("[data-open]").forEach((node) => {
    node.onclick = (e) => {
      // Track cards have no detail page, so the whole card plays.
      if (node.dataset.open === "track") {
        call({ cmd: "loadTracks", uris: [node.dataset.uri], startPlaying: true });
        return;
      }
      // The hover play button starts playback; the card body navigates.
      if (e.target.closest(".play")) {
        e.stopPropagation();
        call({ cmd: "loadContext", uri: node.dataset.uri, startPlaying: true });
        return;
      }
      navigate(node.dataset.open, node.dataset.id);
    };
  });
}

/** Wire a rendered track table to playback + save actions. */
function wireTracks(root, tracks, contextUri = null) {
  root.querySelectorAll("[data-play]").forEach((row) => {
    row.ondblclick = row.onclick = (e) => {
      if (e.target.closest("[data-save]")) return;
      const i = Number(row.dataset.play);
      if (contextUri) {
        call({ cmd: "loadContext", uri: contextUri, index: i, startPlaying: true });
      } else {
        call({ cmd: "loadTracks", uris: [tracks[i].uri], startPlaying: true });
      }
    };
  });

  root.querySelectorAll("[data-save]").forEach((btn) => {
    btn.onclick = async (e) => {
      e.stopPropagation();
      const track = tracks[Number(btn.dataset.save)];
      const saved = !btn.classList.contains("on");
      // Optimistic: the daemon is authoritative, but the heart must feel instant.
      btn.classList.toggle("on", saved);
      btn.innerHTML = icon(saved ? "heart" : "heart-o");
      try {
        await call({ cmd: "setSaved", uri: track.uri, saved });
        track.saved = saved;
      } catch {
        btn.classList.toggle("on", !saved);
        btn.innerHTML = icon(!saved ? "heart" : "heart-o");
      }
    };
  });
}

/** Deterministic hue from an id, so a page keeps the same colour. */
function hueOf(seed) {
  let h = 0;
  for (const ch of String(seed)) h = (h * 31 + ch.charCodeAt(0)) % 360;
  return h;
}

function detailHead({ kind, name, sub, cover, round, seed, saved }) {
  return `<div class="detail-wrap" style="--wash-h:${hueOf(seed || name)}">
      <div class="detail-head">
        ${art(cover, round ? "round" : "")}
        <div>
          <div class="kind">${esc(kind)}</div>
          <h1>${esc(name)}</h1>
          <div class="sub">${sub}</div>
        </div>
      </div>
      <div class="detail-actions">
        <button class="play-big" id="play-context" title="Play">${icon("play")}</button>
        <button class="act" id="ctx-shuffle" title="Shuffle">${icon("shuffle")}</button>
        <button class="act ${saved ? "on" : ""}" id="ctx-save" title="Save">
          ${icon(saved ? "heart" : "heart-o")}
        </button>
        <button class="act" id="ctx-lyrics" title="Lyrics">${icon("lyrics")}</button>
      </div>
    </div>`;
}

/** Shared wiring for the buttons `detailHead` renders. */
function wireDetail(uri) {
  const shuffle = $("#ctx-shuffle");
  if (shuffle) {
    shuffle.onclick = () =>
      call({ cmd: "loadContext", uri, startPlaying: true, shuffle: true });
  }
  const lyrics = $("#ctx-lyrics");
  if (lyrics) lyrics.onclick = () => navigate("lyrics");
}

async function renderPlaylist(content, id) {
  content.innerHTML = skeletonGrid(4);
  const [meta, tracksRes] = await Promise.allSettled([
    call({ cmd: "getPlaylists", limit: 50 }),
    call({ cmd: "getPlaylistTracks", id, limit: 50 }),
  ]);
  if (tracksRes.status === "rejected") {
    return renderError(content, String(tracksRes.reason));
  }
  const tracks = tracksRes.value;

  const pl = meta.value?.items?.find((p) => p.id === id) || { name: "Playlist" };
  const uri = pl.uri || `spotify:playlist:${id}`;

  const me = (state?.auth?.displayName || "").toLowerCase();
  currentPlaylistIsMine = !!me && (pl.owner || "").toLowerCase() === me;

  content.innerHTML =
    detailHead({
      kind: "Public Playlist",
      name: pl.name,
      cover: pl.coverUrl,
      seed: id,
      sub: `<strong>${esc(pl.owner || "")}</strong> · ${tracks.total} songs`,
    }) + trackTable(tracks.items);

  $("#play-context").onclick = () =>
    call({ cmd: "loadContext", uri, startPlaying: true });
  wireDetail(uri);
  wireTracks(content, tracks.items, uri);
}

async function renderAlbum(content, id) {
  content.innerHTML = skeletonGrid(4);
  let album;
  try {
    album = await call({ cmd: "getAlbum", id });
  } catch (e) {
    return renderError(content, String(e));
  }

  content.innerHTML =
    detailHead({
      kind: "Album",
      name: album.name,
      cover: album.coverUrl,
      seed: album.id,
      sub: `<strong>${esc(artistNames(album))}</strong> · ${album.releaseDate ?? ""} · ${album.totalTracks} songs`,
    }) + trackTable(album.tracks);

  $("#play-context").onclick = () =>
    call({ cmd: "loadContext", uri: album.uri, startPlaying: true });
  wireDetail(album.uri);
  wireTracks(content, album.tracks, album.uri);
}

async function renderArtist(content, id) {
  content.innerHTML = skeletonGrid(4);
  let artist;
  try {
    artist = await call({ cmd: "getArtist", id });
  } catch (e) {
    return renderError(content, String(e));
  }

  content.innerHTML =
    detailHead({
      kind: "Artist",
      name: artist.name,
      cover: artist.imageUrl,
      round: true,
      seed: artist.id,
      sub: "",
    }) +
    // "Popular" returns when the endpoint is reachable; on a newly
    // registered Spotify app it 403s and the shelf simply does not appear.
    (artist.topTracks?.length
      ? `<h2 class="section-title">Popular</h2>${trackTable(artist.topTracks.slice(0, 5))}`
      : "") +
    `<h2 class="section-title">Discography</h2>${cardGrid(artist.albums || [], "album")}`;

  $("#play-context").onclick = () =>
    call({ cmd: "loadContext", uri: artist.uri, startPlaying: true });
  wireDetail(artist.uri);
  if (artist.topTracks?.length) wireTracks(content, artist.topTracks.slice(0, 5));
  wireCards(content);
}

async function renderLiked(content) {
  content.innerHTML = skeletonGrid(4);
  let res;
  try {
    res = await call({ cmd: "getSavedTracks", limit: 50 });
  } catch (e) {
    return renderError(content, String(e));
  }

  content.innerHTML =
    detailHead({
      kind: "Playlist",
      name: "Liked Songs",
      cover: null,
      seed: "liked",
      sub: `<strong>${esc(state?.auth?.displayName || "You")}</strong> · ${res.total} songs`,
    }) + trackTable(res.items);

  $("#play-context").onclick = () =>
    call({ cmd: "loadTracks", uris: res.items.map((t) => t.uri), startPlaying: true });
  wireTracks(content, res.items);
}

/** Which library filter is active: "playlists" | "albums". */
let libFilter = "playlists";

function librarySkeleton(n = 7) {
  return Array.from(
    { length: n },
    () => `<div class="library-row">
      <div class="thumb skeleton"></div>
      <div class="meta" style="flex:1">
        <div class="skeleton" style="height:12px;width:70%;margin-bottom:6px"></div>
        <div class="skeleton" style="height:10px;width:40%"></div>
      </div>
    </div>`
  ).join("");
}

async function renderLibrary() {
  const list = $("#library-list");

  document
    .querySelectorAll("[data-lib]")
    .forEach((b) => b.classList.toggle("active", b.dataset.lib === libFilter));

  if (!state?.auth?.loggedIn) {
    list.innerHTML = "";
    return;
  }

  list.innerHTML = librarySkeleton();

  if (libFilter === "albums") {
    let items = [];
    try {
      items = (await call({ cmd: "getSavedAlbums", limit: 50 }))?.items ?? [];
    } catch {
      list.innerHTML = "";
      return;
    }
    list.innerHTML = items
      .map(
        (a) => `<button class="library-row" data-album="${esc(a.id)}"
                  data-uri="${esc(a.uri)}">
          ${art(a.coverUrl)}
          <div class="meta"><div class="title">${esc(a.name)}</div>
          <div class="sub">Album \u00b7 ${esc(artistNames(a))}</div></div>
        </button>`
      )
      .join("");
    list.querySelectorAll("[data-album]").forEach((b) => {
      b.onclick = () => navigate("album", b.dataset.album);
    });
    return;
  }

  // Playlists. The sidebar tolerates failure quietly: the main view already
  // reports the same error, and two copies of it would be noise.
  const res = await call({ cmd: "getPlaylists", limit: 50 }).catch(() => null);

  const liked = `<button class="library-row" data-liked>
      <div class="thumb" style="background:linear-gradient(135deg,#450af5,#c4efd9);
        display:grid;place-items:center">
        <svg style="width:18px;height:18px;fill:#fff"><use href="#i-heart"/></svg>
      </div>
      <div class="meta"><div class="title">Liked Songs</div>
      <div class="sub">Playlist \u00b7 ${esc(state?.auth?.displayName || "You")}</div></div>
    </button>`;

  list.innerHTML =
    liked +
    (res?.items ?? [])
      .map(
        (p) => `<button class="library-row" data-pl="${esc(p.id)}">
          ${art(p.coverUrl)}
          <div class="meta"><div class="title">${esc(p.name)}</div>
          <div class="sub">Playlist \u00b7 ${esc(p.owner)}</div></div>
        </button>`
      )
      .join("");

  list.querySelector("[data-liked]").onclick = () => navigate("liked");
  list.querySelectorAll("[data-pl]").forEach((b) => {
    b.onclick = () => navigate("playlist", b.dataset.pl);
  });
}

document.querySelectorAll("[data-lib]").forEach((b) => {
  b.onclick = () => {
    libFilter = b.dataset.lib;
    renderLibrary();
  };
});

/* --------------------------------------------------------- resizing */

/** Make a panel edge draggable, persisting the width across restarts. */
function makeResizable(gutterId, cssVar, storageKey, min, max, fromRight) {
  const gutter = $(`#${gutterId}`);
  if (!gutter) return;

  const shell = $("#shell");
  const saved = Number(localStorage.getItem(storageKey));
  if (saved >= min && saved <= max) {
    shell.style.setProperty(cssVar, `${saved}px`);
  }

  gutter.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    gutter.classList.add("dragging");
    document.body.classList.add("resizing");

    const move = (ev) => {
      const rect = shell.getBoundingClientRect();
      const raw = fromRight ? rect.right - ev.clientX : ev.clientX - rect.left;
      const width = Math.round(Math.min(max, Math.max(min, raw)));
      shell.style.setProperty(cssVar, `${width}px`);
    };

    const up = () => {
      gutter.classList.remove("dragging");
      document.body.classList.remove("resizing");
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      const current = parseInt(shell.style.getPropertyValue(cssVar), 10);
      if (current) localStorage.setItem(storageKey, String(current));
    };

    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  });

  // Double-click restores the default, which is otherwise hard to find again.
  gutter.addEventListener("dblclick", () => {
    shell.style.removeProperty(cssVar);
    localStorage.removeItem(storageKey);
  });
}

makeResizable("gutter-side", "--side-w", "rustify.sideWidth", 200, 460, false);
makeResizable("gutter-rail", "--rail-w", "rustify.railWidth", 240, 520, true);

/* ------------------------------------------------------------- rail */

/** Which rail tab is showing: "queue" | "recent". */
let railTab = "queue";

function setRail(open) {
  rail = open === undefined ? !rail : !!open;
  $("#rail").hidden = !rail;
  $("#shell").classList.toggle("with-rail", rail);
  $("#btn-queue").classList.toggle("on", rail);
  if (rail) renderRail();
}

/** One row in the rail. */
function railRow(t, opts = {}) {
  return `<button class="friend ${opts.current ? "current" : ""}"
            data-uri="${esc(t.uri)}">
      ${art(t.coverUrl)}
      <div class="meta">
        <div class="who">${esc(t.name)}</div>
        <div class="what">${esc(artistNames(t))}</div>
      </div>
    </button>`;
}

function wireRailRows(root) {
  root.querySelectorAll("[data-uri]").forEach((b) => {
    b.onclick = () =>
      call({ cmd: "loadTracks", uris: [b.dataset.uri], startPlaying: true });
  });
}

/** Queue / recently played, reflecting the account's active device. */
async function renderRail() {
  const body = $("#rail-body");

  document
    .querySelectorAll(".rail-tab")
    .forEach((b) => b.classList.toggle("active", b.dataset.tab === railTab));

  if (railTab === "recent") {
    body.innerHTML = `<p class="rail-empty">Loading\u2026</p>`;
    try {
      const items = (await call({ cmd: "getRecentlyPlayed", limit: 30 }))?.items ?? [];
      body.innerHTML = items.length
        ? items.map((t) => railRow(t)).join("")
        : `<p class="rail-empty">Nothing played recently.</p>`;
    } catch (e) {
      body.innerHTML = `<p class="rail-empty">${esc(String(e))}</p>`;
    }
    wireRailRows(body);
    return;
  }

  const now = state?.track;
  const nowHtml = now
    ? `<div class="rail-section">Now playing</div>${railRow(now, { current: true })}`
    : "";

  // Paint what we already know before the request returns, so the panel is
  // never blank while the queue loads.
  body.innerHTML = `${nowHtml}<div class="rail-section">Next up</div>
    <p class="rail-empty">Loading\u2026</p>`;

  let items = [];
  let contextName = null;
  try {
    const res = await call({ cmd: "getQueue" });
    items = res?.items ?? [];
    contextName = res?.contextName ?? null;
  } catch (e) {
    body.innerHTML = `${nowHtml}<div class="rail-section">Next up</div>
      <p class="rail-empty">${esc(String(e))}</p>`;
    wireRailRows(body);
    return;
  }

  // Spotify's own UI splits user-queued items from the context's upcoming
  // tracks, but the Web API returns one merged list with no flag telling them
  // apart — so label the section with the context instead of guessing.
  const heading = contextName ? `Next from: ${esc(contextName)}` : "Next up";

  body.innerHTML = `${nowHtml}
    <div class="rail-section">${heading}</div>
    ${
      items.length
        ? items.map((t) => railRow(t)).join("")
        : `<p class="rail-empty">Nothing queued.</p>`
    }`;
  wireRailRows(body);
}

document.querySelectorAll(".rail-tab").forEach((b) => {
  b.onclick = () => {
    railTab = b.dataset.tab;
    renderRail();
  };
});

$("#rail-close").onclick = () => setRail(false);
$("#btn-queue").onclick = () => setRail();

/* ------------------------------------------------------ context menu */

const ctx = $("#ctxmenu");

function closeCtx() {
  ctx.hidden = true;
}

document.addEventListener("click", closeCtx);
document.addEventListener("scroll", closeCtx, true);
window.addEventListener("blur", closeCtx);

/** Show a menu at the pointer, clamped to stay on screen. */
function openCtx(x, y, items) {
  ctx.innerHTML = items
    .map((it) =>
      it === "-"
        ? `<div class="sep"></div>`
        : `<button data-act="${esc(it.id)}">${icon(it.icon)}${esc(it.label)}</button>`
    )
    .join("");

  ctx.hidden = false;
  const box = ctx.getBoundingClientRect();
  ctx.style.left = `${Math.min(x, window.innerWidth - box.width - 8)}px`;
  ctx.style.top = `${Math.min(y, window.innerHeight - box.height - 8)}px`;

  ctx.querySelectorAll("[data-act]").forEach((b) => {
    b.onclick = (e) => {
      e.stopPropagation();
      closeCtx();
      items.find((i) => i !== "-" && i.id === b.dataset.act)?.run?.();
    };
  });
}

/** True when the playlist currently open is one the user can edit. */
let currentPlaylistIsMine = false;

/** Cached playlist list for the "Add to playlist" picker. */
let playlistCache = null;

async function ownPlaylists() {
  if (playlistCache) return playlistCache;
  const res = await call({ cmd: "getPlaylists", limit: 50 });
  const me = (state?.auth?.displayName || "").toLowerCase();
  // Only playlists you can actually write to.
  playlistCache = (res?.items ?? []).filter(
    (p) => !me || (p.owner || "").toLowerCase() === me
  );
  return playlistCache;
}

/** Pick a playlist to add a track to, or make a new one. */
async function addToPlaylistDialog(uri) {
  closePopovers();
  const pop = el("div", { className: "popover top" });
  pop.style.cssText =
    "left:50%;right:auto;top:64px;transform:translateX(-50%);width:min(420px,86vw)";
  pop.innerHTML = `<h3>Add to playlist</h3>
    <p class="hint">Loading your playlists\u2026</p>`;
  $("#popovers").append(pop);

  let items = [];
  try {
    items = await ownPlaylists();
  } catch (e) {
    pop.innerHTML = `<h3>Add to playlist</h3><p class="hint">${esc(String(e))}</p>`;
    return;
  }

  pop.innerHTML = `<h3>Add to playlist</h3>
    <div class="jam-input">
      <input id="new-pl" placeholder="New playlist name" spellcheck="false">
      <button class="pill accent" id="new-pl-go">Create</button>
    </div>
    <div style="max-height:46vh;overflow-y:auto;margin-top:10px">
      ${
        items.length
          ? items
              .map(
                (p) => `<button class="device-row" data-add="${esc(p.id)}">
                  ${art(p.coverUrl)}
                  <div><div>${esc(p.name)}</div>
                  <div class="hint" style="margin:0">${p.totalTracks} songs</div></div>
                </button>`
              )
              .join("")
          : `<p class="hint">You don't have any playlists you can edit yet.</p>`
      }
    </div>`;

  const add = async (playlistId) => {
    try {
      await call({ cmd: "addToPlaylist", playlistId, uris: [uri] });
      toast("Added to playlist");
      playlistCache = null; // counts changed
      closePopovers();
    } catch {
      /* the daemon reported why */
    }
  };

  pop.querySelectorAll("[data-add]").forEach((b) => {
    b.onclick = () => add(b.dataset.add);
  });

  pop.querySelector("#new-pl-go").onclick = async () => {
    const name = pop.querySelector("#new-pl").value.trim();
    if (!name) return;
    try {
      const created = await call({ cmd: "createPlaylist", name });
      playlistCache = null;
      await add(created.id);
      renderLibrary();
    } catch {
      /* reported by the daemon */
    }
  };
}

/** Copy an https link for a Spotify URI. */
async function copyLink(uri) {
  const [, kind, id] = String(uri).split(":");
  const url = `https://open.spotify.com/${kind}/${id}`;
  try {
    await navigator.clipboard.writeText(url);
    toast("Link copied");
  } catch {
    toast(url);
  }
}

document.addEventListener("contextmenu", (e) => {
  // Always ours, never the webview's. The browser menu ("Back", "Refresh",
  // "Save as", "Print") is meaningless inside an app window and looked
  // obviously wrong when it appeared over the UI.
  e.preventDefault();

  // Let text fields keep a useful menu of their own.
  if (e.target.closest("input, textarea, [contenteditable]")) {
    openCtx(e.clientX, e.clientY, [
      {
        id: "paste",
        icon: "browse",
        label: "Paste",
        run: async () => {
          try {
            const text = await navigator.clipboard.readText();
            const el = e.target;
            el.value = text;
            el.dispatchEvent(new Event("input", { bubbles: true }));
            el.dispatchEvent(new Event("change", { bubbles: true }));
          } catch {
            toast("Clipboard is not available");
          }
        },
      },
    ]);
    return;
  }

  const row = e.target.closest(
    "[data-uri], [data-play], [data-pl], [data-album], [data-open], [data-track]"
  );

  const uri =
    row?.dataset.uri ||
    row?.dataset.play ||
    row?.dataset.track ||
    (row?.dataset.pl ? `spotify:playlist:${row.dataset.pl}` : null);

  // Nothing playable under the cursor: offer the page-level actions rather
  // than no menu at all.
  if (!uri) {
    openCtx(e.clientX, e.clientY, [
      { id: "home", icon: "home", label: "Home", run: () => navigate("home") },
      { id: "browse", icon: "browse", label: "Browse all", run: () => navigate("browse") },
      { id: "queue", icon: "queue", label: "Show queue", run: () => setRail(true) },
      "-",
      { id: "settings", icon: "devices", label: "Settings", run: () => navigate("settings") },
    ]);
    return;
  }

  const isTrack = uri.includes(":track:");
  const items = [];

  if (isTrack) {
    items.push({
      id: "play",
      icon: "play",
      label: "Play",
      run: () => call({ cmd: "loadTracks", uris: [uri], startPlaying: true }),
    });
    items.push({
      id: "queue",
      icon: "queue",
      label: "Add to queue",
      run: async () => {
        await call({ cmd: "addToQueue", uri });
        toast("Added to queue");
        if (rail) renderRail();
      },
    });
    items.push({
      id: "addpl",
      icon: "queue",
      label: "Add to playlist\u2026",
      run: () => addToPlaylistDialog(uri),
    });
    items.push({
      id: "save",
      icon: "heart",
      label: "Save to your Liked Songs",
      run: async () => {
        await call({ cmd: "setSaved", uri, saved: true });
        toast("Saved");
      },
    });
  } else {
    items.push({
      id: "play",
      icon: "play",
      label: "Play",
      run: () => call({ cmd: "loadContext", uri, startPlaying: true }),
    });
    items.push({
      id: "shuffle",
      icon: "shuffle",
      label: "Shuffle play",
      run: () =>
        call({ cmd: "loadContext", uri, startPlaying: true, shuffle: true }),
    });
  }

  items.push({
    id: "radio",
    icon: "shuffle",
    label: "Go to radio",
    run: async () => {
      toast("Building radio\u2026");
      try {
        await call({ cmd: "startRadio", seedUri: uri });
      } catch {
        /* the daemon already reported why */
      }
    },
  });

  // Only offered where it makes sense: a track, inside a playlist you own.
  if (isTrack && view.name === "playlist" && currentPlaylistIsMine) {
    items.push({
      id: "removepl",
      icon: "x",
      label: "Remove from this playlist",
      run: async () => {
        try {
          await call({
            cmd: "removeFromPlaylist",
            playlistId: view.param,
            uris: [uri],
          });
          toast("Removed");
          playlistCache = null;
          render();
        } catch {
          /* reported by the daemon */
        }
      },
    });
  }

  items.push("-");
  items.push({
    id: "jam",
    icon: "jam",
    label: "Start a Jam",
    run: async () => {
      try {
        await call({ cmd: "jamCreate" });
        toast("Jam started");
      } catch {
        /* the daemon already reported why */
      }
    },
  });
  items.push({
    id: "copy",
    icon: "browse",
    label: "Copy link",
    run: () => copyLink(uri),
  });

  openCtx(e.clientX, e.clientY, items);
});

/* ------------------------------------------------------- now playing */

function renderNowPlaying() {
  const np = $("#np");
  const t = state?.track;

  // When another device owns playback, say which one — otherwise the bar
  // looks like it is playing locally when it is not.
  const remote = state?.remoteDevice;

  np.innerHTML = t
    ? `${art(t.coverUrl)}
       <div class="meta">
         <div class="name">${esc(t.name)}</div>
         <div class="artist">${esc(artistNames(t))}</div>
         ${
           remote
             ? `<span class="remote-chip">${icon("devices")}
                Playing on ${esc(remote)}</span>`
             : ""
         }
       </div>
       <button class="heart ${t.saved ? "on" : ""}" id="np-heart">
         ${icon(t.saved ? "heart" : "heart-o")}
       </button>`
    : "";

  const heart = $("#np-heart");
  if (heart) {
    heart.onclick = () =>
      call({ cmd: "setSaved", uri: t.uri, saved: !t.saved });
  }

  $("#btn-play").innerHTML = icon(state?.playing ? "pause" : "play");
  $("#btn-play").title = state?.playing ? "Pause" : "Play";
  $("#btn-shuffle").classList.toggle("on", !!state?.shuffle);

  const repeat = state?.repeat ?? "off";
  $("#btn-repeat").classList.toggle("on", repeat !== "off");
  $("#btn-repeat").innerHTML = icon(repeat === "track" ? "repeat1" : "repeat");

  $("#btn-jam").classList.toggle("on", !!state?.jam?.active);
  $("#btn-lyrics").classList.toggle("on", view.name === "lyrics");
  $("#btn-queue").classList.toggle("on", rail);

  const auth = state?.auth;
  const name = auth?.displayName || auth?.username || "";
  $("#btn-account").innerHTML = auth?.avatarUrl
    ? `<img src="${esc(auth.avatarUrl)}" alt="">`
    : esc(name.slice(0, 1).toUpperCase() || "?");

  updateProgress();
  updateVolume();
}

function updateProgress() {
  if (scrubbing) return;
  const dur = state?.track?.durationMs ?? 0;
  const pos = Math.min(state?.positionMs ?? 0, dur || Infinity);
  const seek = $("#seek");
  const pct = dur ? (pos / dur) * 100 : 0;
  seek.value = dur ? Math.round((pos / dur) * 1000) : 0;
  seek.style.setProperty("--pct", `${pct}%`);
  $("#time-now").textContent = fmtTime(pos);
  $("#time-total").textContent = fmtTime(dur);
}

function updateVolume() {
  const vol = $("#volume");
  const v = state?.volume ?? 0;
  vol.value = v;
  vol.style.setProperty("--pct", `${(v / 65535) * 100}%`);
}

/* --------------------------------------------------------- transport */

$("#btn-play").onclick = () => call({ cmd: "playPause" });
$("#btn-next").onclick = () => call({ cmd: "next" });
$("#btn-prev").onclick = () => call({ cmd: "previous" });
$("#btn-shuffle").onclick = () =>
  call({ cmd: "setShuffle", enabled: !state?.shuffle });
$("#btn-repeat").onclick = () => {
  const order = ["off", "context", "track"];
  const next = order[(order.indexOf(state?.repeat ?? "off") + 1) % 3];
  call({ cmd: "setRepeat", mode: next });
};

const seek = $("#seek");
seek.addEventListener("pointerdown", () => (scrubbing = true));
seek.addEventListener("input", () => {
  seek.style.setProperty("--pct", `${(seek.value / 1000) * 100}%`);
  const dur = state?.track?.durationMs ?? 0;
  $("#time-now").textContent = fmtTime((seek.value / 1000) * dur);
});
seek.addEventListener("change", () => {
  const dur = state?.track?.durationMs ?? 0;
  scrubbing = false;
  if (dur) call({ cmd: "seek", positionMs: Math.round((seek.value / 1000) * dur) });
});

const volume = $("#volume");
volume.addEventListener("input", () => {
  volume.style.setProperty("--pct", `${(volume.value / 65535) * 100}%`);
});
volume.addEventListener("change", () =>
  call({ cmd: "setVolume", volume: Number(volume.value) })
);

$("#btn-mute").onclick = () => {
  const v = Number(volume.value);
  if (v > 0) {
    lastVolume = v;
    call({ cmd: "setVolume", volume: 0 });
  } else {
    call({ cmd: "setVolume", volume: lastVolume });
  }
};

/* Space toggles playback unless the user is typing. */
document.addEventListener("keydown", (e) => {
  if (e.code !== "Space" || e.target.matches("input, textarea")) return;
  e.preventDefault();
  call({ cmd: "playPause" });
});

/* -------------------------------------------------- devices and jam */

function closePopovers() {
  $("#popovers").innerHTML = "";
}

document.addEventListener("click", (e) => {
  if (!e.target.closest(".popover") && !e.target.closest("#btn-devices, #btn-jam")) {
    closePopovers();
  }
});

$("#btn-devices").onclick = async (e) => {
  e.stopPropagation();
  if ($("#popovers").querySelector("[data-devices]")) return closePopovers();
  closePopovers();

  const pop = el("div", { className: "popover" });
  pop.dataset.devices = "1";
  pop.innerHTML = `<h3>Connect to a device</h3>
    <p class="hint">Playing on <strong>${esc(state?.deviceName || "this computer")}</strong></p>
    <div class="hint">Loading devices…</div>`;
  $("#popovers").append(pop);

  const res = await call({ cmd: "listDevices" }).catch(() => null);
  if (!res) return;

  pop.innerHTML =
    `<h3>Connect to a device</h3>
     <p class="hint">Pick where to play. This app is itself a Spotify Connect
     device, so your phone can control it too.</p>` +
    (res.items || [])
      .map(
        (d) => `<button class="device-row ${d.isActive ? "active" : ""}"
            data-dev="${esc(d.id)}">
            ${icon("devices")}
            <div><div>${esc(d.name)}${d.isSelf ? " (this app)" : ""}</div>
            <div class="hint" style="margin:0">${esc(d.deviceType)}${
              d.isActive ? " · Active" : ""
            }</div></div>
          </button>`
      )
      .join("");

  pop.querySelectorAll("[data-dev]").forEach((b) => {
    b.onclick = async () => {
      await call({ cmd: "transferPlayback", deviceId: b.dataset.dev, play: true });
      closePopovers();
      toast("Playback transferred");
    };
  });
};

$("#btn-jam").onclick = async (e) => {
  e.stopPropagation();
  if ($("#popovers").querySelector("[data-jam]")) return closePopovers();
  closePopovers();

  const pop = el("div", { className: "popover top" });
  pop.dataset.jam = "1";
  $("#popovers").append(pop);

  const draw = (jam) => {
    const active = jam?.active;
    pop.innerHTML = `
      <h3>Jam<span class="experimental">Experimental</span></h3>
      <p class="hint">${
        active && !jam.isHost
          ? `You're in ${esc(
              (jam.participants || []).find((p) => p.isHost)?.displayName ||
                "someone else"
            )}'s Jam.`
          : "Listen together in real time."
      } Jam uses a private Spotify API with no public support, so it can break
      without warning.</p>
      ${
        active
          ? `${(jam.participants || [])
              .map(
                (p) => `<div class="jam-row">
                  ${art(p.imageUrl)}
                  <div><div>${esc(p.displayName)}</div>
                  <div class="hint" style="margin:0">${
                    p.isHost ? "Host" : "Listener"
                  }</div></div>
                </div>`
              )
              .join("")}
             ${
               // Show the link whenever there is one. Spotify's ownership
               // flag is not dependable — a session we just created came back
               // with isHost false — and gating on it would hide the very
               // link the host needs to share.
               jam.joinUrl
                 ? `<div class="jam-input">
                      <input id="jam-link" readonly value="${esc(jam.joinUrl)}">
                      <button class="pill" id="jam-copy">Copy</button>
                    </div>`
                 : ""
             }
             <div class="jam-input">
               <button class="pill" id="jam-leave">Leave Jam</button>
               ${
                 jam.isHost
                   ? ""
                   : `<button class="pill accent" id="jam-start">Start your own</button>`
               }
             </div>`
          : `<div class="jam-input">
               <button class="pill accent" id="jam-start">Start a Jam</button>
             </div>
             <div class="jam-input">
               <input id="jam-join-link" placeholder="Paste a Jam link">
               <button class="pill" id="jam-join">Join</button>
             </div>`
      }`;

    const on = (id, fn) => {
      const node = pop.querySelector(id);
      if (node) node.onclick = fn;
    };

    on("#jam-start", async () => draw(await call({ cmd: "jamCreate" })));
    on("#jam-leave", async () => draw(await call({ cmd: "jamLeave" })));
    on("#jam-join", async () => {
      const link = pop.querySelector("#jam-join-link").value.trim();
      if (!link) return;
      draw(await call({ cmd: "jamJoin", link }));
    });
    on("#jam-copy", async () => {
      await navigator.clipboard.writeText(jam.joinUrl);
      toast("Jam link copied");
    });
  };

  draw(state?.jam);
  // Refresh from the server; the cached state may predate a change.
  call({ cmd: "jamStatus" })
    .then((jam) => {
      if (pop.isConnected) draw(jam);
    })
    .catch(() => {});
};

/* ------------------------------------------------------------- events */

let lastEventAt = 0;

const eventsReady = listen("daemon-event", ({ payload }) => {
  lastEventAt = Date.now();
  // Frames arrive flattened: {kind:"event", event:"state", ...fields}. The
  // frame *is* the event, so use it directly — `payload.event` is the tag
  // string, not a nested object.
  const ev = payload;
  switch (ev.event) {
    case "state": {
      const wasLoggedIn = state?.auth?.loggedIn;
      state = ev;
      renderNowPlaying();
      if (state.auth?.loggedIn !== wasLoggedIn) {
        renderLibrary();
        render();
      }
      break;
    }
    case "position":
      if (state) {
        state.positionMs = ev.positionMs;
        state.playing = ev.playing;
      }
      updateProgress();
      highlightLyrics();
      $("#btn-play").innerHTML = icon(ev.playing ? "pause" : "play");
      break;
    case "volume":
      if (state) state.volume = ev.volume;
      updateVolume();
      break;
    case "trackChanged":
      if (state) state.track = ev;
      renderNowPlaying();
      if (rail && railTab === "queue") renderRail();
      // A new track invalidates cached lyrics.
      if (lyricsCache.uri !== ev.uri) lyricsCache = { uri: null, data: null };
      if (view.name === "lyrics") render();
      // Re-render so the highlighted row follows the track.
      if (["playlist", "album", "liked"].includes(view.name)) render();
      break;
    case "authChanged":
      if (state) state.auth = ev;
      renderNowPlaying();
      renderLibrary();
      render();
      break;
    case "jam":
      if (state) state.jam = ev.active ? ev : null;
      renderNowPlaying();
      break;
    case "notice":
      notices.push(ev.message);
      toast(ev.message, ev.severity);
      break;
  }
});

const statusReady = listen("daemon-status", ({ payload }) => {
  connected = !!payload.connected;
  if (payload.fatal) fatalReason = payload.fatal;
  if (connected) fatalReason = null;
  setConnecting(!connected);

  if (connected) {
    resolveConnected();
    // Reconnected: our snapshot is stale by definition, so pull a fresh one.
    resync();
    // The rail may have tried to load before the socket existed.
    if (rail) renderRail();
  }
});

/* -------------------------------------------------------------- boot */

/** Show or hide the quiet "connecting" strip.
 *
 * A cold start always passes through this: the app launches the daemon, which
 * takes a few seconds to establish a Spotify session. That is the normal path,
 * so it reads as progress rather than failure.
 */
let connectingSince = 0;
/** Set when the player cannot start at all, rather than being merely slow. */
let fatalReason = null;

function setConnecting(on) {
  const bar = $("#offline");
  bar.hidden = !on;
  if (!on) {
    connectingSince = 0;
    return;
  }
  if (!connectingSince) connectingSince = Date.now();

  // A missing binary will not fix itself, so say so immediately rather than
  // implying it might recover.
  if (fatalReason) {
    bar.classList.add("error");
    bar.innerHTML = `<span>The player is missing — reinstall Rustify,
      making sure it isn't running first.</span>`;
    return;
  }

  // Only call it a problem once it has clearly taken too long.
  const stuck = Date.now() - connectingSince > 15000;
  bar.classList.toggle("error", stuck);
  bar.innerHTML = stuck
    ? "Can't reach the player. It should restart on its own\u2026"
    : `<span class="spinner"></span><span>Starting the player\u2026</span>`;
}

/** Pull a full snapshot and repaint. Safe to call at any time. */
async function resync() {
  let next;
  try {
    next = await call({ cmd: "getState" });
  } catch {
    return; // offline; the banner and the watchdog cover it
  }

  const first = state === null;
  const wasLoggedIn = state?.auth?.loggedIn;
  const wasBrowsing = state?.auth?.browsingReady;
  state = next;

  renderNowPlaying();

  // First successful snapshot after a slow start: paint everything.
  if (first) {
    setConnecting(false);
    renderLibrary();
    render();
    setRail(true);
    return;
  }

  // Only rebuild the main view when something structural moved, so a resync
  // never yanks the page out from under someone mid-scroll.
  if (state.auth?.loggedIn !== wasLoggedIn || state.auth?.browsingReady !== wasBrowsing) {
    renderLibrary();
    render();
  }
}

(async function boot() {
  // Paint structure straight away so the window is never empty while the
  // daemon starts up.
  setConnecting(true);
  $("#library-list").innerHTML = librarySkeleton();
  $("#content").innerHTML = `<h1 class="greeting">\u00a0</h1>${skeletonGrid(8)}`;

  // `listen` registers asynchronously. Awaiting it before the first fetch
  // closes the window where an event could arrive with no listener attached.
  await Promise.all([eventsReady, statusReady]).catch(() => {});

  // Nothing may be requested until the socket exists. Firing commands into a
  // dead link was what produced the "not connected to the daemon" pile-up and
  // left the page stuck on skeletons.
  // Check GitHub *before* waiting on the daemon. This used to sit after the
  // connection gate, so anyone whose player was broken — exactly the people
  // an update would fix — was never offered one.
  invoke("check_update")
    .then((info) => info && showUpdateBar(info))
    .catch(() => {});

  const timedOut = await Promise.race([
    connectedOnce.then(() => false),
    new Promise((r) => setTimeout(() => r(true), 30000)),
  ]);

  if (timedOut) {
    setConnecting(true); // shows the stuck wording after 15s
    return; // the link layer keeps retrying; `resync` takes over on connect
  }

  try {
    await call({ cmd: "hello", protocol: 2 });
    state = await call({ cmd: "getState" });
  } catch {
    // The link layer retries; the watchdog below picks things up.
  }

  setConnecting(false);
  renderNowPlaying();
  await renderLibrary();
  navigate("home");
  setRail(true);

  showWhatsNew();

  // Watchdog. Events are the fast path, not the only path: if none arrives
  // for a while, reconcile against the daemon anyway.
  setInterval(() => {
    if (Date.now() - lastEventAt > 4000) resync();
    if (!$("#offline").hidden) setConnecting(true); // refresh the wording
  }, 4000);
})();
