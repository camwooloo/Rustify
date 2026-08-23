/* The miniplayer window.
 *
 * A separate window rather than a mode of the main one: it stays on top of
 * whatever you are doing, and the main window is hidden while it is up, which
 * is what the official client does. Both windows talk to the same daemon
 * through the same bridge, and the daemon broadcasts its events to every
 * window, so this needs no channel of its own.
 */

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

const $ = (sel) => document.querySelector(sel);

const call = (command) => invoke("call", { command });

let state = null;
let ticking = null;

const fmt = (ms) => {
  const total = Math.max(0, Math.round((ms || 0) / 1000));
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
};

const icon = (el, name) => el.querySelector("use").setAttribute("href", `#i-${name}`);

function paint() {
  const track = state?.track;

  $("#mini-name").textContent = track?.name || "Nothing playing";
  $("#mini-artist").textContent = (track?.artists || []).map((a) => a.name).join(", ");

  const art = track?.album?.images?.[0]?.url || track?.coverUrl || "";
  const box = $("#mini-art");
  // A background rather than an <img>: the window is the artwork, and a
  // missing cover should leave the panel rather than a broken box.
  box.style.backgroundImage = art ? `url("${art}")` : "none";

  icon($("#mini-play"), state?.playing ? "pause" : "play");
  icon($("#mini-like"), track?.liked ? "heart" : "heart-o");
  $("#mini-like").classList.toggle("on", !!track?.liked);

  const duration = track?.durationMs || 0;
  const pct = duration ? Math.min(100, (state.positionMs / duration) * 100) : 0;
  $("#mini-fill").style.width = `${pct}%`;
  $("#mini-time").textContent = fmt(state?.positionMs);
}

/** Advance the clock between events, as the main window does. */
function tick() {
  clearInterval(ticking);
  ticking = setInterval(() => {
    if (!state?.playing) return;
    state.positionMs += 500;
    paint();
  }, 500);
}

$("#mini-play").onclick = () => call({ cmd: "playPause" });
$("#mini-prev").onclick = () => call({ cmd: "previous" });
$("#mini-next").onclick = () => call({ cmd: "next" });

$("#mini-like").onclick = async () => {
  const uri = state?.track?.uri;
  if (!uri) return;
  const liked = !state.track.liked;
  state.track.liked = liked;
  paint();
  await call({ cmd: "setSaved", uri, saved: liked });
};

$("#mini-expand").onclick = () => invoke("close_mini", { restore: true });

/* Frames arrive flattened: the frame is the event, with `event` naming it.
 * Position ticks come far more often than full state, so both are handled. */
listen("daemon-event", ({ payload }) => {
  const ev = payload;
  if (ev.event === "state") {
    state = ev;
    paint();
  } else if (ev.event === "position" && state) {
    state.positionMs = ev.positionMs;
    state.playing = ev.playing;
    paint();
  }
});

(async () => {
  try {
    state = await call({ cmd: "getState" });
  } catch {
    // The daemon may still be starting; the event stream will catch up.
  }
  paint();
  tick();

  // Cheap insurance against a missed event: the main window does the same.
  setInterval(async () => {
    try {
      state = await call({ cmd: "getState" });
      paint();
    } catch {
      /* keep the last state on screen */
    }
  }, 5000);
})();
