//! The game window (winit) and what is shown in it (OpenGL, `gl.rs`). Everything here lives
//! on the game's main thread: the game creates its window, pumps its messages and presents its
//! frames there.

use std::cell::RefCell;
use std::collections::VecDeque;
use super::gl::Gl;
use super::log;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::Duration;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::platform::pump_events::EventLoopExtPumpEvents;
use winit::window::{CursorGrabMode, Window, WindowId};

/// Input for the game, in game terms: Windows virtual-key codes and frame pixels.
#[derive(Clone, Copy, Debug)]
pub enum Input {
    Key { vk: u8, scan: u8, down: bool, repeat: bool },
    Char(u8),
    Move(i32, i32),
    /// 0 left, 1 right, 2 middle, 3 back/X1, 4 forward/X2
    Button(u8, bool),
    /// mouse wheel: lines, positive away from the user
    Wheel(i32),
    Close,
    Focus(bool),
    /// scripted only: a dword written to guest memory (address, value)
    Poke(u32, u32),
}

struct App {
    want: Option<String>,
    window: Option<Window>,
    gl: Option<Gl>,
    input: VecDeque<Input>,
    /// a frame shown before the window exists
    early: Option<(Vec<u32>, u32, u32)>,
    /// the overlay changed since the last screenshot
    overlay_changed: bool,
    cursor: PhysicalPosition<f64>,
    /// an Alt key is held (Alt+Enter toggles fullscreen)
    alt: bool,
    shift: bool,
    focused: bool,
    /// the cursor as last set on the window: hidden and kept inside it
    cursor_caught: bool,
    /// the cursor as last set on the window: hidden
    cursor_hidden: bool,
    /// our menu (F10), while open
    menu: Option<super::menu::Menu>,
}

