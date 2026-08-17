//! Taskbar thumbnail toolbar — the buttons under the taskbar preview.
//!
//! Hovering Rustify in the taskbar now gives the same row the official app
//! has: like, previous, play/pause, next. It matters more here than in most
//! players, because closing the window only hides it — the preview is often
//! the fastest way back to the transport without restoring the window at all.
//!
//! Two Win32 pieces are needed. `ITaskbarList3` registers the buttons, and a
//! window subclass catches the `WM_COMMAND` the shell posts when one is
//! clicked, plus the `TaskbarButtonCreated` broadcast that tells us when the
//! taskbar button exists (and again if Explorer restarts).
//!
//! The icons are drawn here rather than shipped as `.ico` files: four small
//! monochrome glyphs are less code than the build plumbing to embed them, and
//! drawing them means they come out crisp at whatever size the shell asks for.

use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tracing::{debug, warn};
use windows::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{
            CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
            DIB_RGB_COLORS,
        },
        System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
        UI::{
            Shell::{
                DefSubclassProc, ITaskbarList3, RemoveWindowSubclass, SetWindowSubclass,
                TaskbarList, THBF_DISABLED, THBF_ENABLED, THBN_CLICKED, THB_FLAGS, THB_ICON,
                THB_TOOLTIP, THUMBBUTTON,
            },
            WindowsAndMessaging::{
                CreateIconIndirect, GetSystemMetrics, KillTimer, RegisterWindowMessageW, SetTimer,
                HICON, ICONINFO, SM_CXSMICON, WM_COMMAND, WM_NCDESTROY, WM_SHOWWINDOW, WM_TIMER,
            },
        },
    },
};

use crate::link::DaemonLink;

const ID_LIKE: u32 = 1;
const ID_PREV: u32 = 2;
const ID_PLAY: u32 = 3;
const ID_NEXT: u32 = 4;

/// Any value; it only has to be unique among subclasses of this window.
const SUBCLASS_ID: usize = 0x5255_5354;

/// One-shot timer that re-registers the buttons after the window reappears.
const REGISTER_TIMER: usize = 0x5255_5355;

/// The shell broadcasts this when a window's taskbar button is created. It is
/// registered once and read from the subclass proc.
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

/// What the buttons are currently showing.
#[derive(Default)]
struct Playback {
    playing: bool,
    /// URI of the playing track, needed by the like button.
    uri: Option<String>,
    saved: bool,
}

pub struct ThumbBar {
    hwnd: HWND,
    taskbar: ITaskbarList3,
    icons: Icons,
    link: Arc<DaemonLink>,
    state: Mutex<Playback>,
    /// The shell accepts one `ThumbBarAddButtons` per window; everything after
    /// that has to be an update.
    added: Mutex<bool>,
}

// The COM interface and the icon handles are only ever touched from the
// window's own thread: the subclass proc runs there, and `apply_event` hops
// onto it before drawing. The state behind them is a normal mutex.
unsafe impl Send for ThumbBar {}
unsafe impl Sync for ThumbBar {}

struct Icons {
    like: HICON,
    liked: HICON,
    prev: HICON,
    play: HICON,
    pause: HICON,
    next: HICON,
}

impl ThumbBar {
    /// Attach the toolbar to the app window.
    pub fn new(hwnd_raw: isize, link: Arc<DaemonLink>) -> Result<Arc<Self>> {
        let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);

