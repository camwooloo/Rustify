/* The visualiser.
 *
 * The levels come from the daemon: playback happens there, and by the time
 * sound reaches the speakers it has left this process entirely. The daemon
 * analyses only while this view is open, and the request expires — so the
 * keepalive below is not decoration, it is what stops a window that crashed
 * from leaving an FFT running for the rest of the session.
 *
 * Frames arrive about thirty times a second and the screen redraws sixty, so
 * what is drawn is always eased towards the last frame rather than snapped to
 * it. That easing is most of what makes it look alive rather than twitchy.
 */

const VIZ_KEY = "rustify.visualiser";
const VIZ_ON_KEY = "rustify.visualiser.enabled";
const VIZ_BANDS = 48;

const VIZ_STYLES = [
  ["bars", "Bars"],
  ["mirror", "Mirror"],
  ["radial", "Radial"],
  ["wave", "Wave"],
];

const VIZ_PALETTES = [
  ["accent", "Theme"],
  ["spectrum", "Spectrum"],
  ["mono", "Mono"],
];

const VIZ_BACKDROPS = [
  ["artwork", "Artwork"],
  ["solid", "Solid"],
  ["glow", "Glow"],
];

const vizDefaults = {
  style: "bars",
  palette: "accent",
  backdrop: "artwork",
  smoothing: 0.62,
  sensitivity: 1.15,
  showTrack: true,
};

let vizOptions = { ...vizDefaults };
let vizOpen = false;
let vizFrame = null;
let vizKeepalive = null;
/** Levels as last received, and as currently drawn. */
let vizTarget = new Array(VIZ_BANDS).fill(0);
let vizDrawn = new Array(VIZ_BANDS).fill(0);

try {
  vizOptions = { ...vizDefaults, ...JSON.parse(localStorage.getItem(VIZ_KEY) || "{}") };
} catch {
  /* a corrupt entry is not worth a broken view */
}

const vizSave = () => localStorage.setItem(VIZ_KEY, JSON.stringify(vizOptions));

/** The theme's accent, so the visualiser wears whatever the app is wearing. */
function vizAccent() {
  return getComputedStyle(document.documentElement).getPropertyValue("--accent").trim() || "#1ed760";
}

function vizColour(ctx, i, height, box) {
  if (vizOptions.palette === "spectrum") {
    // Low notes warm, high notes cool: the ordering people expect.
    return `hsl(${140 + (i / VIZ_BANDS) * 190}deg 85% ${45 + height * 22}%)`;
  }
  if (vizOptions.palette === "mono") {
    return `rgba(255,255,255,${0.35 + height * 0.6})`;
  }
  const gradient = ctx.createLinearGradient(0, box.height, 0, 0);
  gradient.addColorStop(0, vizAccent());
  gradient.addColorStop(1, "#ffffff");
  return gradient;
}

function drawBars(ctx, box, mirrored) {
  // A fifth of each slot as air: enough that the bars read as bars rather
  // than as a filled shape with notches in it.
  const slot = box.width / VIZ_BANDS;
  const gap = slot * 0.22;
  const width = slot - gap;
  const full = (mirrored ? box.height / 2 : box.height) * 0.78;

  for (let i = 0; i < VIZ_BANDS; i++) {
    const level = vizDrawn[i];
    const height = Math.max(2, level * full);
    const x = i * (width + gap);

    ctx.fillStyle = vizColour(ctx, i, level, box);
    const radius = Math.min(width / 2, 6);

    if (mirrored) {
      ctx.beginPath();
      ctx.roundRect(x, box.height / 2 - height, width, height, [radius, radius, 0, 0]);
      ctx.fill();
      ctx.beginPath();
      ctx.roundRect(x, box.height / 2, width, height, [0, 0, radius, radius]);
      ctx.fill();
    } else {
      ctx.beginPath();
      ctx.roundRect(x, box.height - height, width, height, [radius, radius, 0, 0]);
      ctx.fill();
    }
  }
}

/** Bars radiating from a disc — the shape the app's own mark is drawn in. */
function drawRadial(ctx, box) {
  const cx = box.width / 2;
  const cy = box.height / 2;
  const inner = Math.min(box.width, box.height) * 0.17;
  const reach = Math.min(box.width, box.height) * 0.26;
  const width = Math.max(3, (inner * 2 * Math.PI) / VIZ_BANDS / 1.7);

  ctx.save();
  ctx.translate(cx, cy);

  for (let i = 0; i < VIZ_BANDS; i++) {
    const level = vizDrawn[i];
    // Mirrored around the circle, so both halves move together and the
    // shape stays symmetrical rather than lopsided.
    const angle = (i / VIZ_BANDS) * Math.PI * 2;
    const length = Math.max(4, level * reach);

    ctx.save();
    ctx.rotate(angle);
    ctx.fillStyle = vizColour(ctx, i, level, box);
    ctx.beginPath();
    ctx.roundRect(-width / 2, -inner - length, width, length, width / 2);
    ctx.fill();
    ctx.restore();
  }

  ctx.strokeStyle = `rgba(255,255,255,0.12)`;
  ctx.lineWidth = 2;
  ctx.beginPath();
  ctx.arc(0, 0, inner - 6, 0, Math.PI * 2);
  ctx.stroke();
  ctx.restore();
}

