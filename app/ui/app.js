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

/* An image whose address 404s draws the browser's broken-image glyph: a torn
 * page in the corner of an otherwise styled circle. Every rule in the
 * stylesheet sizes `img` and `.thumb` together, so swapping the one for the
 * other loses the glyph and keeps the layout.
 *
 * stats.fm is where this shows up: it hands out a Spotify avatar address for
 * every profile, including the ones whose owner never set a picture, and that
 * address is a 404 rather than a default image. Error events on images do not
 * bubble, hence the capture. */
document.addEventListener(
  "error",
  (event) => {
    const img = event.target;
    if (!(img instanceof HTMLImageElement)) return;

    const thumb = el("div", { className: `thumb ${img.className}`.trim() });
    // Some placeholders say what is missing rather than sitting empty.
    if (img.dataset.fallback) thumb.innerHTML = icon(img.dataset.fallback);
    img.replaceWith(thumb);
  },
  true
);

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
$("#btn-stats").onclick = () => navigate("stats");
$("#btn-hub").onclick = () => navigate("hub", "extensions");

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
  // Lit while the lyrics view is open, so the toggle reads as a toggle.
  $("#btn-lyrics").classList.toggle("on", view.name === "lyrics");
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
    case "stats":
      return renderStats(content);
    case "hub":
      return renderHub(content, view.param);
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
    v: "1.1.0",
    d: "24 Aug 2026",
    notes: [
      "A visualiser, beside the miniplayer and full screen: bars, mirrored, radial or a wave, in theme, spectrum or mono colour, over the artwork or a glow",
      "It is drawn from the audio itself. The player runs the analysis at the same point the equaliser sits, and only while a visualiser is open",
      "A new logo: the ring is a bar visualiser now, and the planet wears the sound",
    ],
  },
  {
    v: "1.0.0",
    d: "24 Aug 2026",
    notes: [
      "Extensions: parts of Rustify that ship switched off, on their own page in the top left. Discord Rich Presence shows what you are playing with a link to the project, and a five-band equaliser sits between the decoder and your speakers — Spotify's own desktop client has neither",
      "Themes moved there too, out of the settings list",
      "The Spicetify marketplace browser is gone. Extensions and apps there run in the official client, not here, so listing them was offering something this app cannot give",
      "Playlists are properly editable: rename, describe, delete, and drag rows to reorder them",
      "A new logo, and a README worth reading",
    ],
  },
  {
    v: "0.9.0",
    d: "24 Aug 2026",
    notes: [
      "Rustify runs on macOS and Linux now: every release carries three downloads, one per platform",
      "Search results filter by Songs, Artists, Albums or Playlists, and the filter stays put while you refine the search",
      "On macOS and Linux the update button opens the download rather than installing itself, since only the Windows installer can do that unattended",
    ],
  },
  {
    v: "0.8.0",
    d: "23 Aug 2026",
    notes: [
      "A miniplayer and a full screen view, from the two buttons to the right of the volume, as in the real client",
      "The miniplayer is a small window that stays above everything else, with the artwork, the controls and a progress line — and it wears whatever theme you have on",
      "Full screen fills the window with the artwork and keeps the controls where they are; Escape leaves",
      "The volume slider is short again. It has carried a width since it was written and never used it: the rule setting it was outweighed, so the slider stretched to fill whatever room was left",
      "The lyrics button is a plain upright microphone rather than the tilted one, which at that size read as a paintbrush",
      "Updates are checked every couple of hours rather than only at launch, so a release that lands while Rustify is open is offered without a restart",
    ],
  },
  {
    v: "0.7.0",
    d: "23 Aug 2026",
    notes: [
      "A Marketplace, from the cart button next to the back and forward arrows: the same catalogue Spicetify's own marketplace lists — themes, extensions, snippets, apps — with search, sorting and previews",
      "Any theme there applies its colours to Rustify on the spot, whichever of its schemes you pick",
      "The Installed tab shows themes Spicetify has put on this computer; those apply here too",
      "Extensions, apps and snippets are listed to browse and open on GitHub. They are written against the official client's markup and APIs, so they run there rather than here",
    ],
  },
  {
    v: "0.6.2",
    d: "23 Aug 2026",
    notes: [
      "Playing a song from Liked Songs carries on through the rest of the list instead of stopping after it. Search results and an artist's top tracks were doing the same thing and are fixed with it",
    ],
  },
  {
    v: "0.6.1",
    d: "23 Aug 2026",
    notes: [
      "Themes have a page of their own now, reached from Settings, rather than a gallery to scroll past in the middle of the settings list",
      "A profile with no picture shows a placeholder instead of a broken image: stats.fm hands out a Spotify avatar address for everyone, including those who never set one",
    ],
  },
  {
    v: "0.6.0",
    d: "23 Aug 2026",
    notes: [
      "Listening stats, from the chart icon in the top bar: connect a stats.fm profile to see hours listened, top tracks, artists and albums over four weeks, six months or all time, and everything you last played",
      "Every row plays: a top track starts it, an artist or album opens its page here",
      "Themes installed by Spicetify itself now appear in the gallery beside the ones from the repository, marked as installed",
    ],
  },
  {
    v: "0.5.0",
    d: "23 Aug 2026",
    notes: [
      "Rustify now looks like Spotify: black shell, dark grey panels, flat cards and small corners, using the client's own colours and spacing rather than the glass look it had",
      "Spicetify themes are built in. Settings → Appearance browses the community theme repository — 14 themes, 111 colour schemes — and applies any of them to the whole app. No Spicetify install needed",
      "Only a theme's colours carry over. Its CSS is written against the official client's markup, which Rustify does not share, so it is never fetched",
      "The New Look option has gone",
    ],
  },
  {
    v: "0.4.1",
    d: "23 Aug 2026",
    notes: [
      "Each downloaded update now keeps its own name and the previous one is cleared away, so an installer that has not finished with its file cannot block the next update",
    ],
  },
  {
    v: "0.4.0",
    d: "23 Aug 2026",
    notes: [
      "Updates now install themselves: one click, a progress bar, and Rustify restarts on the new version — no setup window, no Next buttons",
      "This was the last update that opened the installer. The new one is already in place for whatever comes next",
    ],
  },
  {
    v: "0.3.9",
    d: "18 Aug 2026",
    notes: [
      "The song playing at the bottom now has a heart and an add-to-playlist button of its own",
      "Right-clicking it gives the song's full menu — queue, playlist, radio, artist and album — instead of the page menu",
      "Leaving a Jam you started now actually ends it. Before, the session stayed alive with you still in it and there was no way to close it from Rustify",
    ],
  },
  {
    v: "0.3.8",
    d: "17 Aug 2026",
    notes: [
      "Right-clicking a song now gives you the song's own menu — Add to queue, Add to playlist, Go to song radio, Go to artist and Go to album. Every song list was handing out the menu meant for albums and playlists instead",
      "The save entry knows whether the song is already in your library, and offers to remove it when it is",
    ],
  },
  {
    v: "0.3.7",
    d: "17 Aug 2026",
    notes: [
      "Hovering Rustify in the taskbar now shows media buttons — like, previous, play/pause and next — the same row the official app has",
      "Play, pause, skip and the rest now work while the music is playing on another device; before, every control was quietly ignored unless Rustify itself was playing",
      "The heart in the now-playing bar shows whether the track is in your library, and no longer hides itself until it is",
    ],
  },
  {
    v: "0.3.6",
    d: "16 Aug 2026",
    notes: [
      "The lyrics button works again — its click handler had been lost in an earlier change, leaving the button inert even though the lyrics view behind it was fine",
      "The lyrics button now lights up while lyrics are open, and uses a clearer microphone icon",
    ],
  },
  {
    v: "0.3.5",
    d: "16 Aug 2026",
    notes: [
      "Rustify now reconnects to Spotify by itself when the connection drops — previously music simply stopped after a few hours and stayed stopped until you restarted the player",
    ],
  },
  {
    v: "0.3.4",
    d: "16 Aug 2026",
    notes: [
      "Fixed Rustify claiming it could not reach the player after you quit and reopened it, while the player was in fact running and connected",
    ],
  },
  {
    v: "0.3.3",
    d: "16 Aug 2026",
    notes: [
      "New Look album and playlist pages now match the design: artwork on the right, a left-aligned title, and a cleaner track list ending in duration and the heart",
      "The top bar no longer pushes the window buttons off the edge in a narrow window",
    ],
  },
  {
    v: "0.3.2",
    d: "16 Aug 2026",
    notes: [
      "Media keys now work anywhere, not just when Rustify has focus",
      "Rustify appears in the Windows media overlay with artwork, title and artist — including when the music is playing on another device",
      "Controls keep working while Rustify sits in the tray",
    ],
  },
  {
    v: "0.3.1",
    d: "16 Aug 2026",
    notes: [
      "Fixed repeated network activity: a redundant player now exits immediately instead of opening a Spotify session and registering a device before giving up",
      "Rustify no longer polls Spotify while its window is hidden — closing to the tray used to leave it checking every few seconds around the clock",
    ],
  },
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

