/* Theming, shared by the main window and the miniplayer.
 *
 * Loaded before either of their scripts so a saved theme is applied before
 * anything paints, and kept in its own file because both windows need it —
 * a miniplayer wearing the default colours beside a themed main window looks
 * like a bug, and is one.
 */

/* ---------------------------------------------------------- theming */

/* Spicetify themes are colour schemes: a palette named after the surfaces of
 * the Spotify client. Rustify's variables describe those same surfaces, so a
 * scheme is applied by mapping one set of names onto the other and filling in
 * the shades a scheme does not carry.
 *
 * Only colours cross over. A theme's user.css is written against Spotify's
 * own class names and would find nothing here, so it is never fetched. */

const THEME_KEY = "rustify.theme";
const LAYOUT_KEY = "rustify.layout";

/** "1db954" or "#1db954" or "abc" -> [r, g, b]. */
function parseHex(hex) {
  let h = String(hex || "").replace("#", "").trim();
  if (h.length === 3) h = h.split("").map((c) => c + c).join("");
  if (h.length === 8) h = h.slice(0, 6);
  if (h.length !== 6 || !/^[0-9a-f]{6}$/i.test(h)) return null;
  return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16));
}

const toHex = (rgb) =>
  "#" + rgb.map((v) => Math.max(0, Math.min(255, Math.round(v))).toString(16).padStart(2, "0")).join("");

/** Blend two colours. `t` of 0 is all `a`, 1 is all `b`. */
const mix = (a, b, t) => a.map((v, i) => v + (b[i] - v) * t);

/** Perceived brightness, for deciding what reads on top of a colour. */
const luma = ([r, g, b]) => (0.299 * r + 0.587 * g + 0.114 * b) / 255;

/** Turn a Spicetify palette into the variables the stylesheet reads.
 *
 * A scheme names the surfaces it cares about and leaves the rest to the
 * theme's CSS, which we do not have — so every shade in between is derived
 * from the ones it does name, by moving a surface towards the text colour.
 * That is what keeps hovers and menus visible on a light scheme instead of
 * washing out against a white tint meant for a dark one.
 */
function schemeToVars(colors) {
  const pick = (...keys) => {
    for (const k of keys) {
      const rgb = parseHex(colors[k]);
      if (rgb) return rgb;
    }
    return null;
  };

  const main = pick("main", "background", "base") || [18, 18, 18];
  const text = pick("text", "foreground") || [255, 255, 255];
  const subtext = pick("subtext", "text-secondary") || mix(text, main, 0.35);
  const button = pick("button", "accent", "primary") || [30, 215, 96];
  const buttonActive = pick("button-active", "button-hover") || mix(button, text, 0.2);
  const card = pick("card", "main-elevated") || mix(main, text, 0.04);
  const sidebar = pick("sidebar", "main") || main;
  const player = pick("player", "main") || main;
  const selected = pick("selected-row", "highlight") || mix(main, text, 0.08);
  const shadow = pick("shadow") || [0, 0, 0];

  // A tint the same hue as the text, so it shows on light and dark alike.
  const tint = (alpha) => `rgba(${text.map(Math.round).join(", ")}, ${alpha})`;

  return {
    "--accent": toHex(button),
    "--accent-hi": toHex(buttonActive),
    "--accent-rgb": button.map(Math.round).join(", "),
    "--on-accent": luma(button) > 0.55 ? "#000" : "#fff",

    "--bg-0": toHex(player),
    "--bg-1": toHex(player),
    "--surface": toHex(main),
    "--sidebar": toHex(sidebar),
    "--surface-2": toHex(mix(main, text, 0.08)),
    "--card": toHex(card),
    "--card-hi": toHex(mix(card, text, 0.1)),
    "--field": toHex(mix(main, text, 0.1)),
    "--chip": toHex(mix(main, text, 0.09)),
    "--menu": toHex(mix(card, text, 0.06)),
    "--row-hover": toHex(selected),

    "--text": toHex(text),
    "--text-dim": toHex(subtext),
    "--text-mute": toHex(mix(subtext, main, 0.3)),

    "--hover": tint(0.1),
    "--hover-hi": tint(0.16),
    "--tint": tint(0.16),
    "--tint-soft": tint(0.07),
    "--stroke-2": tint(0.25),
    "--hairline": tint(0.08),
    "--shadow": `0 8px 24px rgba(${shadow.map(Math.round).join(", ")}, 0.5)`,
  };
}

/** Apply a scheme, or clear back to Spotify's own colours with no argument. */
function applyTheme(entry) {
  let style = document.getElementById("spice-vars");
  if (!entry) {
    if (style) style.remove();
    document.documentElement.style.colorScheme = "dark";
    localStorage.removeItem(THEME_KEY);
    return;
  }

  const vars = schemeToVars(entry.colors);
  const body = Object.entries(vars)
    .map(([k, v]) => `${k}:${v};`)
    .join(" ");

  if (!style) {
    style = document.createElement("style");
    style.id = "spice-vars";
    document.head.append(style);
  }
  style.textContent = `:root { ${body} }`;

  // Form controls and scrollbars follow this, not our variables.
  document.documentElement.style.colorScheme =
    luma(parseHex(vars["--surface"])) > 0.5 ? "light" : "dark";

  localStorage.setItem(THEME_KEY, JSON.stringify(entry));
}

/** The saved theme, or null for Spotify's own. */
function savedTheme() {
  try {
    const raw = localStorage.getItem(THEME_KEY);
    return raw ? JSON.parse(raw) : null;
  } catch {
    return null;
  }
}

/** Rearrange the interface. Layouts are structure, themes are colour, and
 * the two are independent: every rule in layouts.css reads the same
 * variables the base does, so any layout wears any scheme.
 *
 * `spotify` is the base itself, so it carries no attribute at all. */
function applyLayout(name) {
  const layout = name && name !== "spotify" ? name : "";
  if (layout) {
    document.body.dataset.layout = layout;
    localStorage.setItem(LAYOUT_KEY, layout);
  } else {
    delete document.body.dataset.layout;
    localStorage.removeItem(LAYOUT_KEY);
  }
}

const savedLayout = () => localStorage.getItem(LAYOUT_KEY) || "spotify";

// Applied before anything renders: the colours are stored with the choice, so
// startup never waits on the network to avoid a flash of the wrong palette.
applyTheme(savedTheme());
applyLayout(savedLayout());