function drawWave(ctx, box) {
  const step = box.width / (VIZ_BANDS - 1);
  const middle = box.height / 2;
  const reach = box.height * 0.36;

  ctx.beginPath();
  ctx.moveTo(0, middle - vizDrawn[0] * reach);
  for (let i = 1; i < VIZ_BANDS; i++) {
    // Through the midpoint between samples, which is what turns a polyline
    // into something that looks drawn rather than plotted.
    const x = i * step;
    const y = middle - vizDrawn[i] * reach;
    const px = (i - 1) * step;
    const py = middle - vizDrawn[i - 1] * reach;
    ctx.quadraticCurveTo(px, py, (px + x) / 2, (py + y) / 2);
  }

  ctx.lineTo(box.width, middle);
  ctx.lineWidth = 3;
  ctx.strokeStyle = vizOptions.palette === "mono" ? "#fff" : vizAccent();
  ctx.stroke();

  ctx.lineTo(box.width, box.height);
  ctx.lineTo(0, box.height);
  ctx.closePath();
  const fill = ctx.createLinearGradient(0, 0, 0, box.height);
  fill.addColorStop(0, `${vizOptions.palette === "mono" ? "#ffffff" : vizAccent()}7a`);
  fill.addColorStop(1, "transparent");
  ctx.fillStyle = fill;
  ctx.fill();
}

function vizRender() {
  const canvas = document.getElementById("viz-canvas");
  if (!canvas || !vizOpen) return;

  const ratio = window.devicePixelRatio || 1;
  const box = { width: canvas.clientWidth, height: canvas.clientHeight };
  if (canvas.width !== box.width * ratio || canvas.height !== box.height * ratio) {
    canvas.width = box.width * ratio;
    canvas.height = box.height * ratio;
  }

  const ctx = canvas.getContext("2d");
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  ctx.clearRect(0, 0, box.width, box.height);

  // Ease towards the last frame received.
  const ease = 1 - vizOptions.smoothing;
  for (let i = 0; i < VIZ_BANDS; i++) {
    vizDrawn[i] += (vizTarget[i] - vizDrawn[i]) * ease;
  }

  if (vizOptions.style === "radial") drawRadial(ctx, box);
  else if (vizOptions.style === "wave") drawWave(ctx, box);
  else drawBars(ctx, box, vizOptions.style === "mirror");

  vizFrame = requestAnimationFrame(vizRender);
}

/** Take a frame of levels from the daemon. */
function vizFeed(bands) {
  if (!Array.isArray(bands)) return;
  for (let i = 0; i < VIZ_BANDS; i++) {
    const level = (bands[i] || 0) / 255;
    vizTarget[i] = Math.min(1, level * vizOptions.sensitivity);
  }
}

function vizPaintChrome() {
  const view = document.getElementById("visualiser");
  if (!view) return;

  const track = state?.track;
  const art = track?.album?.images?.[0]?.url || track?.coverUrl || "";

  view.dataset.backdrop = vizOptions.backdrop;
  view.style.setProperty("--viz-art", art ? `url("${art}")` : "none");

  const info = view.querySelector(".viz-track");
  if (info) {
    info.hidden = !vizOptions.showTrack;
    info.innerHTML = track
      ? `${art ? `<img src="${esc(art)}" alt="" />` : `<div class="thumb"></div>`}
         <div class="meta">
           <div class="name">${esc(track.name)}</div>
           <div class="artist">${esc(artistNames(track))}</div>
         </div>`
      : "";
  }

  // Nothing is analysed when the sound is coming out of another device.
  const remote = view.querySelector(".viz-remote");
  if (remote) {
    const elsewhere = state?.remoteDevice;
    remote.hidden = !elsewhere;
    remote.textContent = elsewhere
      ? `Playing on ${elsewhere} — the visualiser can only draw audio coming out of this computer.`
      : "";
  }
}

