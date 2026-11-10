//! Widescreen (Hor+) in missions, as future_rust_cop's `wide.rs`.
//!
//! The game projects with separate x and y scales, which View_SetWindow_ (0x48d7d0) makes
//! from the view window (`[0x568af0]` x, `[0x568af4]` y): the fixed-point projection
//! matRotTransPers_, the float ones vec3fp16_Project*ListASM_ and others read them. In a
//! mission the x scale is narrowed by k = (4/3) / aspect (`apply`, before every logic frame
//! and picture): the wider view fits the game's 4:3 screen, and the renderer stretches it over
//! the whole wide frame (`glide::View::Full`).
//!
//! The HUD goes into the same ordering table as the 3D picture (HUDUserDraw_ sets a window of
//! its own only in some HUD modes). The primitives made while it runs (disp_AllocPoly_), and
//! those made under another window than the screen's (message boxes, the map screen), are
//! drawn unstretched in the frame's 4:3 middle (`glide::View::Hud(0.5)`, switched in
//! gl_RenderPoly_). While the HUD runs the x scale is the 4:3 one (HOMObjectIconDraw_ sizes
//! its sprites by it).
//!
//! The terrain drawn is the tiles in a trapezoid in front of the camera (ViewPort_Init_
//! 0x46f368: four corners in tiles); its x is widened by 1/k. The number of tiles sizes
//! TileMgr's lists at the mission's start, so that is made for at least 16:9 then; a wider
//! window later widens the trapezoid only up to it.
//!
//! The visible tiles are found by 0x467b04: 0x4672bc writes the trapezoid's rows (from +0x10,
//! 4 bytes a row) into a buffer on its stack (`[ebp-168h]`, room for 64 rows) and 0x4676d4
//! reads them. The wide trapezoid, turned, has more rows: they overwrote 0x467b04's saved
//! registers, and Rndr_TerrainRender_ went on with a wrong ebp (streaks of missing terrain
//! when flying forward and turning, then crashes, 2026-11-08). Both get a larger buffer of
//! ours instead (`trapezoid_rows`, `trapezoid_tiles`; they are called from there only).
//!
//! The mission menu (R: SMFD_Enter_ .. SMFD_Exit_) is 4:3 from its start: it draws its frame
//! once when it opens, and the target, still wide for the HUD's last frames, turned 4:3
//! afterwards: made anew, black (2026-11-08).
//!
//! The cockpit HUD modes (1 and 2: only at the game's 640x480; H cycles 0, 1, 2, off) are 4:3
//! too: a panel over the whole screen around a smaller 3D window, drawn whole once by
//! HUDUserModeSet_ and then only in parts. Stretched, it was drawn wide and its parts again in
//! the 4:3 middle (2026-11-09). HUDUserModeSet_ makes the target 4:3 before it draws.
//!
//! The game allows the cockpit modes only at 640x480 (HUDPrimInit_ makes the panel only there,
//! the HUD key skips them elsewhere); both checks are patched out (`PATCHES`), and the panel,
//! placed and scaled by the screen's size already, works at any resolution. HUDUserDraw_ draws
//! the whole panel only when its flag `[0x51f1b9]` says so (after a mode change), else the parts
//! that change: a target made anew (4:3, window size, MSAA) lost the rest. The flag is set for
//! the next two HUD frames then (`redraw`: one for each buffer).
//!
//! The aspect follows the window, set at every grBufferSwap (`set_aspect`): wide while the
//! HUD is drawn (a mission), else 4:3. Setting `widescreen` (F10 menu); `NSTRIKE_WIDE=<aspect>`
//! forces an aspect (0: off).

use super::{glide, mem, pictures};
use crate::rt::{Cpu, EAX, EBX, ECX, EDX, call_original};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub const SET_WINDOW: u32 = 0x48d7d0;
pub const VIEWPORT_INIT: u32 = 0x46f368;
pub const SMFD_ENTER: u32 = 0x48ea28;
pub const SMFD_EXIT: u32 = 0x48ec64;

/// The mission menu is open.
static SMFD: AtomicBool = AtomicBool::new(false);
pub const HUD_MODE_SET: u32 = 0x45f2b4;

