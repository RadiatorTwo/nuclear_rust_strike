//! The port's own menu (F10): its settings (`settings.rs`), drawn into the window's overlay
//! over the game. The game's own settings stay in its Config menu.
//!
//! It lives on the window's thread (`window.rs` owns a `Menu` while it is open and hands it
//! the input; the game gets none of it, but for keys and buttons going up). Changes are used
//! at once and saved. While it is open a mission stands still (`frames.rs`: `wait_closed`).

use super::font::{FIRST, GLYPHS};
use super::window::Input;
use crate::settings::{self, FrameRate};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The overlay's size: stretched over the 4:3 middle of the picture.
pub const W: i32 = 640;
pub const H: i32 = 480;

/// The keys that open and close the menu: F10 its settings page, F11 its effects page.
pub const KEY: u8 = 0x79;
pub const KEY_FX: u8 = 0x7a;

static OPEN: AtomicBool = AtomicBool::new(false);

/// Is the menu open? Any thread.
pub fn is_open() -> bool {
    OPEN.load(Ordering::Relaxed)
}

/// The game thread, at the start of a mission frame: wait while the menu is open, drawing
/// pictures (`draw`) so that changes show. Returns how long it waited.
pub fn wait_closed(mut draw: impl FnMut()) -> Duration {
    let start = Instant::now();
    while is_open() {
        draw();
        super::without_turn(|| std::thread::sleep(Duration::from_millis(4)));
    }
    start.elapsed()
}

/// What the window has to do for a change.
pub enum Action {
    Fullscreen(bool),
    /// the window's size (None: the default, 1280x960 at the desktop's scale)
    Window(Option<(u32, u32)>),
}

