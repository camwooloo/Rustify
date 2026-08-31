/* Rustify's page: the planet, the sky behind it, and the download that
 * matches whoever is reading.
 *
 * No libraries. The ring is CSS 3D, the sky is a canvas, and the downloads
 * come from the GitHub releases API at read time, so a new release needs no
 * change here.
 */

const REPO = "camwooloo/Rustify";
const RELEASES = `https://github.com/${REPO}/releases/latest`;
const still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

/* ───────────────────────────────────────────────────── the ring */

/** Lay the bars around a circle in 3D, then keep them moving like levels. */
function buildRing() {
  const ring = document.getElementById("ring");
  const rig = document.getElementById("rig");
  if (!ring || !rig) return;

  const COUNT = 56;
  const bars = [];

  for (let i = 0; i < COUNT; i++) {
    const bar = document.createElement("i");
    bar.style.setProperty("--a", `${(360 / COUNT) * i}deg`);
    ring.append(bar);
    bars.push(bar);
  }

  // The radius is in pixels, so it has to follow the element's size.
  const fit = () => {
    const r = Math.round(rig.getBoundingClientRect().width * 0.44);
    bars.forEach((bar) => bar.style.setProperty("--r", `${r}px`));
  };
  fit();
  addEventListener("resize", fit, { passive: true });

  if (still) {
    // A ring of even bars still reads as a ring; it just does not dance.
    bars.forEach((bar, i) => bar.style.setProperty("--s", 1 + 0.4 * Math.abs(Math.sin(i))));
    return;
  }

  // Two waves at unrelated speeds, so the pattern never obviously repeats.
  let spin = 0;
  let pointerX = 0;
  let pointerY = 0;
  let tiltX = 0;
  let tiltY = 0;

  rig.style.transition = "none";

  addEventListener(
    "pointermove",
    (e) => {
      pointerX = (e.clientX / innerWidth - 0.5) * 2;
      pointerY = (e.clientY / innerHeight - 0.5) * 2;
    },
    { passive: true },
  );

  const frame = (now) => {
    const t = now / 1000;
    spin = (spin + 0.12) % 360;

    // Ease towards the pointer rather than snapping to it.
    tiltX += (pointerY * -7 - tiltX) * 0.05;
    tiltY += (pointerX * 10 - tiltY) * 0.05;

    rig.style.transform = `rotateX(${tiltX}deg) rotateY(${spin + tiltY}deg)`;

    for (let i = 0; i < COUNT; i++) {
      const a = Math.sin(t * 2.1 + i * 0.42);
      const b = Math.sin(t * 0.9 - i * 0.17);
      const level = 0.45 + 0.55 * Math.abs(a * 0.7 + b * 0.45);
      bars[i].style.setProperty("--s", level.toFixed(3));
    }

    requestAnimationFrame(frame);
  };

  requestAnimationFrame(frame);
}

/* ────────────────────────────────────────────────────── the sky */

function buildStars() {
  const canvas = document.getElementById("stars");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");

  let stars = [];
  let w = 0;
  let h = 0;

  const seed = () => {
    const dpr = Math.min(devicePixelRatio || 1, 2);
    w = canvas.width = innerWidth * dpr;
    h = canvas.height = innerHeight * dpr;
    canvas.style.width = `${innerWidth}px`;
    canvas.style.height = `${innerHeight}px`;

    const count = Math.round((innerWidth * innerHeight) / 5200);
    stars = Array.from({ length: count }, () => {
      // Depth decides size, brightness and how far it drifts, which is what
      // makes the field read as space rather than as confetti.
      const depth = Math.random();
      return {
        x: Math.random() * w,
        y: Math.random() * h,
        depth,
        r: (0.4 + depth * 1.5) * dpr,
        a: 0.25 + depth * 0.6,
        phase: Math.random() * Math.PI * 2,
        green: Math.random() < 0.07,
      };
    });
  };

  let px = 0;
  let py = 0;

  addEventListener(
    "pointermove",
    (e) => {
      px = (e.clientX / innerWidth - 0.5) * 2;
      py = (e.clientY / innerHeight - 0.5) * 2;
    },
    { passive: true },
  );

  const draw = (now) => {
    ctx.clearRect(0, 0, w, h);
    const t = now / 1000;

    for (const s of stars) {
      const drift = s.depth * 26;
      const x = s.x + px * drift;
      const y = s.y + py * drift;
      const twinkle = still ? 1 : 0.75 + 0.25 * Math.sin(t * (0.6 + s.depth) + s.phase);

      ctx.globalAlpha = s.a * twinkle;
      ctx.fillStyle = s.green ? "#5ff08e" : "#dfe9f4";
      ctx.beginPath();
      ctx.arc(x, y, s.r, 0, Math.PI * 2);
      ctx.fill();
    }

    if (!still) requestAnimationFrame(draw);
  };

  seed();
  addEventListener("resize", () => {
    seed();
    if (still) draw(0);
  }, { passive: true });

  requestAnimationFrame(draw);
}

/* ─────────────────────────────────────────────── cards that tilt */