struct Host {
    event_loop: EventLoop<()>,
    app: App,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// The thread that owns the window (the game's main thread), and a frame another thread
/// presented, shown at the main thread's next pump.
static MAIN: OnceLock<ThreadId> = OnceLock::new();
static PENDING: Mutex<Option<(Vec<u32>, u32, u32)>> = Mutex::new(None);

/// A video frame to show (0RGB pixels, w x h) or None, shown at the main thread's next pump.
type VideoFrame = Option<(Vec<u32>, u32, u32)>;
static VIDEO: Mutex<Option<VideoFrame>> = Mutex::new(None);

/// The game hides the cursor (ShowCursor); applied at the next pump.
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Hide the cursor and keep it inside the window while the window has the focus (missions: the
/// mouse's buttons can be bound, and a cursor outside the window would lose them), or show it
/// again. Any thread.
pub fn hide_cursor(hidden: bool) {
    CURSOR_HIDDEN.store(hidden, Ordering::Relaxed);
}

fn on_main() -> bool {
    thread_local! {
        // asked at every Glide call: thread::current() each time cost
        static IS: bool = *MAIN.get_or_init(|| std::thread::current().id()) == std::thread::current().id();
    }
    IS.with(|m| *m)
}

fn with_host<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    HOST.with(|h| {
        let mut h = h.borrow_mut();
        let host = h.get_or_insert_with(|| {
            #[allow(unused_mut)]
            let mut b = EventLoop::builder();
            #[cfg(windows)]
            winit::platform::windows::EventLoopBuilderExtWindows::with_any_thread(&mut b, true);
            #[cfg(all(unix, not(target_os = "android"), not(target_vendor = "apple")))]
            winit::platform::x11::EventLoopBuilderExtX11::with_any_thread(&mut b, true);
            Host {
                event_loop: b.build().expect("cannot open the window system"),
                app: App {
                    want: None,
                    window: None,
                    gl: None,
                    input: VecDeque::new(),
                    early: None,
                    overlay_changed: false,
                    cursor: PhysicalPosition::new(0.0, 0.0),
                    alt: false,
                    shift: false,
                    focused: true,
                    cursor_caught: false,
                    cursor_hidden: false,
                    menu: None,
                },
            }
        });
        f(host)
    })
}

/// Open the window (shown at the next pump).
pub fn open(title: &str) {
    if !on_main() {
        return;
    }
    with_host(|h| {
        if h.app.window.is_none() {
            h.app.want = Some(title.to_string());
        }
    });
    pump(Duration::ZERO);
}

/// Handle the window system's events, waiting up to `wait` for one.
pub fn pump(wait: Duration) {
    if !on_main() {
        return;
    }
    with_host(|h| {
        if let Some((px, w, hh)) = PENDING.lock().unwrap().take() {
            h.app.frame(&px, w, hh);
        }
        // a context for another thread
        if std::mem::take(&mut *SHARE_WANTED.lock().unwrap()) {
            if let Some(gl) = &h.app.gl {
                *SHARE_GIVEN.lock().unwrap() = Some(gl.share());
                SHARE_CV.notify_all();
            } else {
                *SHARE_WANTED.lock().unwrap() = true;
            }
        }
        // a picture of another thread: to show (it waits for its fence), or shown already
        let pending = SHOW.lock().unwrap().as_ref().map(|s| s.4.is_some());
        match pending {
            Some(true) => h.app.draw(),
            Some(false) => h.app.take_show(),
            None => {}
        }
        if let Some(v) = VIDEO.lock().unwrap().take() {
            if let Some(gl) = &mut h.app.gl {
                gl.video(v.as_ref().map(|(px, w, hh)| (&px[..], *w, *hh)));
                h.app.draw();
            }
        }
        let _ = h.event_loop.pump_app_events(Some(wait), &mut h.app);
        h.app.catch_cursor();
        scripted(&mut h.app);
        h.app.catch_cursor();
    });
}

/// NSTRIKE_INPUT="9000:click@484,303 12000:Enter 15000:Up*3000": input at fixed times (ms
/// after start) for tests. `click@x,y` / `rclick@x,y` / `mclick` / `x1click` / `x2click` (frame
/// pixels, after a move there; held 100 ms or `*ms`), `move@x,y`, `wheel@n` (lines), a key held
/// 100 ms or `*ms` (Enter, Escape, Space, Tab, Up, Down, Left, Right, Ctrl, Alt, Shift,
/// Backspace, Delete, A-Z, 0-9, Numpad0-9, F1-F12, or winit's name: Minus, ShiftRight...),
/// `close` (the window's close button), `blur` / `focus` (the window loses or gets the focus),
/// `poke@511fc0=1` (a dword written to guest memory, hex address and value), `text@Name` (typed
/// characters, `_` a space), `char@0d` (one, hex). `NSTRIKE_INPUT=@file.txt` reads the items from
/// a file; `#` starts a comment to the end of the line. A move also moves DirectInput's mouse
/// by the way from the move before (from the frame's centre for the first).
fn scripted(app: &mut App) {
    static SCRIPT: Mutex<Option<VecDeque<(u64, Input)>>> = Mutex::new(None);
    static LAST: Mutex<Option<(i32, i32)>> = Mutex::new(None);
    let mut s = SCRIPT.lock().unwrap();
    let script = s.get_or_insert_with(|| parse_script(script_text()));
    let now = super::kernel32::start_time().elapsed().as_millis() as u64;
    while script.front().is_some_and(|e| e.0 <= now) {
        match script.pop_front().unwrap().1 {
            Input::Poke(a, v) => super::mem().wr32(a, v),
            Input::Move(x, y) => {
                let (lx, ly) = LAST.lock().unwrap().replace((x, y)).unwrap_or((320, 240));
                if !super::menu::is_open() {
                    super::dinput::motion(x - lx, y - ly);
                }
                app.push(Input::Move(x, y));
            }
            i => app.push(i),
        }
    }
}

/// A test run with scripted input (NSTRIKE_INPUT): the window does not take the foreground,
/// the cursor is not hidden or caught, and the real mouse does not move DirectInput's.
fn scripted_run() -> bool {
    static S: OnceLock<bool> = OnceLock::new();
    *S.get_or_init(|| std::env::var_os("NSTRIKE_INPUT").is_some())
}

/// The scripted input's items (NSTRIKE_INPUT, or the file it names with `@`), comments removed.
pub fn script_text() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        let Ok(v) = std::env::var("NSTRIKE_INPUT") else { return String::new() };
        let text = match v.strip_prefix('@') {
            Some(path) => std::fs::read_to_string(path).unwrap_or_else(|e| {
                log!("NSTRIKE_INPUT: cannot read {path}: {e}");
                String::new()
            }),
            None => v,
        };
        text.lines().map(|l| l.split('#').next().unwrap_or("")).collect::<Vec<_>>().join("\n")
    })
}