const RATES: [u32; 6] = [30, 60, 75, 120, 144, 240];
/// window sizes offered (those that fit the display): 4:3, then 16:9 (widescreen)
const SIZES: [(u32, u32); 11] = [
    (960, 720),
    (1280, 960),
    (1440, 1080),
    (1600, 1200),
    (1920, 1440),
    (2880, 2160),
    (1280, 720),
    (1600, 900),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

#[derive(Clone, Copy, PartialEq)]
enum Row {
    Fullscreen,
    Window,
    FrameRate,
    Vsync,
    Msaa,
    Filter,
    Wide,
    Ao,
    Lights,
    Shadows,
    ShadowQuality,
    Bloom,
    Close,
}

/// The settings page (F10) and the effects page (F11: the modern graphics effects).
const SETTINGS: &[Row] = &[Row::Fullscreen, Row::Window, Row::FrameRate, Row::Vsync, Row::Msaa, Row::Filter, Row::Wide, Row::Close];
const EFFECTS: &[Row] = &[Row::Ao, Row::Lights, Row::Shadows, Row::ShadowQuality, Row::Bloom, Row::Close];

pub struct Menu {
    /// the page's rows: `SETTINGS` or `EFFECTS`
    rows: &'static [Row],
    sel: usize,
    /// the row under the mouse
    hot: Option<usize>,
    /// the mouse in overlay pixels (`Input::Move` comes converted)
    mouse: (i32, i32),
    /// the display's size (window sizes larger are not offered)
    display: (u32, u32),
    /// the window is fullscreen (Alt+Enter changes it too)
    pub fullscreen: bool,
}

impl Menu {
    /// Opened by `key` (KEY or KEY_FX: its page).
    pub fn open(key: u8, display: (u32, u32), fullscreen: bool) -> Menu {
        OPEN.store(true, Ordering::Relaxed);
        Menu { rows: page(key), sel: 0, hot: None, mouse: (-1, -1), display, fullscreen }
    }

    /// The window sizes offered (None: the default), with the one in effect.
    fn sizes(&self, cur: Option<(u32, u32)>) -> Vec<Option<(u32, u32)>> {
        let mut v = vec![None];
        v.extend(SIZES.iter().filter(|&&(w, h)| w <= self.display.0 && h <= self.display.1).map(|&z| Some(z)));
        if !v.contains(&cur) {
            v.push(cur);
        }
        v
    }

    /// The choices of a row as (text, value index), and the one in effect.
    fn choices(&self, r: Row) -> (Vec<String>, usize) {
        let s = settings::get();
        match r {
            Row::Fullscreen => (vec!["Off".into(), "On".into()], self.fullscreen as usize),
            Row::Window => {
                let v = self.sizes(s.window);
                let k = v.iter().position(|&z| z == s.window).unwrap();
                (v.iter().map(|z| z.map_or("Auto".into(), |(w, h)| format!("{w}x{h}"))).collect(), k)
            }
            Row::FrameRate => {
                let v = rates(s.frame_rate);
                let k = v.iter().position(|&r| r == s.frame_rate).unwrap();
                (v.iter().map(|&r| rate_text(r)).collect(), k)
            }
            Row::Vsync => (vec!["Off".into(), "On".into()], s.vsync as usize),
            Row::Msaa => {
                let c = settings::MSAA_CHOICES;
                (c.iter().map(|&n| if n == 0 { "Off".into() } else { format!("{n}x") }).collect(), c.iter().position(|&n| n == s.msaa).unwrap_or(0))
            }
            Row::Filter => {
                let c = settings::FILTER_CHOICES;
                let text = |n: u32| match n {
                    0 => "Original".to_string(),
                    1 => "Trilinear".into(),
                    n => format!("Anisotropic {n}x"),
                };
                (c.iter().map(|&n| text(n)).collect(), c.iter().position(|&n| n == s.texture_filter).unwrap_or(0))
            }
            Row::Wide => (vec!["Off".into(), "On".into()], s.widescreen as usize),
            Row::Ao => (vec!["Off".into(), "On".into()], s.ambient_occlusion as usize),
            Row::Lights => (vec!["Off".into(), "On".into()], s.dynamic_lights as usize),
            Row::Shadows => (vec!["Off".into(), "On".into()], s.shadows as usize),
            Row::ShadowQuality => {
                let all = settings::ShadowQuality::ALL;
                (vec!["Low".into(), "Mid".into(), "High".into(), "Ultra".into()], all.iter().position(|&q| q == s.shadow_quality).unwrap_or(1))
            }
            Row::Bloom => (vec!["Off".into(), "On".into()], s.bloom as usize),
            Row::Close => (Vec::new(), 0),
        }
    }

    /// Move a row's choice by `d` (wrapping) and use it.
    fn change(&mut self, r: Row, d: i32) -> Option<Action> {
        let (c, k) = self.choices(r);
        if c.is_empty() {
            return None;
        }
        let k = (k as i32 + d).rem_euclid(c.len() as i32) as usize;
        let s = settings::get();
        match r {
            Row::Fullscreen => {
                self.fullscreen = k == 1;
                return Some(Action::Fullscreen(self.fullscreen));
            }
            Row::Window => {
                let z = self.sizes(s.window)[k];
                settings::update(|s| s.window = z);
                return Some(Action::Window(z));
            }
            Row::FrameRate => super::frames::set_rate(rates(s.frame_rate)[k]),
            Row::Vsync => super::frames::set_vsync(k == 1),
            Row::Msaa => super::glide::set_msaa(settings::MSAA_CHOICES[k]),
            Row::Filter => super::glide::set_filtering(settings::FILTER_CHOICES[k]),
            Row::Wide => settings::update(|s| s.widescreen = k == 1),
            Row::Ao => super::ao::set_enabled(k == 1),
            Row::Lights => super::lights::set_enabled(k == 1),
            Row::Shadows => super::shadows::set_enabled(k == 1),
            Row::ShadowQuality => super::shadows::set_quality(settings::ShadowQuality::ALL[k]),
            Row::Bloom => super::bloom::set_enabled(k == 1),
            Row::Close => {}
        }
        None
    }

    /// Input while open (a move in overlay pixels): `Err(())` closes the menu. The actions are
    /// the window's to do.
    pub fn input(&mut self, i: &Input, out: &mut Vec<Action>) -> Result<(), ()> {
        match *i {
            // its page's key closes it, the other's turns to that page
            Input::Key { vk: k @ (KEY | KEY_FX), down: true, .. } if page(k) != self.rows => {
                self.rows = page(k);
                self.sel = 0;
                self.hot = None;
            }
            Input::Key { vk, down: true, .. } => match vk {
                KEY | KEY_FX | 0x1b => return self.close(),
                0x26 => self.sel = (self.sel + self.rows.len() - 1) % self.rows.len(),
                0x28 | 0x09 => self.sel = (self.sel + 1) % self.rows.len(),
                0x25 => out.extend(self.change(self.rows[self.sel], -1)),
                0x27 => out.extend(self.change(self.rows[self.sel], 1)),
                0x0d | 0x20 => {
                    if self.rows[self.sel] == Row::Close {
                        return self.close();
                    }
                    out.extend(self.change(self.rows[self.sel], 1));
                }
                _ => {}
            },
            Input::Move(x, y) => {
                self.mouse = (x, y);
                self.hot = row_at(self.rows.len(), x, y).map(|(k, _)| k);
            }
            // a click on a row's value: on its left half back, else on
            Input::Button(0, true) => {
                if let Some((k, left)) = row_at(self.rows.len(), self.mouse.0, self.mouse.1) {
                    self.sel = k;
                    if self.rows[k] == Row::Close {
                        return self.close();
                    }
                    out.extend(self.change(self.rows[k], if left { -1 } else { 1 }));
                }
            }
            Input::Wheel(n) => out.extend(self.change(self.rows[self.sel], n.signum())),
            _ => {}
        }
        Ok(())
    }


    fn close(&mut self) -> Result<(), ()> {
        OPEN.store(false, Ordering::Relaxed);
        Err(())
    }

    /// The menu as ARGB pixels, W x H.
    pub fn render(&self) -> Vec<u32> {
        let mut cv = Canvas { px: vec![DIM; (W * H) as usize] };
        let n = self.rows.len();
        let p = panel(n);
        cv.fill(p, PANEL);
        cv.frame(p, EDGE_HI);
        cv.frame(inset(p, 1), EDGE);
        let bar = [p[0] + 2, p[1] + 2, p[2] - 2, p[1] + 20];
        cv.fill(bar, CAPTION);
        let title = if self.rows == EFFECTS { "NSTRIKE EFFECTS" } else { "NSTRIKE SETTINGS" };
        cv.label(bar, title, TEXT, 1, 2);
        for (k, &r) in self.rows.iter().enumerate() {
            let rr = row_rect(n, k);
            let sel = k == self.sel;
            if sel || self.hot == Some(k) {
                cv.fill(rr, if sel { FACE_HOT } else { FACE });
            }
            if sel {
                cv.frame(rr, EDGE_HI);
            }
            if r == Row::Close {
                cv.label(rr, "Close", TEXT, 1, 2);
                continue;
            }
            let name = match r {
                Row::Fullscreen => "Fullscreen",
                Row::Window => "Window size",
                Row::FrameRate => "Frame rate",
                Row::Vsync => "Vsync",
                Row::Msaa => "Anti-aliasing",
                Row::Filter => "Textures",
                Row::Wide => "Widescreen",
                Row::Ao => "Ambient occlusion",
                Row::Lights => "Dynamic lights",
                Row::Shadows => "Shadows",
                Row::ShadowQuality => "Shadow quality",
                Row::Bloom => "Bloom",
                Row::Close => unreachable!(),
            };
            cv.label([rr[0] + 12, rr[1], rr[2], rr[3]], name, TEXT, 0, 2);
            let (c, i) = self.choices(r);
            let v = value_rect(rr);
            cv.label(v, "<", MARK, 0, 2);
            cv.label(v, ">", MARK, 2, 2);
            cv.label(v, &c[i], TEXT, 1, 2);
        }
        let note = match self.rows[self.sel] {
            Row::Window => "Used when not fullscreen",
            Row::FrameRate => "Pictures a second; missions run 60 a second",
            Row::Vsync => "Picture changes at the display's refresh: no tearing",
            Row::Filter => "Filtering of textures the game filters",
            Row::Wide => "Missions as wide as the window, HUD in the middle",
            Row::Ao => "Corners and the ground under things darker",
            Row::Lights => "Muzzle flashes and explosions light what is near",
            Row::Shadows => "The sun casts shadows from what is on screen",
            Row::Bloom => "A soft glow around fire and explosions",
            Row::ShadowQuality => match settings::get().shadow_quality {
                settings::ShadowQuality::Low => "Fastest; things at the screen's edge may lose theirs",
                settings::ShadowQuality::Mid => "Everything around casts; soft edges",
                settings::ShadowQuality::High => "Sharper; edges a little soft",
                settings::ShadowQuality::Ultra => "Sharp edges; most work for the graphics card",
            },
            _ => "",
        };
        cv.label([p[0], p[3] - 46, p[2], p[3] - 30], note, TEXT_OFF, 1, 1);
        cv.label([p[0], p[3] - 26, p[2], p[3] - 8], "Arrows: choose and change   F10 settings   F11 effects   Esc: close", TEXT_OFF, 1, 1);
        cv.px
    }
}

/// The frame rates offered, with the one in effect.
fn rates(cur: FrameRate) -> Vec<FrameRate> {
    let mut v = vec![FrameRate::Display];
    v.extend(RATES.iter().map(|&n| FrameRate::Fps(n)));
    v.push(FrameRate::Unlimited);
    if !v.contains(&cur) {
        v.insert(v.len() - 1, cur);
    }
    v
}

fn rate_text(r: FrameRate) -> String {
    match r {
        FrameRate::Display => "Display".into(),
        FrameRate::Unlimited => "Unlimited".into(),
        FrameRate::Fps(n) => n.to_string(),
    }
}

// Layout (overlay pixels).
const ROW_H: i32 = 30;
const PANEL_W: i32 = 520;

/// The page a key opens.
fn page(key: u8) -> &'static [Row] {
    if key == KEY_FX { EFFECTS } else { SETTINGS }
}