/// The cockpit HUD at any resolution (the game allows it only at 640x480): HUDPrimInit_ makes
/// its panel (`jmp 461833`: past the check), and the HUD key goes from mode 0 to 1
/// (HUDCommandProcess_, `jmp 45e403`).
pub const PATCHES: &[(u32, &[u8], &[u8])] = &[
    (0x461804, &[0xa1, 0x24, 0x2f, 0x52, 0x00], &[0xeb, 0x2d, 0x90, 0x90, 0x90]),
    (0x45e3e3, &[0x8b, 0x15, 0x24, 0x2f, 0x52, 0x00], &[0xeb, 0x1e, 0x90, 0x90, 0x90, 0x90]),
];
/// A cockpit HUD mode is set.
static COCKPIT: AtomicBool = AtomicBool::new(false);
pub const TRAPEZOID_ROWS: u32 = 0x4672bc;
pub const TRAPEZOID_TILES: u32 = 0x4676d4;

/// The mission's 3D view window in HUD modes with their own HUD window.
const VIEW_3D: u32 = 0x51f1a4;
/// What View_SetWindow_ makes: the view's width, the x scale, the clip margin
/// (x scale * 0x1000 >> 16) and the right clip bound (2 * margin + width).
const WIDTH: u32 = 0x568ae0;
const SCALE_X: u32 = 0x568af0;
const MARGIN_X: u32 = 0x568af8;
const BOUND_X: u32 = 0x568b00;

const NARROW: f32 = 4.0 / 3.0;

/// k as f32 bits; 1.0 is off.
static K: AtomicU32 = AtomicU32::new(0x3f80_0000);

/// HUD frames still to draw the whole cockpit panel (`redraw`).
static REDRAW: AtomicU32 = AtomicU32::new(0);
/// HUDUserDraw_'s flag: draw the whole cockpit panel.
const DRAW_ALL: u32 = 0x51f1b9;

/// The Glide target was made anew: the cockpit panel is drawn whole again (both buffers).
pub fn redraw() {
    REDRAW.store(2, Ordering::Relaxed);
}

/// When the HUD was last drawn: frames are wide shortly after.
static SEEN: Mutex<Option<Instant>> = Mutex::new(None);

pub fn k() -> f32 {
    f32::from_bits(K.load(Ordering::Relaxed))
}

fn forced() -> Option<f32> {
    static S: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    *S.get_or_init(|| std::env::var("NSTRIKE_WIDE").ok().and_then(|v| v.trim().parse().ok()))
}

/// The aspect wanted for a window of `a` (4:3 when off).
fn wanted(a: f32) -> f32 {
    let a = match forced() {
        Some(f) if f == 0.0 => NARROW,
        Some(f) => f,
        None if crate::settings::get().widescreen => a,
        None => NARROW,
    };
    if a > NARROW + 0.01 { a } else { NARROW }
}

/// The aspect of the next frame for a window of aspect `a` (between frames): the window's
/// in a mission, else 4:3. Returns it; the renderer sizes its target by it.
pub fn set_aspect(a: f32) -> f32 {
    let a = if mission() { wanted(a) } else { NARROW };
    K.store((NARROW / a).to_bits(), Ordering::Relaxed);
    a
}

/// A new Glide screen: 4:3 until the HUD is drawn again.
pub fn reset() {
    *SEEN.lock().unwrap() = None;
    K.store(1.0f32.to_bits(), Ordering::Relaxed);
}

/// The projection's x scale as View_SetWindow_ made it for the screen (window 0 or the 3D
/// one), and whether such a window is set (else a message box's or the map screen's: their
/// drawing is unstretched, `Hud`).
static BASE: AtomicU32 = AtomicU32::new(0);
static WINDOW_3D: AtomicBool = AtomicBool::new(true);

fn mission() -> bool {
    !SMFD.load(Ordering::Relaxed) && !COCKPIT.load(Ordering::Relaxed) && SEEN.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(250))
}

/// SMFD_Enter_: the target 4:3 before the menu draws (see the module's comment).
pub fn smfd_enter(c: &mut Cpu) {
    SMFD.store(true, Ordering::Relaxed);
    glide::follow_window();
    call_original(c, SMFD_ENTER);
}

pub fn smfd_exit(c: &mut Cpu) {
    call_original(c, SMFD_EXIT);
    SMFD.store(false, Ordering::Relaxed);
}