fn parse_script(text: &str) -> VecDeque<(u64, Input)> {
    let mut ev = Vec::new();
    for item in text.split_whitespace() {
        let Some((t, what)) = item.split_once(':') else { continue };
        let Ok(t) = t.parse::<u64>() else { continue };
        let at = |s: &str| -> Option<(i32, i32)> {
            let (x, y) = s.split_once(',')?;
            Some((x.parse().ok()?, y.parse().ok()?))
        };
        if let Some(text) = what.strip_prefix("text@") {
            // typed characters (WM_CHAR), 50 ms apart; `_` is a space
            for (n, ch) in text.chars().filter(|&c| (c as u32) < 256).enumerate() {
                ev.push((t + 50 * n as u64, Input::Char(if ch == '_' { b' ' } else { ch as u8 })));
            }
        } else if let Some(ch) = what.strip_prefix("char@").and_then(|h| u8::from_str_radix(h, 16).ok()) {
            ev.push((t, Input::Char(ch)));
        } else if let Some(n) = what.strip_prefix("wheel@").and_then(|n| n.parse().ok()) {
            ev.push((t, Input::Wheel(n)));
        } else if let Some(p) = what.strip_prefix("poke@") {
            let hex = |s: &str| u32::from_str_radix(s, 16).ok();
            if let Some((a, v)) = p.split_once('=').and_then(|(a, v)| Some((hex(a)?, hex(v)?))) {
                ev.push((t, Input::Poke(a, v)));
            }
        } else if let Some((b, xy)) = what.split_once('@') {
            let (xy, hold) = xy.split_once('*').map_or((xy, 100), |(p, h)| (p, h.parse().unwrap_or(100)));
            let Some((x, y)) = at(xy) else { continue };
            ev.push((t, Input::Move(x, y)));
            let b = match b {
                "click" => 0,
                "rclick" => 1,
                "mclick" => 2,
                "x1click" => 3,
                "x2click" => 4,
                _ => continue,
            };
            ev.push((t + 30, Input::Button(b, true)));
            ev.push((t + 30 + hold, Input::Button(b, false)));
        } else if what == "close" {
            ev.push((t, Input::Close));
        } else if what == "blur" || what == "focus" {
            ev.push((t, Input::Focus(what == "focus")));
        } else {
            let (name, hold) = what.split_once('*').map_or((what, 100), |(n, h)| (n, h.parse().unwrap_or(100)));
            let Some((vk, scan)) = key_by_name(name).and_then(vk_of) else {
                eprintln!("NSTRIKE_INPUT: unknown key {name}");
                continue;
            };
            ev.push((t, Input::Key { vk, scan, down: true, repeat: false }));
            ev.push((t + hold, Input::Key { vk, scan, down: false, repeat: false }));
        }
    }
    ev.sort_by_key(|e| e.0);
    ev.into()
}

const FN: [KeyCode; 12] = [
    KeyCode::F1, KeyCode::F2, KeyCode::F3, KeyCode::F4, KeyCode::F5, KeyCode::F6, KeyCode::F7, KeyCode::F8, KeyCode::F9, KeyCode::F10,
    KeyCode::F11, KeyCode::F12,
];

fn key_by_name(n: &str) -> Option<KeyCode> {
    use KeyCode::*;
    const LETTERS: [KeyCode; 26] = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU,
        KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    const DIGITS: [KeyCode; 10] = [Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9];
    const PAD: [KeyCode; 10] = [Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7, Numpad8, Numpad9];
    let b = n.as_bytes();
    if b.len() == 1 && b[0].is_ascii_alphabetic() {
        return Some(LETTERS[(b[0].to_ascii_uppercase() - b'A') as usize]);
    }
    if b.len() == 1 && b[0].is_ascii_digit() {
        return Some(DIGITS[(b[0] - b'0') as usize]);
    }
    if let Some(d) = n.strip_prefix("Numpad").and_then(|d| d.parse::<usize>().ok()) {
        return PAD.get(d).copied();
    }
    if let Some(d) = n.strip_prefix('F').and_then(|d| d.parse::<usize>().ok()) {
        return FN.get(d.wrapping_sub(1)).copied();
    }
    match n {
        "Return" => return Some(Enter),
        "Esc" => return Some(Escape),
        _ => {}
    }
    SHORT.iter().find(|s| s.0 == n).map(|s| s.1).or_else(|| OTHER.into_iter().find(|k| format!("{k:?}") == n))
}

/// Keys with a short name in NSTRIKE_INPUT besides letters, digits, Numpad<n> and F<n>.
const SHORT: [(&str, KeyCode); 13] = [
    ("Enter", KeyCode::Enter),
    ("Escape", KeyCode::Escape),
    ("Space", KeyCode::Space),
    ("Tab", KeyCode::Tab),
    ("Up", KeyCode::ArrowUp),
    ("Down", KeyCode::ArrowDown),
    ("Left", KeyCode::ArrowLeft),
    ("Right", KeyCode::ArrowRight),
    ("Ctrl", KeyCode::ControlLeft),
    ("Alt", KeyCode::AltLeft),
    ("Shift", KeyCode::ShiftLeft),
    ("Backspace", KeyCode::Backspace),
    ("Delete", KeyCode::Delete),
];

/// The other keys the game gets (`vk_of`), by winit's name.
const OTHER: [KeyCode; 26] = {
    use KeyCode::*;
    [
        NumpadEnter, ShiftRight, ControlRight, AltRight, Insert, Home, End, PageUp, PageDown, NumpadAdd, NumpadSubtract,
        NumpadMultiply, NumpadDivide, NumpadDecimal, Minus, Equal, Comma, Period, Slash, Semicolon, Quote, BracketLeft,
        BracketRight, Backslash, Backquote, Pause,
    ]
};