/// The panel for `n` rows.
fn panel(n: usize) -> [i32; 4] {
    let h = 30 + n as i32 * (ROW_H + 4) + 60;
    let (x, y) = ((W - PANEL_W) / 2, (H - h) / 2);
    [x, y, x + PANEL_W, y + h]
}

fn row_rect(n: usize, k: usize) -> [i32; 4] {
    let p = panel(n);
    let y = p[1] + 30 + k as i32 * (ROW_H + 4);
    [p[0] + 10, y, p[2] - 10, y + ROW_H]
}

fn value_rect(r: [i32; 4]) -> [i32; 4] {
    [r[0] + 260, r[1], r[2] - 10, r[3]]
}

/// The row at overlay pixel (x, y), and whether it is on the left half of its value.
fn row_at(n: usize, x: i32, y: i32) -> Option<(usize, bool)> {
    (0..n).find_map(|k| {
        let r = row_rect(n, k);
        let v = value_rect(r);
        (x >= r[0] && x < r[2] && y >= r[1] && y < r[3]).then_some((k, x < (v[0] + v[2]) / 2))
    })
}

const DIM: u32 = 0x8000_0000;
const PANEL: u32 = 0xf00a_1440;
const CAPTION: u32 = 0xff1c_3c9c;
const EDGE: u32 = 0xff3c_64d2;
const EDGE_HI: u32 = 0xff6a_9cff;
const FACE: u32 = 0xff0c_1e5a;
const FACE_HOT: u32 = 0xff1a_3a9a;
const TEXT: u32 = 0xffe8_f0ff;
const TEXT_OFF: u32 = 0xff8a_9aca;
const MARK: u32 = 0xff7f_d0ff;