function wireTilt() {
  if (still) return;

  for (const card of document.querySelectorAll(".tilt")) {
    card.addEventListener(
      "pointermove",
      (e) => {
        const box = card.getBoundingClientRect();
        const x = (e.clientX - box.left) / box.width - 0.5;
        const y = (e.clientY - box.top) / box.height - 0.5;
        card.style.transform = `perspective(900px) rotateY(${x * 6}deg) rotateX(${-y * 6}deg)`;
      },
      { passive: true },
    );

    card.addEventListener("pointerleave", () => {
      card.style.transform = "";
    });
  }
}

/* ───────────────────────────────────────────── reveal on scroll */

function wireReveals() {
  if (still) return;

  const targets = [...document.querySelectorAll(".band, .numbers, .card, .plat")];
  targets.forEach((el) => el.classList.add("reveal"));

  // Deliberately not an IntersectionObserver: a fast scroll — a flick, or a
  // jump to an anchor — coalesces its callbacks, and anything it skips stays
  // invisible until you scroll back to it. A read of the geometry cannot
  // miss anything, and one per frame is cheap.
  let queued = false;

  const check = () => {
    queued = false;
    const edge = innerHeight * 0.88;
    let shown = 0;

    for (const el of targets) {
      if (el.classList.contains("seen")) continue;
      if (el.getBoundingClientRect().top > edge) continue;

      // A short stagger, so a row of cards arrives in order.
      el.style.transitionDelay = `${Math.min(shown * 70, 280)}ms`;
      el.classList.add("seen");
      shown += 1;
    }
  };

  const ping = () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(check);
  };

  addEventListener("scroll", ping, { passive: true });
  addEventListener("resize", ping, { passive: true });
  addEventListener("load", ping);
  check();
}

/* ───────────────────────────────────────────────────── platform */

const ICONS = {
  windows:
    '<svg viewBox="0 0 24 24"><path d="M3 5.5 10.2 4.5v6.7H3V5.5Zm0 13L10.2 19.5v-6.6H3v5.6Zm8.4 1.2L21 21V12.9h-9.6v6.8Zm0-15.4v6.9H21V3l-9.6 1.3Z"/></svg>',
  macos:
    '<svg viewBox="0 0 24 24"><path d="M16.2 12.6c0-2.2 1.8-3.3 1.9-3.3-1-1.5-2.6-1.7-3.2-1.7-1.4-.1-2.7.8-3.3.8-.7 0-1.7-.8-2.8-.8-1.5 0-2.8.8-3.6 2.1-1.5 2.6-.4 6.5 1.1 8.6.7 1 1.6 2.2 2.7 2.2s1.5-.7 2.8-.7 1.6.7 2.8.7 1.9-1 2.6-2c.8-1.2 1.1-2.3 1.2-2.3-.1 0-2.2-.9-2.2-3.6ZM14.3 5.6c.6-.7 1-1.7.9-2.7-.9 0-2 .6-2.6 1.3-.6.6-1.1 1.6-.9 2.6 1 .1 2-.5 2.6-1.2Z"/></svg>',
  linux:
    '<svg viewBox="0 0 24 24"><path d="M12 2.2c-2.5 0-3.6 1.9-3.5 4.2.1 1.6-.1 2.5-.8 3.7-.8 1.4-2 2.7-2.5 4.4-.4 1.4.2 2.2 1.1 2.3.6.1.8.4 1 .9.4 1.2 1.8 2.1 4.7 2.1s4.3-.9 4.7-2.1c.2-.5.4-.8 1-.9.9-.1 1.5-.9 1.1-2.3-.5-1.7-1.7-3-2.5-4.4-.7-1.2-.9-2.1-.8-3.7.1-2.3-1-4.2-3.5-4.2Zm-1.6 3.1c.4 0 .7.4.7 1s-.3 1-.7 1-.7-.4-.7-1 .3-1 .7-1Zm3.2 0c.4 0 .7.4.7 1s-.3 1-.7 1-.7-.4-.7-1 .3-1 .7-1ZM12 8.5c.9 0 1.9.5 1.9 1s-1 1.1-1.9 1.1-1.9-.6-1.9-1.1 1-1 1.9-1Z"/></svg>',
};

/** Which build is this reader most likely to want? */
async function detect() {
  const ua = navigator.userAgent;
  const data = navigator.userAgentData;

  if (/Android/i.test(ua)) return { os: "linux", arch: null, mobile: true };
  if (/iPhone|iPad|iPod/i.test(ua)) return { os: "macos", arch: "arm", mobile: true };
  if (/Windows|Win64|WOW64/i.test(ua)) return { os: "windows", arch: "x64" };
  if (/Linux|X11|CrOS/i.test(ua)) return { os: "linux", arch: "x64" };

  if (/Mac|Darwin/i.test(ua)) {
    // Chrome and Edge will say outright; Safari will not, so the GPU's name
    // is asked instead — an Apple Silicon Mac reports "Apple M…".
    if (data && data.getHighEntropyValues) {
      try {
        const hints = await data.getHighEntropyValues(["architecture"]);
        if (hints.architecture) {
          return { os: "macos", arch: hints.architecture === "arm" ? "arm" : "x64" };
        }
      } catch {
        /* Fall through to the GPU. */
      }
    }

    try {
      const gl = document.createElement("canvas").getContext("webgl");
      const info = gl && gl.getExtension("WEBGL_debug_renderer_info");
      const name = info ? gl.getParameter(info.UNMASKED_RENDERER_WEBGL) : "";
      if (/apple\s*m\d/i.test(name)) return { os: "macos", arch: "arm" };
      if (name) return { os: "macos", arch: "x64" };
    } catch {
      /* No WebGL, no idea. */
    }

    // Newer Macs outnumber the older ones by now.
    return { os: "macos", arch: "arm" };
  }

  return { os: null, arch: null };
}