        let taskbar: ITaskbarList3 = unsafe {
            CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)
                .context("creating the taskbar list")?
        };
        unsafe { taskbar.HrInit().context("initialising the taskbar list")? };

        let size = icon_size();
        let icons = Icons {
            like: glyph::make(glyph::Kind::Like, size)?,
            liked: glyph::make(glyph::Kind::Liked, size)?,
            prev: glyph::make(glyph::Kind::Previous, size)?,
            play: glyph::make(glyph::Kind::Play, size)?,
            pause: glyph::make(glyph::Kind::Pause, size)?,
            next: glyph::make(glyph::Kind::Next, size)?,
        };

        let bar = Arc::new(Self {
            hwnd,
            taskbar,
            icons,
            link,
            state: Mutex::new(Playback::default()),
            added: Mutex::new(false),
        });

        TASKBAR_CREATED.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) });

        // The proc owns a reference for as long as the window lives; it is
        // reclaimed on WM_NCDESTROY.
        let owned = Arc::into_raw(bar.clone()) as usize;
        let ok = unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, owned) };
        if !ok.as_bool() {
            // Take the reference back rather than leaking it.
            drop(unsafe { Arc::from_raw(owned as *const ThumbBar) });
            anyhow::bail!("could not subclass the window");
        }

        // Usually too early — the taskbar button does not exist yet, and the
        // broadcast below does the real work. Harmless to try, and it covers
        // the case where the window was already shown.
        bar.refresh();

        Ok(bar)
    }

    /// Build the current row. Rebuilt from scratch each time; it is six
    /// pointer copies, and it keeps the state in exactly one place.
    fn buttons(&self) -> [THUMBBUTTON; 4] {
        let st = self.state.lock().unwrap();
        let mask = THB_ICON | THB_TOOLTIP | THB_FLAGS;

        let (like_icon, like_tip) = if st.saved {
            (self.icons.liked, "Remove from your Liked Songs")
        } else {
            (self.icons.like, "Save to your Liked Songs")
        };

        let (play_icon, play_tip) = if st.playing {
            (self.icons.pause, "Pause")
        } else {
            (self.icons.play, "Play")
        };

        [
            THUMBBUTTON {
                dwMask: mask,
                iId: ID_LIKE,
                hIcon: like_icon,
                szTip: tip(like_tip),
                // Nothing to save when nothing is playing.
                dwFlags: if st.uri.is_some() {
                    THBF_ENABLED
                } else {
                    THBF_DISABLED
                },
                ..Default::default()
            },
            THUMBBUTTON {
                dwMask: mask,
                iId: ID_PREV,
                hIcon: self.icons.prev,
                szTip: tip("Previous"),
                dwFlags: THBF_ENABLED,
                ..Default::default()
            },
            THUMBBUTTON {
                dwMask: mask,
                iId: ID_PLAY,
                hIcon: play_icon,
                szTip: tip(play_tip),
                dwFlags: THBF_ENABLED,
                ..Default::default()
            },
            THUMBBUTTON {
                dwMask: mask,
                iId: ID_NEXT,
                hIcon: self.icons.next,
                szTip: tip("Next"),
                dwFlags: THBF_ENABLED,
                ..Default::default()
            },
        ]
    }

    /// Push the current row to the shell. Must run on the window's thread.
    ///
    /// Whether the shell wants an add or an update depends on state we cannot
    /// see — hiding to the tray destroys the taskbar button and takes the
    /// registration with it — so this tries the likely one and falls back
    /// rather than trusting its own bookkeeping. Getting that wrong once would
    /// otherwise freeze the buttons for the rest of the session.
    fn refresh(&self) {
        let buttons = self.buttons();
        let mut added = self.added.lock().unwrap();

        if *added {
            match unsafe { self.taskbar.ThumbBarUpdateButtons(self.hwnd, &buttons) } {
                Ok(()) => return,
                Err(e) => debug!("taskbar buttons need re-adding: {e}"),
            }
        }

        if unsafe { self.taskbar.ThumbBarAddButtons(self.hwnd, &buttons) }.is_ok() {
            *added = true;
            debug!("taskbar buttons registered");
            return;
        }

        // Already registered after all: the add is refused once per window.
        *added = unsafe { self.taskbar.ThumbBarUpdateButtons(self.hwnd, &buttons) }.is_ok();
        if !*added {
            // Normal before the taskbar button exists; the shell tells us when
            // it does.
            debug!("taskbar buttons not accepted yet");
        }
    }

    fn on_click(self: &Arc<Self>, id: u32) {
        let command = match id {
            ID_PREV => json!({ "cmd": "previous" }),
            ID_PLAY => json!({ "cmd": "playPause" }),
            ID_NEXT => json!({ "cmd": "next" }),
            ID_LIKE => {
                let (uri, saved) = {
                    let mut st = self.state.lock().unwrap();
                    let Some(uri) = st.uri.clone() else { return };
                    // Flip now so the icon answers the click immediately; the
                    // daemon's own state event confirms or corrects it.
                    st.saved = !st.saved;
                    (uri, st.saved)
                };
                self.refresh();
                json!({ "cmd": "setSaved", "uri": uri, "saved": saved })
            }
            _ => return,
        };

        let link = self.link.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = link.call(command).await {
                warn!("taskbar button failed: {e}");
            }
        });
    }

    /// Fold a daemon event in. Returns true when the buttons need redrawing.
    fn merge(&self, frame: &Value) -> bool {
        let mut st = self.state.lock().unwrap();
        let before = (st.playing, st.uri.clone(), st.saved);

        fn take_track(st: &mut Playback, track: &Value) {
            st.uri = track
                .get("uri")
                .and_then(Value::as_str)
                .map(str::to_string);
            st.saved = track.get("saved").and_then(Value::as_bool).unwrap_or(false);
        }

        match frame.get("event").and_then(Value::as_str) {
            Some("trackChanged") => take_track(&mut st, frame),
            Some("position") => {
                st.playing = frame
                    .get("playing")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
            Some("state") => {
                match frame.get("track") {
                    Some(track) if !track.is_null() => take_track(&mut st, track),
                    _ => {
                        st.uri = None;
                        st.saved = false;
                    }
                }
                st.playing = frame
                    .get("playing")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
            _ => return false,
        }

        before != (st.playing, st.uri.clone(), st.saved)
    }
}

/// Update the toolbar from a daemon event frame.
///
/// Driven by the same events the window renders from, so the buttons match
/// what is actually playing — including playback on another device.
pub fn apply_event(app: &tauri::AppHandle, frame: &Value) {
    use tauri::Manager;

    let Some(bar) = app.try_state::<Arc<ThumbBar>>() else {
        return;
    };
    let bar = bar.inner().clone();

    if !bar.merge(frame) {
        return;
    }

    // COM calls on this interface belong to the thread that owns the window.
    if let Err(e) = app.run_on_main_thread(move || bar.refresh()) {
        debug!("could not reach the main thread to redraw the taskbar: {e}");
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    data: usize,
) -> LRESULT {
    // Borrowed, not owned: the reference belongs to the window until it dies.
    let bar = ManuallyDrop::new(unsafe { Arc::from_raw(data as *const ThumbBar) });

    if msg == WM_COMMAND && (wparam.0 >> 16) as u32 == THBN_CLICKED {
        bar.on_click((wparam.0 & 0xffff) as u32);
        return LRESULT(0);
    }

    if Some(&msg) == TASKBAR_CREATED.get() {
        *bar.added.lock().unwrap() = false;
        bar.refresh();
    }

    // Closing Rustify only hides it, which destroys the taskbar button. When
    // it comes back from the tray the button is rebuilt, but not instantly —
    // hence the short timer rather than registering here and finding nothing
    // to register against.
    if msg == WM_SHOWWINDOW && wparam.0 != 0 {
        unsafe { SetTimer(Some(hwnd), REGISTER_TIMER, 500, None) };
    }

    if msg == WM_TIMER && wparam.0 == REGISTER_TIMER {
        unsafe {
            let _ = KillTimer(Some(hwnd), REGISTER_TIMER);
        }
        bar.refresh();
        return LRESULT(0);
    }

    if msg == WM_NCDESTROY {
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
            drop(Arc::from_raw(data as *const ThumbBar));
        }
    }

    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// A tooltip in the fixed-size buffer `THUMBBUTTON` wants.
fn tip(text: &str) -> [u16; 260] {
    let mut buf = [0u16; 260];
    for (slot, ch) in buf.iter_mut().zip(text.encode_utf16()).take(259) {
        *slot = ch;
    }
    buf
}

/// What the shell will draw the buttons at.
fn icon_size() -> u32 {
    let n = unsafe { GetSystemMetrics(SM_CXSMICON) };
    (n.max(16) as u32).min(48)
}

/// Drawing the button glyphs.
///
/// Each is described as shapes in a 24×24 space and sampled into an ARGB
/// bitmap, so one description covers every DPI.
mod glyph {
    use super::*;

    #[derive(Clone, Copy)]
    enum Shape {
        Rect { x: f32, y: f32, w: f32, h: f32 },
        Tri([(f32, f32); 3]),
        Disc { cx: f32, cy: f32, r: f32 },
        /// A circle outline of thickness `t`.
        Ring { cx: f32, cy: f32, r: f32, t: f32 },
        /// A round-capped line.
        Line { a: (f32, f32), b: (f32, f32), w: f32 },
    }

    impl Shape {
        fn hit(&self, px: f32, py: f32) -> bool {
            match *self {
                Shape::Rect { x, y, w, h } => px >= x && px <= x + w && py >= y && py <= y + h,
                Shape::Tri(p) => {
                    let side = |a: (f32, f32), b: (f32, f32)| {
                        (b.0 - a.0) * (py - a.1) - (b.1 - a.1) * (px - a.0)
                    };
                    let (d0, d1, d2) = (side(p[0], p[1]), side(p[1], p[2]), side(p[2], p[0]));
                    let neg = d0 < 0.0 || d1 < 0.0 || d2 < 0.0;
                    let pos = d0 > 0.0 || d1 > 0.0 || d2 > 0.0;
                    !(neg && pos)
                }
                Shape::Disc { cx, cy, r } => (px - cx).powi(2) + (py - cy).powi(2) <= r * r,
                Shape::Ring { cx, cy, r, t } => {
                    let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                    (d - r).abs() <= t / 2.0
                }
                Shape::Line { a, b, w } => {
                    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                    let len2 = dx * dx + dy * dy;
                    let t = if len2 == 0.0 {
                        0.0
                    } else {
                        (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0)
                    };
                    let (nx, ny) = (a.0 + t * dx, a.1 + t * dy);
                    (px - nx).powi(2) + (py - ny).powi(2) <= (w / 2.0).powi(2)
                }
            }
        }
    }

    /// Sample the shapes into premultiplied white BGRA.
    ///
    /// `cut` is subtracted, which is how the tick is punched out of the filled
    /// circle instead of being drawn as a second colour.
    fn raster(size: u32, add: &[Shape], cut: &[Shape]) -> Vec<u8> {
        const SS: u32 = 4; // 4×4 samples a pixel: 17 levels of edge, plenty here

        let scale = 24.0 / size as f32;
        let mut out = vec![0u8; (size * size * 4) as usize];

        for y in 0..size {
            for x in 0..size {
                let mut hits = 0u32;
                for sy in 0..SS {
                    for sx in 0..SS {
                        let px = (x as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                        let py = (y as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                        if add.iter().any(|s| s.hit(px, py)) && !cut.iter().any(|s| s.hit(px, py)) {
                            hits += 1;
                        }
                    }
                }

                let alpha = (hits * 255 / (SS * SS)) as u8;
                let i = ((y * size + x) * 4) as usize;
                // White, premultiplied: every channel is the coverage.
                out[i] = alpha;
                out[i + 1] = alpha;
                out[i + 2] = alpha;
                out[i + 3] = alpha;
            }
        }
        out
    }

    /// Wrap a pixel buffer up as an icon the shell can own.
    fn icon(size: u32, pixels: &[u8]) -> Result<HICON> {
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            // Negative: top-down, matching the order `raster` writes.
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let info = BITMAPINFO {
            bmiHeader: header,
            ..Default::default()
        };

        let mut bits = std::ptr::null_mut();
        let colour = unsafe {
            CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)
                .context("creating the icon bitmap")?
        };
        unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len()) };

        // The alpha channel does the masking, but CreateIconIndirect still
        // insists on a mask bitmap. Scanlines are word-aligned.
        let stride = ((size as usize + 15) / 16) * 2;
        let mask_bits = vec![0u8; stride * size as usize];
        let mask = unsafe {
            CreateBitmap(
                size as i32,
                size as i32,
                1,
                1,
                Some(mask_bits.as_ptr() as *const _),
            )
        };

        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: colour,
        };
        let handle = unsafe { CreateIconIndirect(&info).context("creating the button icon")? };

        // The icon owns copies; these were only ever scaffolding.
        unsafe {
            let _ = DeleteObject(colour.into());
            let _ = DeleteObject(mask.into());
        }
        Ok(handle)
    }

    #[derive(Clone, Copy, Debug)]
    pub enum Kind {
        /// A plus in a circle, which is what Spotify shows for an unsaved track.
        Like,
        /// The filled tick it becomes once saved.
        Liked,
        Previous,
        Play,
        Pause,
        Next,
    }

    /// The shapes to draw, and the shapes to punch out of them.
    fn shapes(kind: Kind) -> (Vec<Shape>, Vec<Shape>) {
        match kind {
            Kind::Play => (vec![Shape::Tri([(8.5, 4.5), (8.5, 19.5), (20.0, 12.0)])], vec![]),

            Kind::Pause => (
                vec![
                    Shape::Rect { x: 7.2, y: 5.0, w: 3.4, h: 14.0 },
                    Shape::Rect { x: 13.4, y: 5.0, w: 3.4, h: 14.0 },
                ],
                vec![],
            ),

            Kind::Next => (
                vec![
                    Shape::Tri([(6.0, 5.0), (6.0, 19.0), (15.2, 12.0)]),
                    Shape::Rect { x: 16.2, y: 5.0, w: 2.6, h: 14.0 },
                ],
                vec![],
            ),

            Kind::Previous => (
                vec![
                    Shape::Tri([(18.0, 5.0), (18.0, 19.0), (8.8, 12.0)]),
                    Shape::Rect { x: 5.2, y: 5.0, w: 2.6, h: 14.0 },
                ],
                vec![],
            ),

            Kind::Like => (
                vec![
                    Shape::Ring { cx: 12.0, cy: 12.0, r: 8.2, t: 1.7 },
                    Shape::Rect { x: 11.15, y: 7.6, w: 1.7, h: 8.8 },
                    Shape::Rect { x: 7.6, y: 11.15, w: 8.8, h: 1.7 },
                ],
                vec![],
            ),

            Kind::Liked => (
                vec![Shape::Disc { cx: 12.0, cy: 12.0, r: 9.0 }],
                vec![
                    Shape::Line { a: (7.6, 12.2), b: (10.8, 15.4), w: 2.2 },
                    Shape::Line { a: (10.8, 15.4), b: (16.4, 8.6), w: 2.2 },
                ],
            ),
        }
    }

    pub fn make(kind: Kind, size: u32) -> Result<HICON> {
        let (add, cut) = shapes(kind);
        icon(size, &raster(size, &add, &cut))
    }

    /// Prints each glyph as text.
    ///
    /// Drawing icons in code is only worth it if you can see what came out,
    /// and the alternative — reading the coordinates and hoping — is how the
    /// lyrics microphone ended up looking like a paintbrush three times over.
    /// Run with `cargo test -p spotify-rust-app -- --nocapture`.
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn glyphs_look_right() {
            const SIZE: u32 = 26;

            for kind in [
                Kind::Like,
                Kind::Liked,
                Kind::Previous,
                Kind::Play,
                Kind::Pause,
                Kind::Next,
            ] {
                let (add, cut) = shapes(kind);
                let px = raster(SIZE, &add, &cut);

                println!("\n{kind:?}");
                for y in 0..SIZE {
                    let row: String = (0..SIZE)
                        .map(|x| {
                            match px[((y * SIZE + x) * 4 + 3) as usize] {
                                0..=31 => ' ',
                                32..=159 => '+',
                                _ => '#',
                            }
                        })
                        .collect();
                    println!("|{row}|");
                }

                // A glyph that drew nothing, or filled everything, is a bug in
                // the coordinates rather than something to eyeball.
                let lit = px.iter().skip(3).step_by(4).filter(|a| **a > 0).count();
                let total = (SIZE * SIZE) as usize;
                assert!(
                    lit > total / 20 && lit < total * 4 / 5,
                    "{kind:?} covers {lit}/{total} pixels"
                );
            }
        }
    }
}