fn inset(r: [i32; 4], d: i32) -> [i32; 4] {
    [r[0] + d, r[1] + d, r[2] - d, r[3] - d]
}

struct Canvas {
    px: Vec<u32>,
}

impl Canvas {
    fn put(&mut self, x: i32, y: i32, c: u32) {
        if x >= 0 && x < W && y >= 0 && y < H {
            let p = &mut self.px[(y * W + x) as usize];
            *p = over(*p, c);
        }
    }

    fn fill(&mut self, r: [i32; 4], c: u32) {
        for y in r[1]..r[3] {
            for x in r[0]..r[2] {
                self.put(x, y, c);
            }
        }
    }

    fn frame(&mut self, r: [i32; 4], c: u32) {
        self.fill([r[0], r[1], r[2], r[1] + 1], c);
        self.fill([r[0], r[3] - 1, r[2], r[3]], c);
        self.fill([r[0], r[1] + 1, r[0] + 1, r[3] - 1], c);
        self.fill([r[2] - 1, r[1] + 1, r[2], r[3] - 1], c);
    }

    /// Text at `scale` with its top left at (x, y).
    fn text(&mut self, x: i32, y: i32, s: &str, c: u32, scale: i32) {
        let mut x = x;
        for ch in s.chars() {
            let Some((g, l, w)) = glyph(ch) else {
                x += 3 * scale;
                continue;
            };
            for (row, bits) in g.iter().enumerate() {
                for col in l..l + w {
                    if bits >> col & 1 != 0 {
                        let (px, py) = (x + (col - l) * scale, y + row as i32 * scale);
                        self.fill([px, py, px + scale, py + scale], c);
                    }
                }
            }
            x += (w + 1) * scale;
        }
    }

    /// Text in a rectangle: `align` 0 left, 1 centred, 2 right; vertically centred.
    fn label(&mut self, r: [i32; 4], s: &str, c: u32, align: u8, scale: i32) {
        let w = text_width(s) * scale;
        let x = match align {
            1 => (r[0] + r[2] - w) / 2,
            2 => r[2] - w,
            _ => r[0],
        };
        self.text(x, (r[1] + r[3] - 8 * scale) / 2 + scale, s, c, scale);
    }
}

/// `c` (ARGB) over `p` (ARGB).
fn over(p: u32, c: u32) -> u32 {
    let a = c >> 24;
    if a == 255 || p >> 24 == 0 {
        return c;
    }
    let mix = |sh: u32| (((c >> sh) & 0xff) * a + ((p >> sh) & 0xff) * (255 - a)) / 255 << sh;
    (a.max(p >> 24) << 24) | mix(16) | mix(8) | mix(0)
}

/// A character's glyph, its first column and width (None: blank, e.g. a space).
fn glyph(ch: char) -> Option<(&'static [u8; 8], i32, i32)> {
    let ch = if ch.is_ascii() { ch } else { '?' };
    let g = GLYPHS.get((ch as u8).checked_sub(FIRST)? as usize)?;
    let cols = g.iter().fold(0u8, |a, &b| a | b);
    if cols == 0 {
        return None;
    }
    let l = cols.trailing_zeros() as i32;
    let r = 7 - cols.leading_zeros() as i32;
    Some((g, l, r - l + 1))
}

fn text_width(s: &str) -> i32 {
    let w: i32 = s.chars().map(|ch| glyph(ch).map_or(3, |g| g.2 + 1)).sum();
    (w - 1).max(0)
}