/* ───────────────────────────────────────────────────── downloads */

const KINDS = [
  {
    os: "windows",
    label: "Installer",
    note: "x64 · .exe",
    match: (n) => n.endsWith("-setup.exe") || n.endsWith(".msi"),
  },
  {
    os: "macos",
    arch: "arm",
    label: "Apple Silicon",
    note: "M1 and later · .dmg",
    match: (n) => n.endsWith(".dmg") && /aarch64|arm64/.test(n),
  },
  {
    os: "macos",
    arch: "x64",
    label: "Intel",
    note: "x86_64 · .dmg",
    match: (n) => n.endsWith(".dmg") && /x64|x86_64|intel/.test(n),
  },
  {
    os: "linux",
    label: "Debian · Ubuntu · Mint",
    note: ".deb",
    match: (n) => n.endsWith(".deb"),
  },
  {
    os: "linux",
    label: "Fedora · RHEL · openSUSE",
    note: ".rpm",
    match: (n) => n.endsWith(".rpm"),
  },
  {
    os: "linux",
    label: "Everything else",
    note: "Arch, portable · .AppImage",
    match: (n) => n.endsWith(".appimage"),
  },
];

const mb = (bytes) => `${(bytes / 1048576).toFixed(1)} MB`;

const OS_NAME = { windows: "Windows", macos: "macOS", linux: "Linux" };

async function wireDownloads() {
  const me = await detect();

  const primary = document.getElementById("dl-main");
  const label = document.getElementById("dl-label");
  const meta = document.getElementById("dl-meta");
  const icon = document.getElementById("dl-icon");
  const line = document.getElementById("rel-line");

  if (me.os) {
    icon.innerHTML = ICONS[me.os];
    label.textContent = `Download for ${OS_NAME[me.os]}`;
    document
      .querySelector(`.plat[data-os="${me.os}"]`)
      ?.setAttribute("data-yours", "true");
  }

  let release;
  try {
    const response = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
      headers: { Accept: "application/vnd.github+json" },
    });
    if (!response.ok) throw new Error(String(response.status));
    release = await response.json();
  } catch {
    // GitHub rate-limits by address, and someone may simply be offline. The
    // page still works: every link already points at the releases page.
    line.textContent = "Couldn't reach GitHub just now — the releases page has every build.";
    for (const slot of document.querySelectorAll(".links")) {
      const os = slot.dataset.slot;
      slot.innerHTML = KINDS.filter((k) => k.os === os)
        .map(
          (k) =>
            `<li><a href="${RELEASES}"><b>${k.label}</b><small>${k.note}</small></a></li>`,
        )
        .join("");
    }
    return;
  }

  const version = (release.tag_name || "").replace(/^v/, "");
  const assets = release.assets || [];
  const find = (kind) => assets.find((a) => kind.match(a.name.toLowerCase()));

  const when = release.published_at
    ? new Date(release.published_at).toLocaleDateString(undefined, {
        day: "numeric",
        month: "long",
        year: "numeric",
      })
    : "";

  line.innerHTML = `Latest release <strong>v${version}</strong>${when ? ` · ${when}` : ""}.`;

  // Fill each platform's column with what the release actually carries.
  for (const slot of document.querySelectorAll(".links")) {
    const os = slot.dataset.slot;
    const rows = KINDS.filter((k) => k.os === os).map((kind) => {
      const asset = find(kind);
      if (!asset) {
        return `<li><a href="${RELEASES}"><b>${kind.label}</b><small>not in this release</small></a></li>`;
      }
      return `<li><a href="${asset.browser_download_url}"><b>${kind.label}</b><small>${kind.note} · ${mb(
        asset.size,
      )}</small></a></li>`;
    });
    slot.innerHTML = rows.join("");
  }

  // And point the big button at the one build this reader wants.
  const mine = KINDS.find(
    (k) => k.os === me.os && (!k.arch || k.arch === me.arch) && find(k),
  );

  if (mine) {
    const asset = find(mine);
    primary.href = asset.browser_download_url;
    meta.textContent = `v${version} · ${mb(asset.size)} · ${mine.note.replace(" · ", " ")}`;
  } else if (me.mobile) {
    label.textContent = "Rustify is a desktop app";
    meta.textContent = "open this page on your computer";
  } else {
    meta.textContent = `v${version} · choose a build`;
    primary.href = "#download";
  }
}

/* ───────────────────────────────────────────────────────── boot */

buildRing();
buildStars();
wireTilt();
wireReveals();
wireDownloads();