function vizOptionsPanel() {
  const chips = (name, list, current) =>
    list
      .map(
        ([key, label]) =>
          `<button class="chip${key === current ? " active" : ""}" data-${name}="${key}">${label}</button>`
      )
      .join("");

  return `
    <div class="viz-panel" id="viz-panel" hidden>
      <div class="viz-group"><span>Style</span>
        <div class="lib-filters">${chips("style", VIZ_STYLES, vizOptions.style)}</div>
      </div>
      <div class="viz-group"><span>Colour</span>
        <div class="lib-filters">${chips("palette", VIZ_PALETTES, vizOptions.palette)}</div>
      </div>
      <div class="viz-group"><span>Background</span>
        <div class="lib-filters">${chips("backdrop", VIZ_BACKDROPS, vizOptions.backdrop)}</div>
      </div>
      <div class="viz-group"><span>Smoothing</span>
        <input type="range" id="viz-smoothing" min="0" max="0.92" step="0.02"
               value="${vizOptions.smoothing}" />
      </div>
      <div class="viz-group"><span>Sensitivity</span>
        <input type="range" id="viz-sensitivity" min="0.6" max="2.4" step="0.05"
               value="${vizOptions.sensitivity}" />
      </div>
      <div class="viz-group">
        <label class="viz-check">
          <input type="checkbox" id="viz-showtrack" ${vizOptions.showTrack ? "checked" : ""} />
          Show what is playing
        </label>
      </div>
    </div>`;
}

function vizWire(view) {
  const panel = view.querySelector("#viz-panel");

  view.querySelector("#viz-settings").onclick = () => {
    panel.hidden = !panel.hidden;
  };
  view.querySelector("#viz-close").onclick = () => setVisualiser(false);

  ["style", "palette", "backdrop"].forEach((name) => {
    view.querySelectorAll(`[data-${name}]`).forEach((b) => {
      b.onclick = () => {
        vizOptions[name] = b.dataset[name];
        vizSave();
        view
          .querySelectorAll(`[data-${name}]`)
          .forEach((o) => o.classList.toggle("active", o === b));
        vizPaintChrome();
      };
    });
  });

  view.querySelector("#viz-smoothing").oninput = (e) => {
    vizOptions.smoothing = Number(e.target.value);
    vizSave();
  };
  view.querySelector("#viz-sensitivity").oninput = (e) => {
    vizOptions.sensitivity = Number(e.target.value);
    vizSave();
  };
  view.querySelector("#viz-showtrack").onchange = (e) => {
    vizOptions.showTrack = e.target.checked;
    vizSave();
    vizPaintChrome();
  };
}

/** Open or close the visualiser. */
async function setVisualiser(on) {
  const view = document.getElementById("visualiser");
  if (!view) return;

  vizOpen = on;
  document.body.classList.toggle("viz", on);
  view.hidden = !on;
  document.getElementById("btn-viz")?.classList.toggle("on", on);

  clearInterval(vizKeepalive);
  cancelAnimationFrame(vizFrame);

  if (!on) {
    vizTarget.fill(0);
    vizDrawn.fill(0);
    try {
      await call({ cmd: "watchSpectrum", on: false });
    } catch {
      /* the daemon gives up on its own if it never hears again */
    }
    return;
  }

  view.innerHTML = `
    <canvas id="viz-canvas"></canvas>
    <div class="viz-chrome">
      <button class="viz-btn" id="viz-settings" title="Options">${icon("dots")}</button>
      <button class="viz-btn" id="viz-close" title="Close">${icon("x")}</button>
    </div>
    ${vizOptionsPanel()}
    <div class="viz-track"></div>
    <p class="viz-remote" hidden></p>`;

  vizWire(view);
  vizPaintChrome();

  const ask = () => call({ cmd: "watchSpectrum", on: true }).catch(() => {});
  await ask();
  // The daemon forgets after fifteen seconds without hearing from a window.
  vizKeepalive = setInterval(ask, 5000);

  vizFrame = requestAnimationFrame(vizRender);
}

/** Is the extension switched on? On unless someone said otherwise. */
function visualiserEnabled() {
  return localStorage.getItem(VIZ_ON_KEY) !== "0";
}

/** Switch the extension on or off, which shows or hides its button. */
function enableVisualiser(on) {
  localStorage.setItem(VIZ_ON_KEY, on ? "1" : "0");
  const button = document.getElementById("btn-viz");
  if (button) button.hidden = !on;
  // Leaving the view up after switching the extension off would strand it
  // with no way back to it.
  if (!on && vizOpen) setVisualiser(false);
}

enableVisualiser(visualiserEnabled());

document.getElementById("btn-viz")?.addEventListener("click", () => setVisualiser(!vizOpen));

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && vizOpen) setVisualiser(false);
});