/// The NSTRIKE_INPUT name of the key with this virtual-key and scan code (`record.rs`).
pub fn key_name(vk: u8, scan: u8) -> Option<String> {
    let mut names: Vec<String> = SHORT.iter().map(|s| s.0.to_string()).collect();
    names.extend((b'A'..=b'Z').chain(b'0'..=b'9').map(|c| (c as char).to_string()));
    names.extend((0..10).map(|d| format!("Numpad{d}")));
    names.extend((1..=12).map(|d| format!("F{d}")));
    names.extend(OTHER.iter().map(|k| format!("{k:?}")));
    names.into_iter().find(|n| key_by_name(n).and_then(vk_of) == Some((vk, scan)))
}

/// The next input event, if any (call `pump` first).
pub fn next_input() -> Option<Input> {
    if !on_main() {
        return None;
    }
    with_host(|h| h.app.input.pop_front())
}

/// Show a frame of 0RGB pixels, scaled to the window with its aspect kept.
pub fn present(pixels: &[u32], w: u32, h: u32) {
    if !on_main() {
        *PENDING.lock().unwrap() = Some((pixels.to_vec(), w, h));
        return;
    }
    with_host(|host| host.app.frame(pixels, w, h));
}

thread_local! {
    /// another thread's context sharing the window's (the game renders on its own thread)
    static SHARED: RefCell<Option<super::gl::Shared>> = const { RefCell::new(None) };
}

/// A thread waits for a shared context (true), which the window's thread hands over.
static SHARE_WANTED: Mutex<bool> = Mutex::new(false);
static SHARE_GIVEN: Mutex<Option<super::gl::SharedParts>> = Mutex::new(None);
static SHARE_CV: std::sync::Condvar = std::sync::Condvar::new();
/// A texture another thread rendered, to show at the window thread's next pump.
static SHOW: Mutex<Option<(glow::Texture, u32, u32, f32, Option<Fence>)>> = Mutex::new(None);

/// A GL fence: the rendering of a picture another thread hands over. The window's context
/// waits for it on the GPU, so the thread that rendered does not wait at all.
struct Fence(glow::Fence);
// SAFETY: a sync object belongs to the share group, not to a thread
unsafe impl Send for Fence {}

/// Run `f` with an OpenGL context: the window's on its thread, else a context sharing its
/// objects (made at the first use, by the window's thread at its next pump). None before the
/// window exists.
pub fn with_gl<R>(f: impl FnOnce(&glow::Context) -> R) -> Option<R> {
    if on_main() {
        return with_host(|h| h.app.gl.as_ref().map(|g| f(&g.gl)));
    }
    SHARED.with_borrow_mut(|sh| {
        if sh.is_none() {
            *SHARE_WANTED.lock().unwrap() = true;
            let mut given = SHARE_GIVEN.lock().unwrap();
            while given.is_none() {
                given = SHARE_CV.wait(given).unwrap();
            }
            *sh = given.take().map(super::gl::SharedParts::make_current);
        }
        sh.as_ref().map(|s| f(&s.gl))
    })
}

/// Does this thread have its GL context already (the window's, or one sharing it)?
pub fn gl_ready_here() -> bool {
    on_main() || SHARED.with_borrow(|s| s.is_some())
}

/// What the window showed last (its thread's last drawing, or a picture another thread
/// showed since): another thread shows its pictures with it.
static LOOK: Mutex<Option<super::gl::Look>> = Mutex::new(None);

/// Show a texture something rendered (first row at the bottom), w x h, with a gamma. From
/// another thread: shown at once by that thread (with the window's overlay and video), and
/// the window's thread takes it as the picture shown at its next pump. Before the window has
/// drawn once: shown at its next pump, after the rendering (a fence).
pub fn show_texture(tex: glow::Texture, w: u32, h: u32, gamma: f32) {
    if !on_main() {
        let look = *LOOK.lock().unwrap();
        let shown = look.and_then(|mut l| {
            l.shown = (tex, w, h, false, gamma);
            SHARED.with_borrow(|sh| {
                let s = sh.as_ref()?;
                if let Some((px, pw, ph)) = s.present(&l, shot_due(false)) {
                    write_shot(&px, pw, ph);
                }
                Some(l)
            })
        });
        if let Some(l) = shown {
            *LOOK.lock().unwrap() = Some(l);
            let old = SHOW.lock().unwrap().replace((tex, w, h, gamma, None));
            if let Some((.., Some(Fence(f)))) = old {
                SHARED.with_borrow(|sh| sh.as_ref().map(|s| unsafe { glow::HasContext::delete_sync(&s.gl, f) }));
            }
            return;
        }
        let fence = SHARED.with_borrow(|sh| {
            let s = sh.as_ref()?;
            unsafe {
                let f = glow::HasContext::fence_sync(&s.gl, glow::SYNC_GPU_COMMANDS_COMPLETE, 0).ok();
                glow::HasContext::flush(&s.gl);
                f
            }
        });
        let old = SHOW.lock().unwrap().replace((tex, w, h, gamma, fence.map(Fence)));
        // not shown: its fence is not needed any more
        if let Some((.., Some(Fence(f)))) = old {
            SHARED.with_borrow(|sh| sh.as_ref().map(|s| unsafe { glow::HasContext::delete_sync(&s.gl, f) }));
        }
        return;
    }
    with_host(|host| {
        if let Some(gl) = &mut host.app.gl {
            gl.shown = (tex, w, h, false, gamma);
            host.app.draw();
        }
    });
}