/// HUDUserModeSet_(eax HUD, edx mode: 0 plain, 1 and 2 cockpit, 0xff off): the target 4:3
/// before a cockpit mode draws its panel, wide again at the next swap after it.
pub fn hud_mode_set(c: &mut Cpu) {
    let cockpit = matches!(c.r[EDX], 1 | 2);
    if cockpit != COCKPIT.swap(cockpit, Ordering::Relaxed) && cockpit {
        glide::follow_window();
    }
    call_original(c, HUD_MODE_SET);
}

/// Copies of a screen window made by View_GetWindow_ (a message box saves the window set,
/// draws under the screen's and sets the copy back: the Esc menu's box every frame). Before,
/// the copy counted as another window: the x scale stayed 4:3 and the 3D picture stretched,
/// also after the box closed (2026-11-10).
static COPIES: Mutex<Vec<u32>> = Mutex::new(Vec::new());

pub const GET_WINDOW: u32 = 0x48d618;
pub const FREE_WINDOW: u32 = 0x48d77c;

/// View_GetWindow_() -> eax a new copy of the window set.
pub fn get_window(c: &mut Cpu) {
    call_original(c, GET_WINDOW);
    let w = c.r[EAX];
    if w != 0 && WINDOW_3D.load(Ordering::Relaxed) {
        COPIES.lock().unwrap().push(w);
    }
}

/// View_FreeWindow_(eax window).
pub fn free_window(c: &mut Cpu) {
    let w = c.r[EAX];
    COPIES.lock().unwrap().retain(|&x| x != w);
    call_original(c, FREE_WINDOW);
}

/// View_SetWindow_(eax window, 0: the whole screen): returns 0 when set.
pub fn set_window(c: &mut Cpu) {
    let w = c.r[EAX];
    call_original(c, SET_WINDOW);
    let m = mem();
    if c.r[EAX] != 0 {
        return;
    }
    let three_d = w == 0 || w == m.rd32(VIEW_3D) || COPIES.lock().unwrap().contains(&w);
    WINDOW_3D.store(three_d, Ordering::Relaxed);
    if three_d {
        BASE.store(m.rd32(SCALE_X), Ordering::Relaxed);
        apply();
    }
}

/// The x scale narrowed by k (in a mission, with the screen's window set). Before a logic
/// frame and a picture (in its static data, which a picture puts back afterwards).
pub fn apply() {
    scale(if mission() { k() } else { 1.0 });
}

/// The screen window's x scale times `k`.
fn scale(k: f32) {
    let base = BASE.load(Ordering::Relaxed) as i32;
    if base == 0 || !WINDOW_3D.load(Ordering::Relaxed) {
        return;
    }
    let m = mem();
    let sx = (base as f32 * k) as i32;
    let margin = ((sx as i64 * 0x1000) >> 16) as i32;
    m.wr32(SCALE_X, sx as u32);
    m.wr32(MARGIN_X, margin as u32);
    m.wr32(BOUND_X, (2 * margin + m.rd32(WIDTH) as i32) as u32);
}

pub const ALLOC_POLY: u32 = 0x47af6c;
pub const RENDER_POLY: u32 = 0x47a44c;

thread_local! {
    /// HUDUserDraw_ runs (its primitives are the HUD's)
    static IN_HUD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// the primitives made by the HUD (or under another window) since the HUD last began
    static HUD: std::cell::RefCell<std::collections::HashSet<u32>> = std::cell::RefCell::new(std::collections::HashSet::new());
}

/// HUDUserDraw_ begins (`true`) or ends: once a mission frame. The HUD is drawn unstretched
/// with the x scale of 4:3 (HOMObjectIconDraw_ sizes its sprites by it).
pub fn hud(on: bool) {
    if on {
        if REDRAW.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1)).is_ok() {
            mem().wr8(DRAW_ALL, 1);
        }
        *SEEN.lock().unwrap() = Some(Instant::now());
        HUD.with(|h| h.borrow_mut().clear());
        scale(1.0);
    } else {
        apply();
    }
    IN_HUD.with(|h| h.set(on));
}

/// disp_AllocPoly_(eax ordering table set, dl type) -> eax primitive.
pub fn alloc_poly(c: &mut Cpu) {
    let ret = mem().rd32(c.r[crate::rt::ESP]);
    call_original(c, ALLOC_POLY);
    let p = c.r[EAX];
    super::lights::made(p, ret);
    if p != 0 && (IN_HUD.with(|h| h.get()) || !WINDOW_3D.load(Ordering::Relaxed)) {
        HUD.with(|h| h.borrow_mut().insert(p));
    }
}