/* Whether this build can install an update itself. Asked once, so the update
 * bar can decide what its button does without waiting on a promise. */
let canSelfInstall = true;
invoke("update_installs_itself")
  .then((self) => {
    canSelfInstall = !!self;
  })
  .catch(() => {});

/** Ask GitHub whether there is a newer release, and offer it if so. */
function checkForUpdate() {
  invoke("check_update")
    .then((info) => info && showUpdateBar(info))
    .catch(() => {
      /* No network, no release, no matter. */
    });
}

/** Bottom-centre bar offering an update, and then showing it happening. */
function showUpdateBar(info) {
  if (!info || document.getElementById("updbar")) return;

  const bar = el("div", { className: "updbar", id: "updbar" });
  bar.innerHTML = `
    <span class="upd-i">${icon("browse")}</span>
    <span class="upd-t">Update <b>v${esc(info.version)}</b> is available</span>
    <button class="upd-go">Update now</button>
    <button class="upd-x" title="Later">${icon("x")}</button>
    <span class="upd-bar"><span class="upd-fill"></span></span>`;
  document.body.append(bar);

  const text = bar.querySelector(".upd-t");
  const fill = bar.querySelector(".upd-fill");

  bar.querySelector(".upd-x").onclick = () => bar.remove();

  // On macOS and Linux there is nothing to hand off to: a dmg is dragged and
  // an AppImage lives wherever its owner put it. Those builds send people to
  // the download instead of pretending to install it.
  if (!canSelfInstall) {
    const go = bar.querySelector(".upd-go");
    go.textContent = "Download";
    go.onclick = () => {
      call({ cmd: "openExternal", url: info.url });
      bar.remove();
    };
    return;
  }

  bar.querySelector(".upd-go").onclick = async () => {
    bar.classList.add("busy");
    text.textContent = "Downloading the update…";

    // The installer closes Rustify as its first step, so from here the window
    // going away *is* the success case. Nothing below runs on a good update.
    const stop = listen("update-progress", ({ payload }) => {
      fill.style.width = `${payload}%`;
      text.textContent =
        payload >= 100
          ? "Installing…"
          : `Downloading the update… ${payload}%`;
      if (payload >= 100) bar.classList.add("installing");
    });

    try {
      await invoke("apply_update", { url: info.url });
      // Downloaded and handed over. Rustify has moments left to live.
      bar.classList.add("installing");
      text.textContent = "Installing…";
    } catch (e) {
      (await stop)();
      bar.classList.remove("busy", "installing");
      fill.style.width = "0";
      text.innerHTML = `Update <b>v${esc(info.version)}</b> is available`;
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
           style="padding:8px 12px;border-radius:var(--radius-sm);border:1px solid var(--stroke);
                  background:var(--field);color:var(--text);outline:none;width:220px">`
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

      <div class="set-group">Appearance</div>
      ${setRow(
        "Themes and extensions",
        "Colour schemes, Discord Rich Presence and the equaliser live together on their own page.",
        `<button class="pill" id="set-themes">Open</button>`
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
      push();
    };
  });

  content.querySelector('[data-set="bitrate"]').onchange = push;
  content.querySelector("#set-device").onchange = push;

  content.querySelector("[data-zoom]").onchange = (e) => {
    applyZoom(Number(e.target.value));
  };

  content.querySelector("#set-themes").onclick = () => navigate("hub", "themes");

  content.querySelector("#set-site").onclick = () =>
    call({ cmd: "openExternal", url: "https://camwooloo.com" });

  content.querySelector("#set-clear-cache").onclick = async () => {
    await call({ cmd: "clearCache" });
    render();
  };
}

/** The catalogue, once per run: it is a network call and it never changes
 * under us mid-session. */
let themeCatalogue = null;

/** Draw the theme gallery into a grid element. */
async function renderThemes(grid, refresh) {
  const current = savedTheme();
  grid.innerHTML = `<p class="set-note">${refresh ? "Fetching themes…" : "Loading themes…"}</p>`;

  if (refresh || !themeCatalogue) {
    try {
      themeCatalogue = await invoke("spicetify_themes", { refresh: !!refresh });
    } catch (e) {
      grid.innerHTML = `
        <p class="set-note">Could not read the theme catalogue: ${esc(String(e))}</p>
        <button class="pill" id="theme-retry">Try again</button>`;
      grid.querySelector("#theme-retry").onclick = () => renderThemes(grid, true);
      return;
    }
  }

  // Spotify's own palette first, as the way back out of a theme.
  const spotify = {
    name: "Spotify",
    schemes: [
      {
        name: "Default",
        colors: {
          main: "121212", sidebar: "121212", player: "000000", card: "181818",
          text: "ffffff", subtext: "b3b3b3", button: "1ed760",
          "button-active": "3be477", "selected-row": "1a1a1a",
        },
      },
    ],
  };

  const swatch = (colors) =>
    ["main", "card", "button", "text", "subtext"]
      .map((k) => `<i style="background:#${esc(colors[k] || "888888")}"></i>`)
      .join("");

  const card = (theme, isDefault) => `
    <div class="theme-card">
      <div class="theme-name">${esc(theme.name)}${
        isDefault ? " <span>built in</span>" : theme.local ? " <span>installed</span>" : ""
      }</div>
      <div class="scheme-list">
        ${theme.schemes
          .map((sc) => {
            const on = isDefault
              ? !current
              : current && current.theme === theme.name && current.scheme === sc.name;
            return `
              <button class="scheme${on ? " on" : ""}"
                      data-theme="${esc(theme.name)}" data-scheme="${esc(sc.name)}">
                <span class="swatches">${swatch(sc.colors)}</span>
                <span class="scheme-name">${esc(sc.name)}</span>
              </button>`;
          })
          .join("")}
      </div>
    </div>`;

  grid.innerHTML = card(spotify, true) + themeCatalogue.map((t) => card(t, false)).join("");

  grid.querySelectorAll(".scheme").forEach((b) => {
    b.onclick = () => {
      const themeName = b.dataset.theme;
      const schemeName = b.dataset.scheme;

      if (themeName === "Spotify") {
        applyTheme(null);
      } else {
        const theme = themeCatalogue.find((t) => t.name === themeName);
        const scheme = theme && theme.schemes.find((sc) => sc.name === schemeName);
        if (!scheme) return;
        applyTheme({ theme: themeName, scheme: schemeName, colors: scheme.colors });
      }

      grid.querySelectorAll(".scheme").forEach((o) => o.classList.remove("on"));
      b.classList.add("on");
      toast(themeName === "Spotify" ? "Back to Spotify's colours" : `${themeName} · ${schemeName}`);
    };
  });
}

/** Interface scale. Kept in the UI because it is purely presentational. */
function applyZoom(percent) {
  localStorage.setItem("rustify.zoom", String(percent));
  document.documentElement.style.fontSize = `${(percent / 100) * 14}px`;
  document.body.style.zoom = `${percent}%`;
}

applyZoom(Number(localStorage.getItem("rustify.zoom") || 100));

/* ------------------------------------------------------------ stats */

/* stats.fm keeps years of listening history for a Spotify account and answers
 * for it publicly. A profile name is all it takes, which is why this asks for
 * one rather than for a login: Rustify never sees a stats.fm credential.
 *
 * Everything here carries the Spotify id stats.fm returns, so a chart is
 * something to play from rather than only to read. */

const STATSFM_KEY = "rustify.statsfm";
const RANGES = [
  ["weeks", "Last 4 weeks"],
  ["months", "Last 6 months"],
  ["lifetime", "All time"],
];

let statsRange = "weeks";

const statsfmUser = () => localStorage.getItem(STATSFM_KEY) || "";

/** Ask for a profile name, with a search so nobody needs to know their id. */
function renderStatsConnect(content, note) {
  content.innerHTML = `
    <h1 class="greeting">Listening stats</h1>
    <div class="center-note stats-connect">
      <h2>Connect stats.fm</h2>
      <p>stats.fm keeps the full history of what you have played on Spotify.
         Enter your profile name to see it here — nothing is signed in to, and
         only what your profile already shows publicly is read.</p>
      ${note ? `<p class="stats-warn">${esc(note)}</p>` : ""}
      <div class="jam-input">
        <input id="statsfm-name" placeholder="stats.fm profile name" autocomplete="off" />
        <button class="pill accent" id="statsfm-go">Connect</button>
      </div>
      <div id="statsfm-results" class="stats-results"></div>
      <button class="pill" id="statsfm-site">Open stats.fm</button>
    </div>`;

  const input = content.querySelector("#statsfm-name");
  const results = content.querySelector("#statsfm-results");

  const connect = (name) => {
    if (!name) return;
    localStorage.setItem(STATSFM_KEY, name);
    renderStats(content);
  };

  content.querySelector("#statsfm-go").onclick = () => connect(input.value.trim());
  input.onkeydown = (e) => {
    if (e.key === "Enter") connect(input.value.trim());
  };

  // Typing searches, so a display name is enough to find the profile.
  let timer = null;
  input.oninput = () => {
    clearTimeout(timer);
    const query = input.value.trim();
    if (query.length < 2) {
      results.innerHTML = "";
      return;
    }
    timer = setTimeout(async () => {
      let found = [];
      try {
        found = await invoke("statsfm_search", { query });
      } catch {
        return;
      }
      results.innerHTML = found
        .map(
          (u) => `
          <button class="device-row" data-id="${esc(u.id)}">
            ${
              u.image
                ? `<img src="${esc(u.image)}" alt="" data-fallback="artist" />`
                : `<div class="thumb">${icon("artist")}</div>`
            }
            <span class="meta">
              <span class="title">${esc(u.name)}</span>
              <span class="sub">${esc(u.id)}</span>
            </span>
          </button>`
        )
        .join("");
      results.querySelectorAll("[data-id]").forEach((b) => {
        b.onclick = () => connect(b.dataset.id);
      });
    }, 300);
  };

  content.querySelector("#statsfm-site").onclick = () =>
    call({ cmd: "openExternal", url: "https://stats.fm" });
}

/** One chart row: rank, artwork, name, and what it was played. */
function statsRow(entry, index) {
  const plays = entry.streams
    ? `${entry.streams.toLocaleString()} play${entry.streams === 1 ? "" : "s"}`
    : "";
  const mins = entry.minutes ? `${entry.minutes.toLocaleString()} min` : "";

  return `
    <button class="stats-row${entry.uri ? "" : " noplay"}" data-uri="${esc(entry.uri || "")}">
      <span class="rank">${index + 1}</span>
      ${entry.image ? `<img src="${esc(entry.image)}" alt="" />` : `<div class="thumb"></div>`}
      <span class="meta">
        <span class="title">${esc(entry.name)}</span>
        <span class="sub">${esc(entry.sub)}</span>
      </span>
      <span class="count">${esc([plays, mins].filter(Boolean).join(" · "))}</span>
    </button>`;
}

async function renderStats(content) {
  const user = statsfmUser();
  if (!user) return renderStatsConnect(content);

  content.innerHTML = `<h1 class="greeting">Listening stats</h1>
    <p class="set-note">Reading ${esc(user)} on stats.fm…</p>`;

  let data;
  try {
    data = await invoke("statsfm_overview", { user, range: statsRange });
  } catch (e) {
    return renderStatsConnect(content, `Could not read that profile: ${String(e)}`);
  }

  const section = (title, entries, kind) =>
    entries.length
      ? `<div class="stats-block">
           <h2 class="section-title">${title}</h2>
           <div class="stats-list">${entries.map(statsRow).join("")}</div>
         </div>`
      : data.private.includes(kind)
        ? `<div class="stats-block">
             <h2 class="section-title">${title}</h2>
             <p class="set-note">This profile keeps ${esc(kind)} private on stats.fm.</p>
           </div>`
        : "";

  const hours = Math.round(data.minutes / 60).toLocaleString();

  content.innerHTML = `
    <div class="stats-head">
      ${
        data.account.image
          ? `<img src="${esc(data.account.image)}" alt="" data-fallback="artist" />`
          : `<div class="thumb">${icon("artist")}</div>`
      }
      <div class="meta">
        <span class="kind">STATS.FM</span>
        <h1>${esc(data.account.name)}</h1>
        <span class="sub">${esc(data.account.id)}</span>
      </div>
      <button class="pill" id="stats-disconnect">Use another profile</button>
    </div>

    <div class="lib-filters stats-ranges">
      ${RANGES.map(
        ([key, label]) =>
          `<button class="chip${key === statsRange ? " active" : ""}" data-range="${key}">${label}</button>`
      ).join("")}
    </div>

    <div class="stats-figures">
      <div class="figure"><b>${hours}</b><span>hours listened</span></div>
      <div class="figure"><b>${data.minutes.toLocaleString()}</b><span>minutes</span></div>
      <div class="figure"><b>${data.streams.toLocaleString()}</b><span>streams</span></div>
    </div>

    ${section("Top tracks", data.tracks, "tracks")}
    ${section("Top artists", data.artists, "artists")}
    ${section("Top albums", data.albums, "albums")}
    ${section("Recently played", data.recent, "recent")}`;

  content.querySelectorAll("[data-range]").forEach((b) => {
    b.onclick = () => {
      statsRange = b.dataset.range;
      renderStats(content);
    };
  });

  content.querySelector("#stats-disconnect").onclick = () => {
    localStorage.removeItem(STATSFM_KEY);
    renderStats(content);
  };

  // A row opens what it points at: artists and albums have pages of their
  // own here, and a track is simply played.
  content.querySelectorAll(".stats-row[data-uri]").forEach((b) => {
    const uri = b.dataset.uri;
    if (!uri) return;
    b.onclick = () => {
      const [, kind, id] = uri.split(":");
      if (kind === "artist") return navigate("artist", id);
      if (kind === "album") return navigate("album", id);
      call({ cmd: "loadTracks", uris: [uri], startPlaying: true });
      toast("Playing");
    };
  });
}

/* -------------------------------------------------------------- hub */

/* Extensions and themes, in one place reached from the top bar.
 *
 * Extensions here are parts of Rustify that ship switched off, not code
 * fetched from strangers. That is the whole difference: everything on this
 * page was written for this app, so enabling one cannot hand an account to
 * somebody's repository. */

const HUB_TABS = [
  ["extensions", "Extensions"],
  ["themes", "Themes"],
];

/* Presets are the ones people actually reach for, in the order a list of
 * them is usually read. Values are per band: 60 Hz, 230 Hz, 910 Hz, 3.6 kHz,
 * 14 kHz. */
const EQ_PRESETS = [
  ["Flat", [0, 0, 0, 0, 0]],
  ["Bass boost", [7, 4, 0, 0, 0]],
  ["Bass cut", [-7, -3, 0, 0, 1]],
  ["Vocal", [-2, 0, 4, 3, 0]],
  ["Treble boost", [0, 0, 0, 4, 7]],
  ["Late night", [4, 1, 0, 1, 3]],
];

const EQ_BANDS = ["60 Hz", "230 Hz", "910 Hz", "3.6 kHz", "14 kHz"];

let hubTab = "extensions";

/** Read the daemon's settings, or null when it cannot be reached. */
async function hubSettings() {
  try {
    return (await call({ cmd: "getSettings" })).settings;
  } catch {
    return null;
  }
}

/** Send settings back, leaving everything not named here alone. */
async function saveSettings(settings, changes) {
  const next = { ...settings, ...changes };
  await call({ cmd: "setSettings", ...next });
  return next;
}

function eqSliders(gains) {
  return EQ_BANDS.map(
    (label, i) => `
    <label class="eq-band">
      <input type="range" class="eq-slider" data-band="${i}"
             min="-12" max="12" step="1" value="${gains[i] ?? 0}" />
      <span class="eq-db" data-db="${i}">${(gains[i] ?? 0) > 0 ? "+" : ""}${gains[i] ?? 0}</span>
      <span class="eq-hz">${label}</span>
    </label>`
  ).join("");
}

async function renderHub(content, tab) {
  hubTab = tab || hubTab;

  content.innerHTML = `
    <h1 class="greeting">Extensions</h1>
    <div class="lib-filters hub-tabs">
      ${HUB_TABS.map(
        ([key, label]) =>
          `<button class="chip${key === hubTab ? " active" : ""}" data-hub="${key}">${label}</button>`
      ).join("")}
    </div>
    <div id="hub-body"><p class="set-note">Loading…</p></div>`;

  content.querySelectorAll("[data-hub]").forEach((b) => {
    b.onclick = () => renderHub(content, b.dataset.hub);
  });

  const body = content.querySelector("#hub-body");

  if (hubTab === "themes") {
    body.innerHTML = `
      <p class="set-note hub-note">
        Colour schemes in Spicetify's format, from any Spicetify install on
        this computer and from the community collection. Only the colours are
        read — never anyone's code.
      </p>
      <div class="theme-bar">
        <div class="control"><button class="pill" id="theme-refresh">Refresh</button></div>
      </div>
      <div class="theme-grid" id="theme-grid"></div>`;

    const grid = body.querySelector("#theme-grid");
    renderThemes(grid, false);
    body.querySelector("#theme-refresh").onclick = () => renderThemes(grid, true);
    return;
  }

  const settings = await hubSettings();
  if (!settings) {
    body.innerHTML = `<p class="set-note">The player is not reachable, so its extensions cannot be read.</p>`;
    return;
  }

  let current = settings;
  const gains = (current.equaliserGains?.length ? current.equaliserGains : [0, 0, 0, 0, 0]).slice();

  body.innerHTML = `
    <div class="ext-card">
      <div class="ext-head">
        <div>
          <b>Discord Rich Presence</b>
          <p>Shows what you are playing on your Discord profile, with a link
             to the project underneath it. Discord's own Spotify integration
             only reads the official client, so this is how listening here
             still shows up.</p>
        </div>
        ${toggleHtml("discordPresence", current.discordPresence)}
      </div>
    </div>

    <div class="ext-card">
      <div class="ext-head">
        <div>
          <b>Visualiser</b>
          <p>A view of what is playing, drawn from the audio itself — bars,
             mirrored, radial or a wave, over the artwork. It sits next to the
             miniplayer and full screen in the player bar. Levels are analysed
             only while it is open.</p>
        </div>
        <button class="pill" id="ext-viz">Open</button>
      </div>
    </div>

    <div class="ext-card">
      <div class="ext-head">
        <div>
          <b>Equaliser</b>
          <p>Five bands, applied between the decoder and your speakers.
             Changes are heard immediately. Spotify's own desktop client has
             no equaliser at all.</p>
        </div>
        ${toggleHtml("equaliser", current.equaliser)}
      </div>

      <div class="eq" id="eq-panel" ${current.equaliser ? "" : "data-off"}>
        <div class="lib-filters eq-presets">
          ${EQ_PRESETS.map(
            ([name]) => `<button class="chip" data-preset="${esc(name)}">${esc(name)}</button>`
          ).join("")}
        </div>
        <div class="eq-bands">${eqSliders(gains)}</div>
      </div>
    </div>`;

  body.querySelector("#ext-viz").onclick = () => setVisualiser(true);

  // Both toggles write straight through: a switch that needs a save button
  // is a switch people leave in the wrong position.
  body.querySelectorAll("[data-toggle]").forEach((b) => {
    b.onclick = async () => {
      b.classList.toggle("on");
      const on = b.classList.contains("on");
      b.setAttribute("aria-checked", on);

      const key = b.dataset.toggle;
      if (key === "equaliser") {
        body.querySelector("#eq-panel").toggleAttribute("data-off", !on);
      }

      try {
        current = await saveSettings(current, { [key]: on });
        if (key === "discordPresence") {
          toast(on ? "Discord will show what you play" : "Discord status off");
        }
      } catch {
        // The daemon reported it; put the switch back where it was.
        b.classList.toggle("on");
      }
    };
  });

  const pushGains = async () => {
    try {
      current = await saveSettings(current, { equaliserGains: gains });
    } catch {
      /* reported by the daemon */
    }
  };

  body.querySelectorAll(".eq-slider").forEach((slider) => {
    const band = Number(slider.dataset.band);
    const readout = body.querySelector(`[data-db="${band}"]`);

    slider.oninput = () => {
      gains[band] = Number(slider.value);
      readout.textContent = `${gains[band] > 0 ? "+" : ""}${gains[band]}`;
    };
    // Saved when the slider is let go, not on every pixel of the drag.
    slider.onchange = pushGains;
  });

  body.querySelectorAll("[data-preset]").forEach((b) => {
    b.onclick = () => {
      const preset = EQ_PRESETS.find(([name]) => name === b.dataset.preset);
      if (!preset) return;

      preset[1].forEach((value, i) => {
        gains[i] = value;
        const slider = body.querySelector(`.eq-slider[data-band="${i}"]`);
        const readout = body.querySelector(`[data-db="${i}"]`);
        if (slider) slider.value = String(value);
        if (readout) readout.textContent = `${value > 0 ? "+" : ""}${value}`;
      });

      body.querySelectorAll("[data-preset]").forEach((o) => o.classList.remove("active"));
      b.classList.add("active");
      pushGains();
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

/* Which kinds of result the search page is showing.
 *
 * Kept outside the render so it survives typing: narrowing to Songs and then
 * refining the query should stay narrowed, the way it does in the real
 * client. A filter with no results is disabled rather than hidden, so the
 * row of filters does not move under the pointer as results change. */
const SEARCH_FILTERS = [
  ["all", "All"],
  ["tracks", "Songs"],
  ["artists", "Artists"],
  ["albums", "Albums"],
  ["playlists", "Playlists"],
];

let searchFilter = "all";

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

  const counts = {
    all: (res.tracks?.length || 0) + (res.artists?.length || 0) +
      (res.albums?.length || 0) + (res.playlists?.length || 0),
    tracks: res.tracks?.length || 0,
    artists: res.artists?.length || 0,
    albums: res.albums?.length || 0,
    playlists: res.playlists?.length || 0,
  };

  // A filter narrowed to something this search has none of would show an
  // empty page and look broken, so it falls back to everything.
  if (!counts[searchFilter]) searchFilter = "all";

  const showing = (kind) => searchFilter === "all" || searchFilter === kind;
  const heading = (title) => (searchFilter === "all" ? `<h2 class="section-title">${title}</h2>` : "");

  content.innerHTML = `
    <div class="lib-filters search-filters">
      ${SEARCH_FILTERS.map(
        ([key, label]) => `
        <button class="chip${key === searchFilter ? " active" : ""}"
                data-filter="${key}" ${counts[key] ? "" : "disabled"}>${label}</button>`
      ).join("")}
    </div>
    ${showing("tracks") && counts.tracks ? `${heading("Songs")}${trackTable(res.tracks)}` : ""}
    ${showing("artists") && counts.artists ? `${heading("Artists")}${cardGrid(res.artists, "artist")}` : ""}
    ${showing("albums") && counts.albums ? `${heading("Albums")}${cardGrid(res.albums, "album")}` : ""}
    ${showing("playlists") && counts.playlists ? `${heading("Playlists")}${cardGrid(res.playlists, "playlist")}` : ""}
    ${counts.all ? "" : `<p class="set-note">Nothing found for “${esc(query)}”.</p>`}`;

  content.querySelectorAll("[data-filter]").forEach((b) => {
    b.onclick = () => {
      searchFilter = b.dataset.filter;
      renderSearch(content, query);
    };
  });

  wireCards(content);
  wireTracks(content, res.tracks || []);
}

/** Just the rows, so a reorder can redraw them without the header. */
function trackRows(tracks) {
  return tracks
    .map((t, i) => {
      const current = state?.track?.uri && state.track.uri === t.uri;
      // `data-play` is the row's index, which is all the click handler needs.
      // The right-click menu needs to know what the row *is*, though, and
      // without these it read the index as the URI — so every song, in every
      // list, got the menu meant for albums and playlists instead of one with
      // "Add to queue" on it.
      return `<div class="track-row ${current ? "current" : ""}" data-play="${i}"
        data-uri="${esc(t.uri)}"
        ${t.artists?.[0]?.uri ? `data-artist-uri="${esc(t.artists[0].uri)}"` : ""}
        ${t.album?.uri ? `data-album-uri="${esc(t.album.uri)}"` : ""}>
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
}

function trackTable(tracks) {
  return `<div class="track-head">
      <div style="text-align:right">#</div>
      <div>Title</div>
      <div>Album</div>
      <div class="col-added">Date added</div>
      <div></div>
      <div style="text-align:right">Time</div>
    </div>${trackRows(tracks)}`;
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
        // Liked Songs, search results and an artist's top tracks have no
        // context to hand over, so the list itself is the context: send all
        // of it and say where to start. Sending the one clicked track left
        // the player with nothing to play next.
        call({
          cmd: "loadTracks",
          uris: tracks.map((t) => t.uri),
          index: i,
          startPlaying: true,
        });
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
        <button class="act" id="ctx-more" title="More" hidden>${icon("dots")}</button>
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

  if (currentPlaylistIsMine) {
    wirePlaylistEditing(content, id, pl);
    wireReorder(content, id, tracks.items);
  }
}

/* --------------------------------------------------- playlist editing */

/** Rename, describe or remove a playlist you own. */
function wirePlaylistEditing(content, id, playlist) {
  const more = content.querySelector("#ctx-more");
  if (!more) return;
  more.hidden = false;

  more.onclick = (e) => {
    e.stopPropagation();
    const box = more.getBoundingClientRect();
    openCtx(box.left, box.bottom + 6, [
      {
        id: "rename",
        icon: "browse",
        label: "Rename",
        run: () =>
          askFor("Rename playlist", playlist.name || "", async (name) => {
            await call({ cmd: "renamePlaylist", playlistId: id, name });
            toast("Renamed");
            playlistCache = null;
            renderLibrary();
            render();
          }),
      },
      {
        id: "describe",
        icon: "browse",
        label: "Edit description",
        run: () =>
          askFor("Playlist description", playlist.description || "", async (description) => {
            await call({ cmd: "describePlaylist", playlistId: id, description });
            toast("Description saved");
          }),
      },
      "-",
      {
        id: "delete",
        icon: "x",
        label: "Delete playlist",
        run: async () => {
          // Spotify keeps it recoverable from the web player, which is why
          // this asks once rather than twice.
          if (!(await confirmAction(`Delete “${playlist.name}”?`))) return;
          await call({ cmd: "unfollowPlaylist", playlistId: id });
          toast("Playlist deleted");
          playlistCache = null;
          renderLibrary();
          navigate("home");
        },
      },
    ]);
  };
}

/** A one-field prompt, since the platform's own is not available here. */
function askFor(title, value, done) {
  closePopovers();
  const pop = el("div", { className: "popover top" });
  pop.style.cssText =
    "left:50%;right:auto;top:80px;transform:translateX(-50%);width:min(420px,84vw)";
  pop.innerHTML = `
    <h3>${esc(title)}</h3>
    <div class="jam-input">
      <input id="ask-value" value="${esc(value)}" autocomplete="off" />
      <button class="pill accent" id="ask-ok">Save</button>
    </div>`;
  $("#popovers").append(pop);

  const input = pop.querySelector("#ask-value");
  input.focus();
  input.select();

  const submit = async () => {
    const next = input.value.trim();
    closePopovers();
    if (!next && title.startsWith("Rename")) return;
    try {
      await done(next);
    } catch {
      /* the daemon reported it */
    }
  };

  pop.querySelector("#ask-ok").onclick = submit;
  input.onkeydown = (e) => {
    if (e.key === "Enter") submit();
    if (e.key === "Escape") closePopovers();
  };
}

/** Ask before something irreversible, resolving to true or false. */
function confirmAction(question) {
  return new Promise((resolve) => {
    closePopovers();
    const pop = el("div", { className: "popover top" });
    pop.style.cssText =
      "left:50%;right:auto;top:80px;transform:translateX(-50%);width:min(420px,84vw)";
    pop.innerHTML = `
      <h3>${esc(question)}</h3>
      <p class="hint">It leaves your library. Spotify keeps it recoverable
         from the web player for a while.</p>
      <div class="jam-input">
        <button class="pill" id="confirm-no">Cancel</button>
        <button class="pill accent" id="confirm-yes">Delete</button>
      </div>`;
    $("#popovers").append(pop);

    pop.querySelector("#confirm-no").onclick = () => {
      closePopovers();
      resolve(false);
    };
    pop.querySelector("#confirm-yes").onclick = () => {
      closePopovers();
      resolve(true);
    };
  });
}

/** Drag a row to move it, in a playlist you own. */
function wireReorder(content, id, tracks) {
  const rows = [...content.querySelectorAll("[data-play]")];
  let from = null;

  rows.forEach((row) => {
    row.draggable = true;

    row.ondragstart = (e) => {
      from = Number(row.dataset.play);
      row.classList.add("dragging");
      // Firefox refuses to start a drag without data on the transfer.
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData("text/plain", String(from));
    };

    row.ondragover = (e) => {
      if (from === null) return;
      e.preventDefault();
      row.classList.add("drop-target");
    };

    row.ondragleave = () => row.classList.remove("drop-target");

    row.ondragend = () => {
      row.classList.remove("dragging");
      rows.forEach((r) => r.classList.remove("drop-target"));
    };

    row.ondrop = async (e) => {
      e.preventDefault();
      row.classList.remove("drop-target");
      const to = Number(row.dataset.play);
      if (from === null || from === to) return;

      const moved = from;
      from = null;

      // Move it on screen first: waiting for a round trip to reorder a list
      // someone just dragged feels broken even when it is fast.
      const [track] = tracks.splice(moved, 1);
      tracks.splice(to, 0, track);
      renderPlaylistRows(content, tracks, id);

      try {
        await call({ cmd: "reorderPlaylist", playlistId: id, from: moved, to });
      } catch {
        // Put it back: the server is the truth.
        playlistCache = null;
        render();
      }
    };
  });
}

/** Redraw just the rows of a playlist, keeping the header where it is. */
function renderPlaylistRows(content, tracks, id) {
  const table = content.querySelector(".track-head")?.parentElement;
  if (!table) return;
  const rows = table.querySelectorAll("[data-play]");
  rows.forEach((r) => r.remove());
  table.insertAdjacentHTML("beforeend", trackRows(tracks));
  wireTracks(content, tracks, `spotify:playlist:${id}`);
  wireReorder(content, id, tracks);
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

// Toggles the lyrics view. This handler was lost in an earlier edit, which
// left the button inert even though the view behind it still worked.
$("#btn-lyrics").onclick = () =>
  navigate(view.name === "lyrics" ? "home" : "lyrics");

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
    // The row's own heart already tracks this, optimistic updates included,
    // so read it rather than keeping a second copy of the answer.
    const heartBtn = row.querySelector(".heart");
    const alreadySaved = heartBtn?.classList.contains("on") ?? false;

    items.push({
      id: "save",
      icon: alreadySaved ? "heart" : "heart-o",
      label: alreadySaved
        ? "Remove from your Liked Songs"
        : "Save to your Liked Songs",
      run: async () => {
        // Let the heart do it, so the row updates the way it always does.
        if (heartBtn) {
          heartBtn.click();
          return;
        }
        await call({ cmd: "setSaved", uri, saved: !alreadySaved });
        toast(alreadySaved ? "Removed" : "Saved");
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
    label: isTrack ? "Go to song radio" : "Go to radio",
    run: async () => {
      toast("Building radio\u2026");
      try {
        await call({ cmd: "startRadio", seedUri: uri });
      } catch {
        /* the daemon already reported why */
      }
    },
  });

  // Spotify's two most-used entries, and the reason its menu is a navigation
  // tool rather than just a list of actions.
  const goTo = (kind, target) => ({
    id: `go-${kind}`,
    icon: kind === "artist" ? "artist" : "album",
    label: `Go to ${kind}`,
    run: () => navigate(kind, target.split(":").pop()),
  });

  if (isTrack && row?.dataset.artistUri) {
    items.push(goTo("artist", row.dataset.artistUri));
  }
  if (isTrack && row?.dataset.albumUri) {
    items.push(goTo("album", row.dataset.albumUri));
  }

  // Only offered where it makes sense: a track, inside a playlist you own.
  if (
    isTrack &&
    row.dataset.play !== undefined &&
    view.name === "playlist" &&
    currentPlaylistIsMine
  ) {
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

/** Set an attribute, or remove it when there is no value to set. */
function setOrDrop(node, attr, value) {
  if (value) node.setAttribute(attr, value);
  else node.removeAttribute(attr);
}

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
       <button class="heart ${t.saved ? "on" : ""}" id="np-heart"
               title="${t.saved ? "Remove from your Liked Songs" : "Save to your Liked Songs"}">
         ${icon(t.saved ? "heart" : "heart-o")}
       </button>
       <button class="np-add" id="np-addpl" title="Add to playlist">
         ${icon("plus")}
       </button>`
    : "";

  const heart = $("#np-heart");
  if (heart) {
    heart.onclick = () =>
      call({ cmd: "setSaved", uri: t.uri, saved: !t.saved });
  }

  const addpl = $("#np-addpl");
  if (addpl) {
    addpl.onclick = (e) => {
      // Without this the document-level handler that dismisses popovers sees
      // the same click and shuts the picker before it is on screen.
      e.stopPropagation();
      addToPlaylistDialog(t.uri);
    };
  }

  // The bar is a track like any other, so right-clicking it should offer the
  // track menu rather than the page one. These are what the menu reads.
  if (t) {
    np.setAttribute("data-uri", t.uri);
    setOrDrop(np, "data-artist-uri", t.artists?.[0]?.uri);
    setOrDrop(np, "data-album-uri", t.album?.uri);
  } else {
    ["data-uri", "data-artist-uri", "data-album-uri"].forEach((a) =>
      np.removeAttribute(a)
    );
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

/* -------------------------------------------------------- full screen */

/* Full screen is the artwork, the title, and the player bar — the rest of
 * the interface steps out of the way rather than being rebuilt. The window
 * itself goes fullscreen too, so this is not a maximised window pretending. */

let fullscreenOn = false;

function paintFullscreen() {
  if (!fullscreenOn) return;
  const track = state?.track;
  const art = track?.album?.images?.[0]?.url || track?.coverUrl || "";

  $("#fullscreen").innerHTML = `
    <div class="fs-art">${art ? `<img src="${esc(art)}" alt="" />` : `<div class="thumb"></div>`}</div>
    <div class="fs-meta">
      <h1>${esc(track?.name || "Nothing playing")}</h1>
      <p>${esc(artistNames(track) || "")}</p>
    </div>`;
}

async function setFullscreen(on) {
  fullscreenOn = on;
  document.body.classList.toggle("fs", on);
  $("#fullscreen").hidden = !on;
  $("#btn-fullscreen").classList.toggle("on", on);
  paintFullscreen();

  try {
    await appWindow.setFullscreen(on);
  } catch {
    // A window manager that refuses is not a reason to lose the view.
  }
}

$("#btn-fullscreen").onclick = () => setFullscreen(!fullscreenOn);
$("#btn-mini").onclick = () => invoke("open_mini");

// Escape is what everyone reaches for, and there is no other way out while
// the window has no chrome.
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && fullscreenOn) setFullscreen(false);
});

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
      paintFullscreen();
      if (typeof vizPaintChrome === "function") vizPaintChrome();
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
    case "spectrum":
      // The visualiser owns these; nothing else has a use for them.
      if (typeof vizFeed === "function") vizFeed(ev.bands);
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
  checkForUpdate();
  // A release can land while Rustify is open, which for a music player is
  // most of the time. One request every two hours is a few hundred bytes and
  // means the offer does not wait for a restart. The bar shows once: a second
  // check while it is up does nothing.
  setInterval(checkForUpdate, 2 * 60 * 60 * 1000);

  // Reconcile against the daemon on a timer. Started *before* anything can
  // return early: when this sat after the connection gate, a missed status
  // event left the window insisting the player was unreachable forever, over
  // a socket that was in fact connected the whole time.
  setInterval(() => {
    if (Date.now() - lastEventAt > 4000) resync();
    if (!$("#offline").hidden) setConnecting(true); // refresh the wording
  }, 4000);

  // Ask directly rather than only waiting for an event. If the player was
  // already running, the link connects before this webview exists and the
  // status event is delivered to nobody.
  try {
    if (await invoke("connected")) {
      connected = true;
      resolveConnected();
    }
  } catch {
    /* the race below still covers it */
  }

  const timedOut = await Promise.race([
    connectedOnce.then(() => false),
    new Promise((r) => setTimeout(() => r(true), 30000)),
  ]);

  if (timedOut) {
    setConnecting(true); // shows the stuck wording after 15s
    return; // the watchdog above keeps trying
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

})();