/// NSTRIKE_SHOT=file.png: the picture shown is written there (as the window shows it), at most
/// every half second (NSTRIKE_SHOT_MS: other milliseconds), and at every change of the overlay (for checks without looking). A
/// `{t}` in the name is replaced by the time since start in ms (as in NSTRIKE_INPUT), so every
/// picture is kept.
fn shot_path() -> Option<&'static String> {
    static PATH: OnceLock<Option<String>> = OnceLock::new();
    PATH.get_or_init(|| std::env::var("NSTRIKE_SHOT").ok()).as_ref()
}

static LAST_SHOT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn shot_due(force: bool) -> bool {
    static MS: OnceLock<u64> = OnceLock::new();
    let ms = *MS.get_or_init(|| std::env::var("NSTRIKE_SHOT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(500));
    shot_path().is_some() && (force || LAST_SHOT.lock().unwrap().is_none_or(|t| t.elapsed() >= Duration::from_millis(ms)))
}

fn write_shot(px: &[u32], w: u32, h: u32) {
    *LAST_SHOT.lock().unwrap() = Some(std::time::Instant::now());
    if let Some(path) = shot_path() {
        let t = super::kernel32::start_time().elapsed().as_millis();
        let _ = std::fs::write(path.replace("{t}", &format!("{t:06}")), png(px, w, h));
    }
}

/// An uncompressed PNG of 0RGB pixels.
fn png(pixels: &[u32], w: u32, h: u32) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend(kind);
        out.extend(data);
        let c = crc(&out[start..]);
        out.extend(c.to_be_bytes());
    }
    let mut raw = Vec::with_capacity(((w * 3 + 1) * h) as usize);
    for row in pixels.chunks(w as usize).take(h as usize) {
        raw.push(0);
        for p in row {
            raw.extend([(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
        }
    }
    // zlib stream of stored deflate blocks
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(0xffff).collect();
    for (k, b) in blocks.iter().enumerate() {
        z.push((k + 1 == blocks.len()) as u8);
        z.extend((b.len() as u16).to_le_bytes());
        z.extend((!(b.len() as u16)).to_le_bytes());
        z.extend(*b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend((b << 16 | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 13, 10, 0x1a, 10];
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Draw `pixels` (ARGB, w x h) over the game's frames, stretched to the frame's area; `None`
/// removes it.
pub fn set_overlay(overlay: Option<(Vec<u32>, u32, u32)>) {
    if !on_main() {
        return;
    }
    with_host(|h| {
        if let Some(gl) = &mut h.app.gl {
            gl.overlay(overlay.as_ref().map(|(px, w, hh)| (&px[..], *w, *hh)));
            h.app.overlay_changed = true;
            h.app.draw();
        }
    });
}

/// Show a video frame (0RGB, w x h) over the game's frames, as large as fits the window and
/// centred; `None` removes it. Any thread; shown at the next pump.
pub fn set_video(frame: VideoFrame) {
    *VIDEO.lock().unwrap() = Some(frame);
}

/// Size of the last frame shown (input positions are in its pixels).
pub fn frame_size() -> (u32, u32) {
    if !on_main() {
        return (640, 480);
    }
    with_host(|h| h.app.gl.as_ref().map_or((640, 480), |g| (g.shown.1, g.shown.2)))
}

/// The window's drawable size in pixels (1280x960 before it exists).
pub fn window_size() -> (u32, u32) {
    if !on_main() {
        return (1280, 960);
    }
    with_host(|h| h.app.window.as_ref().map(|w| w.inner_size()).filter(|s| s.width > 0 && s.height > 0).map_or((1280, 960), |s| (s.width, s.height)))
}

/// The display the window is on: its size and the sizes of its video modes (largest first,
/// at least 640x480).
pub fn display_modes() -> ((u32, u32), Vec<(u32, u32)>) {
    let none = ((1280, 960), Vec::new());
    if !on_main() {
        return none;
    }
    with_host(|h| {
        let Some(m) = h.app.window.as_ref().and_then(|w| w.current_monitor()) else { return none };
        let desktop = (m.size().width, m.size().height);
        let mut modes: Vec<(u32, u32)> = m.video_modes().map(|v| (v.size().width, v.size().height)).filter(|&(w, h)| w >= 640 && h >= 480).collect();
        modes.sort_unstable_by(|a, b| b.cmp(a));
        modes.dedup();
        (desktop, modes)
    })
}

/// The refresh rate of the window's display in mHz (0: not known yet).
static REFRESH: AtomicU32 = AtomicU32::new(0);

pub fn refresh_mhz() -> u32 {
    REFRESH.load(Ordering::Relaxed)
}

fn note_refresh(w: &Window) {
    if let Some(r) = w.current_monitor().and_then(|m| m.refresh_rate_millihertz()) {
        REFRESH.store(r, Ordering::Relaxed);
    }
}

/// Is the window fullscreen?
pub fn is_fullscreen() -> bool {
    on_main() && with_host(|h| h.app.window.as_ref().is_some_and(|w| w.fullscreen().is_some()))
}

/// Borderless fullscreen on the window's display, or a window again.
pub fn set_fullscreen(on: bool) {
    if !on_main() {
        return;
    }
    with_host(|h| {
        if let Some(w) = &h.app.window {
            if w.fullscreen().is_some() != on {
                w.set_fullscreen(on.then_some(winit::window::Fullscreen::Borderless(None)));
            }
        }
    });
}

/// Give the window this inner size (unless it is fullscreen).
pub fn set_window_size(w: u32, h: u32) {
    if !on_main() {
        return;
    }
    with_host(|host| {
        if let Some(win) = host.app.window.as_ref().filter(|w| w.fullscreen().is_none()) {
            let _ = win.request_inner_size(winit::dpi::PhysicalSize::new(w, h));
        }
    });
}

/// The 4:3 part in the middle of a frame of w x h, where dialogs are shown: x offset and width.
pub fn overlay_span(w: u32, h: u32) -> (u32, u32) {
    let ow = w.min(h * 4 / 3);
    ((w - ow) / 2, ow)
}

/// Where a frame of w x h lands in a window of ww x wh: offset and scale.
pub fn fit(w: u32, h: u32, ww: u32, wh: u32) -> (f64, f64, f64) {
    let s = (ww as f64 / w as f64).min(wh as f64 / h as f64);
    ((ww as f64 - w as f64 * s) / 2.0, (wh as f64 - h as f64 * s) / 2.0, s)
}

impl App {
    fn frame(&mut self, px: &[u32], w: u32, h: u32) {
        match &mut self.gl {
            Some(gl) => {
                gl.frame(px, w, h);
                self.draw();
            }
            None => self.early = Some((px.to_vec(), w, h)),
        }
    }

    /// The picture another thread handed over, as the one shown (after its fence).
    fn take_show(&mut self) {
        let Some(gl) = &mut self.gl else { return };
        if let Some((tex, w, hh, gamma, fence)) = SHOW.lock().unwrap().take() {
            if let Some(Fence(f)) = fence {
                unsafe {
                    glow::HasContext::wait_sync(&gl.gl, f, 0, glow::TIMEOUT_IGNORED);
                    glow::HasContext::delete_sync(&gl.gl, f);
                }
            }
            gl.shown = (tex, w, hh, false, gamma);
        }
    }

    fn draw(&mut self) {
        self.take_show();
        let (Some(win), Some(gl)) = (&self.window, &mut self.gl) else { return };
        let size = win.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        *LOOK.lock().unwrap() = Some(gl.look(size.width, size.height));
        let want_shot = shot_due(self.overlay_changed);
        if let Some((px, w, h)) = gl.draw(size.width, size.height, want_shot) {
            write_shot(&px, w, h);
            self.overlay_changed = false;
        }
    }

    /// Input from the window system for the game, or for our menu while it is open (F10 opens
    /// it). Keys and buttons going up, the focus and the close button still reach the game:
    /// nothing stays held.
    fn push(&mut self, i: Input) {
        use super::menu::{Action, Menu, KEY};
        let full = self.window.as_ref().is_some_and(|w| w.fullscreen().is_some());
        let Some(menu) = &mut self.menu else {
            if matches!(i, Input::Key { vk: KEY, down: true, repeat: false, .. }) {
                let display = self.window.as_ref().and_then(|w| w.current_monitor()).map_or((1280, 960), |m| (m.size().width, m.size().height));
                self.menu = Some(Menu::open(display, full));
                self.show_menu();
                self.catch_cursor();
            } else {
                self.input.push_back(i);
            }
            return;
        };
        match i {
            Input::Key { down: false, .. } | Input::Button(_, false) | Input::Focus(_) | Input::Close => self.input.push_back(i),
            _ => {}
        }
        // the mouse in the overlay's pixels (it covers the picture's 4:3 middle)
        let i = match i {
            Input::Move(x, y) => {
                let (fw, fh) = self.gl.as_ref().map_or((640, 480), |g| (g.shown.1, g.shown.2));
                let (sx, sw) = overlay_span(fw, fh);
                Input::Move((x - sx as i32) * super::menu::W / sw.max(1) as i32, y * super::menu::H / fh.max(1) as i32)
            }
            i => i,
        };
        menu.fullscreen = full;
        let mut acts = Vec::new();
        let closed = menu.input(&i, &mut acts).is_err();
        for a in acts {
            let Some(w) = &self.window else { continue };
            match a {
                Action::Fullscreen(on) => {
                    w.set_fullscreen(on.then_some(winit::window::Fullscreen::Borderless(None)));
                    crate::settings::update(|s| s.fullscreen = on);
                }
                Action::Window(_) if w.fullscreen().is_some() => {}
                Action::Window(Some((ww, wh))) => drop(w.request_inner_size(winit::dpi::PhysicalSize::new(ww, wh))),
                Action::Window(None) => drop(w.request_inner_size(LogicalSize::new(1280.0, 960.0))),
            }
        }
        if closed {
            self.menu = None;
            if let Some(gl) = &mut self.gl {
                gl.overlay(None);
            }
            self.overlay_changed = true;
            self.draw();
            self.catch_cursor();
        } else {
            self.show_menu();
        }
    }

    /// Our menu drawn into the overlay.
    fn show_menu(&mut self) {
        let (Some(m), Some(gl)) = (&self.menu, &mut self.gl) else { return };
        gl.overlay(Some((&m.render(), super::menu::W as u32, super::menu::H as u32)));
        self.overlay_changed = true;
        self.draw();
    }

    fn create(&mut self, el: &ActiveEventLoop) {
        let Some(title) = self.want.take() else { return };
        // the settings' window size (NSTRIKE_WINDOW=1920x1080); else 1280x960 at the desktop's scale
        let set = crate::settings::get();
        let attrs = Window::default_attributes().with_title(title);
        let attrs = match set.window {
            Some((w, h)) => attrs.with_inner_size(winit::dpi::PhysicalSize::new(w, h)),
            None => attrs.with_inner_size(LogicalSize::new(1280.0, 960.0)),
        };
        let attrs = attrs.with_fullscreen(set.fullscreen.then_some(winit::window::Fullscreen::Borderless(None)));
        let (win, gl) = Gl::new(el, attrs);
        // to the front, while our start window still has the foreground (Windows lets only the
        // foreground's process bring a window forward)
        if !scripted_run() {
            win.focus_window();
        }
        note_refresh(&win);
        self.window = Some(win);
        self.gl = Some(gl);
        if let Some((px, w, h)) = self.early.take() {
            self.frame(&px, w, h);
        }
    }

    /// Hide the cursor and confine it to the window while the game hides it and the window has
    /// the focus (Windows and X11 confine it; Wayland and macOS only lock it in place).
    fn catch_cursor(&mut self) {
        let Some(win) = &self.window else { return };
        // a scripted test run leaves the mouse to the user
        let catch = self.focused && CURSOR_HIDDEN.load(Ordering::Relaxed) && !scripted_run() && self.menu.is_none();
        let hide = catch;
        if hide != self.cursor_hidden {
            self.cursor_hidden = hide;
            win.set_cursor_visible(!hide);
        }
        if catch == self.cursor_caught {
            return;
        }
        self.cursor_caught = catch;
        let grab = if catch {
            win.set_cursor_grab(CursorGrabMode::Confined).or_else(|_| win.set_cursor_grab(CursorGrabMode::Locked))
        } else {
            win.set_cursor_grab(CursorGrabMode::None)
        };
        if let Err(e) = grab {
            log!("warn: cursor grab: {e}");
        }
    }

    /// The cursor in frame pixels.
    fn frame_pos(&self) -> (i32, i32) {
        let Some(win) = &self.window else { return (0, 0) };
        let size = win.inner_size();
        let (w, h) = self.gl.as_ref().map_or((640, 480), |g| (g.shown.1, g.shown.2));
        let (ox, oy, s) = fit(w, h, size.width.max(1), size.height.max(1));
        let x = ((self.cursor.x - ox) / s).clamp(0.0, w as f64 - 1.0);
        let y = ((self.cursor.y - oy) / s).clamp(0.0, h as f64 - 1.0);
        (x as i32, y as i32)
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        self.create(el);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.create(el);
    }

    fn device_event(&mut self, _: &ActiveEventLoop, _: winit::event::DeviceId, e: winit::event::DeviceEvent) {
        // DirectInput's mouse is relative: the raw motion, while the window has the focus
        if let winit::event::DeviceEvent::MouseMotion { delta } = e {
            if self.focused && !scripted_run() && self.menu.is_none() {
                super::dinput::motion(delta.0.round() as i32, delta.1.round() as i32);
            }
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, e: WindowEvent) {
        match e {
            WindowEvent::CloseRequested => self.push(Input::Close),
            WindowEvent::Focused(f) => {
                self.focused = f;
                // Windows drops a confinement when the focus goes: set it anew when it comes back
                self.cursor_caught = false;
                self.cursor_hidden = false;
                if let Some(w) = self.window.as_ref().filter(|_| !f) {
                    w.set_cursor_visible(true);
                    let _ = w.set_cursor_grab(CursorGrabMode::None);
                }
                self.push(Input::Focus(f));
            }
            WindowEvent::Moved(_) => {
                if let Some(w) = &self.window {
                    note_refresh(w);
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(gl) = &self.gl {
                    gl.resize(size.width, size.height);
                }
                self.draw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                let (x, y) = self.frame_pos();
                self.push(Input::Move(x, y));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    winit::event::MouseScrollDelta::PixelDelta(p) => (p.y / 40.0).round() as i32,
                };
                if lines != 0 {
                    self.push(Input::Wheel(lines));
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Back | MouseButton::Other(4) => 3,
                    MouseButton::Forward | MouseButton::Other(5) => 4,
                    _ => return,
                };
                self.push(Input::Button(b, state == ElementState::Pressed));
            }
            WindowEvent::ModifiersChanged(m) => {
                self.alt = m.state().alt_key();
                self.shift = m.state().shift_key();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let down = event.state == ElementState::Pressed;
                if self.alt && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Enter | KeyCode::NumpadEnter)) {
                    // Alt+Enter: borderless fullscreen on the window's display, or back
                    if down && !event.repeat {
                        if let Some(w) = &self.window {
                            let full = w.fullscreen().is_some();
                            w.set_fullscreen((!full).then_some(winit::window::Fullscreen::Borderless(None)));
                            crate::settings::update(|s| s.fullscreen = !full);
                        }
                    }
                    return;
                }
                if let PhysicalKey::Code(code) = event.physical_key {
                    if let Some((vk, scan)) = vk_of(code) {
                        self.push(Input::Key { vk, scan, down, repeat: event.repeat });
                    }
                }
                if down {
                    if let Some(t) = &event.text {
                        for ch in t.chars().filter(|&c| (c as u32) < 256) {
                            self.push(Input::Char(ch as u8));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Windows virtual-key code and set-1 scan code for a key.
fn vk_of(k: KeyCode) -> Option<(u8, u8)> {
    use KeyCode::*;
    let letters = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU,
        KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    const LETTER_SCAN: [u8; 26] =
        [0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c];
    if let Some(i) = letters.iter().position(|&l| l == k) {
        return Some((b'A' + i as u8, LETTER_SCAN[i]));
    }
    let digits = [Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9];
    if let Some(i) = digits.iter().position(|&d| d == k) {
        return Some((b'0' + i as u8, if i == 0 { 0x0b } else { 1 + i as u8 }));
    }
    let pad = [Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7, Numpad8, Numpad9];
    const PAD_SCAN: [u8; 10] = [0x52, 0x4f, 0x50, 0x51, 0x4b, 0x4c, 0x4d, 0x47, 0x48, 0x49];
    if let Some(i) = pad.iter().position(|&d| d == k) {
        return Some((0x60 + i as u8, PAD_SCAN[i]));
    }
    let f = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
    if let Some(i) = f.iter().position(|&d| d == k) {
        return Some((0x70 + i as u8, if i < 10 { 0x3b + i as u8 } else { 0x57 + (i as u8 - 10) }));
    }
    Some(match k {
        Escape => (0x1b, 0x01),
        Enter => (0x0d, 0x1c),
        NumpadEnter => (0x0d, 0x1c),
        Space => (0x20, 0x39),
        Tab => (0x09, 0x0f),
        Backspace => (0x08, 0x0e),
        ShiftLeft => (0x10, 0x2a),
        ShiftRight => (0x10, 0x36),
        ControlLeft | ControlRight => (0x11, 0x1d),
        AltLeft | AltRight => (0x12, 0x38),
        ArrowLeft => (0x25, 0x4b),
        ArrowUp => (0x26, 0x48),
        ArrowRight => (0x27, 0x4d),
        ArrowDown => (0x28, 0x50),
        Insert => (0x2d, 0x52),
        Delete => (0x2e, 0x53),
        Home => (0x24, 0x47),
        End => (0x23, 0x4f),
        PageUp => (0x21, 0x49),
        PageDown => (0x22, 0x51),
        NumpadAdd => (0x6b, 0x4e),
        NumpadSubtract => (0x6d, 0x4a),
        NumpadMultiply => (0x6a, 0x37),
        NumpadDivide => (0x6f, 0x35),
        NumpadDecimal => (0x6e, 0x53),
        Minus => (0xbd, 0x0c),
        Equal => (0xbb, 0x0d),
        Comma => (0xbc, 0x33),
        Period => (0xbe, 0x34),
        Slash => (0xbf, 0x35),
        Semicolon => (0xba, 0x27),
        Quote => (0xde, 0x28),
        BracketLeft => (0xdb, 0x1a),
        BracketRight => (0xdd, 0x1b),
        Backslash => (0xdc, 0x2b),
        Backquote => (0xc0, 0x29),
        Pause => (0x13, 0x45),
        _ => return None,
    })
}