/// gl_RenderPoly_(eax primitive): the HUD's in the frame's 4:3 middle, the rest over all of it.
pub fn render_poly(c: &mut Cpu) {
    let hud = HUD.with(|h| h.borrow().contains(&c.r[EAX]));
    super::lights::drawing(c.r[EAX]);
    glide::set_3d(!hud);
    glide::set_view(if hud { glide::View::Hud(0.5) } else { glide::View::Full });
    call_original(c, RENDER_POLY);
}

/// The mission's ViewPort_Init_ arguments (zoom, corners), the widening its tile count was
/// made for, and the widening in its tables now.
struct Port {
    zoom: u32,
    corners: u32,
    most: f32,
    now: f32,
}

static PORT: Mutex<Option<Port>> = Mutex::new(None);

/// A ViewPort_Init_ made by `logic_frame` (the hook runs it as it is).
static PLAIN: AtomicBool = AtomicBool::new(false);

/// The terrain's widening for an aspect.
fn widening(a: f32) -> f32 {
    a / NARROW
}

/// ViewPort_Init_(eax zoom, edx corners: 4 x {x, y} i16 in tiles, 0: from the zoom) -> the
/// number of tiles in the trapezoid (TileMgr_Init_ sizes its lists by it).
pub fn viewport_init(c: &mut Cpu) {
    let (zoom, corners) = (c.r[EAX], c.r[EDX]);
    if corners == 0 || PLAIN.load(Ordering::Relaxed) {
        return call_original(c, VIEWPORT_INIT);
    }
    let a = super::window::window_size();
    let now = widening(wanted(a.0 as f32 / a.1.max(1) as f32));
    let most = now.max(widening(16.0 / 9.0));
    // the count for the widest, the tables for now
    let (ebx, ecx) = (c.r[EBX], c.r[ECX]);
    let mut count = 0;
    widened(corners, most, || count = pictures::call(c, VIEWPORT_INIT, [zoom, corners, ebx, ecx]));
    widened(corners, now, || call_original(c, VIEWPORT_INIT));
    c.r[EAX] = count;
    *PORT.lock().unwrap() = Some(Port { zoom, corners, most, now });
}

/// Run `f` with the corners' x widened by `w` (put back afterwards), ViewPort_Init_ plain.
fn widened(corners: u32, w: f32, f: impl FnOnce()) {
    let m = mem();
    let old: Vec<u16> = (0..4).map(|i| m.rd16(corners + 4 * i)).collect();
    for (i, &x) in old.iter().enumerate() {
        m.wr16(corners + 4 * i as u32, (x as i16 as f32 * w).round() as i16 as u16);
    }
    PLAIN.store(true, Ordering::Relaxed);
    f();
    PLAIN.store(false, Ordering::Relaxed);
    for (i, &x) in old.iter().enumerate() {
        m.wr16(corners + 4 * i as u32, x);
    }
}

/// Before a logic frame: the trapezoid follows the aspect (up to the widest it was sized for).
pub fn logic_frame(c: &mut Cpu) {
    apply();
    let mut p = PORT.lock().unwrap();
    let Some(port) = p.as_mut() else { return };
    let w = widening(NARROW / k()).min(port.most);
    if (w - port.now).abs() < 0.001 {
        return;
    }
    port.now = w;
    let (zoom, corners) = (port.zoom, port.corners);
    drop(p);
    let (ebx, ecx) = (c.r[EBX], c.r[ECX]);
    widened(corners, w, || {
        pictures::call(c, VIEWPORT_INIT, [zoom, corners, ebx, ecx]);
    });
}

/// The rows buffer for 0x4672bc and 0x4676d4 (see the module's comment): 0x10 bytes of header,
/// then 4 bytes a row.
fn rows_buffer() -> u32 {
    static B: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *B.get_or_init(|| super::alloc(0x10 + 4 * 2048))
}

/// 0x4672bc(eax rows buffer, edx corners, ...): the trapezoid's rows, into our buffer.
pub fn trapezoid_rows(c: &mut Cpu) {
    c.r[EAX] = rows_buffer();
    call_original(c, TRAPEZOID_ROWS);
}

/// 0x4676d4(eax rows buffer, edx ...): the tiles from those rows.
pub fn trapezoid_tiles(c: &mut Cpu) {
    c.r[EAX] = rows_buffer();
    call_original(c, TRAPEZOID_TILES);
}

