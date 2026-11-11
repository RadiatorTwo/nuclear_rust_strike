//! The port's own settings, in `nstrike.toml` in the game files folder (`setup::root()`).
//!
//! The game's own settings (sound, controls, difficulty, ...) stay in its Config menu and
//! `nukepc.sav`; here is only what the port adds. The file is plain `key = value` lines (a
//! part of TOML) and is written whole with a comment for each setting, at the first start and
//! whenever a setting changes (`update`). `NSTRIKE_*` environment variables override a value
//! for one run without saving it.

use std::path::PathBuf;
use std::sync::Mutex;

/// Texture filtering when the game filters: 0 the original (bilinear, no mipmaps),
/// 1 trilinear mipmaps, 2...16 also anisotropic.
pub const FILTER_CHOICES: &[u32] = &[0, 1, 2, 4, 8, 16];
/// Multisample anti-aliasing: samples per pixel (0 off).
pub const MSAA_CHOICES: &[u32] = &[0, 2, 4, 8];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameRate {
    /// the display's refresh rate
    Display,
    /// as fast as it goes
    Unlimited,
    /// frames a second
    Fps(u32),
}

/// The sun shadows' quality (`platform::shadows`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowQuality {
    /// a 1024² map, only what is on screen casts (objects at the screen's edge may lose
    /// their shadow): the fastest
    Low,
    /// a 2048² map, all objects around cast, soft edges
    Mid,
    /// a 4096² map, edges a little soft
    High,
    /// a 4096² map, sharp edges
    Ultra,
}

impl ShadowQuality {
    pub const ALL: [ShadowQuality; 4] = [ShadowQuality::Low, ShadowQuality::Mid, ShadowQuality::High, ShadowQuality::Ultra];

    pub fn name(self) -> &'static str {
        match self {
            ShadowQuality::Low => "low",
            ShadowQuality::Mid => "mid",
            ShadowQuality::High => "high",
            ShadowQuality::Ultra => "ultra",
        }
    }

    pub fn parse(v: &str) -> Option<ShadowQuality> {
        Self::ALL.into_iter().find(|q| q.name().eq_ignore_ascii_case(v.trim_matches('"')))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// the window's size in pixels at the start; None: 1280x960 at the desktop's scale
    pub window: Option<(u32, u32)>,
    /// borderless over the whole display
    pub fullscreen: bool,
    pub frame_rate: FrameRate,
    /// the picture changes only at the display's refresh (no tearing)
    pub vsync: bool,
    /// samples per pixel, one of `MSAA_CHOICES`
    pub msaa: u32,
    /// one of `FILTER_CHOICES`
    pub texture_filter: u32,
    /// missions as wide as the window (Hor+), the HUD in the 4:3 middle
    pub widescreen: bool,
    /// ambient occlusion: corners and the ground under things darker (`platform::ao`)
    pub ambient_occlusion: bool,
    /// dynamic lights: glows (muzzle flashes, explosions) light what is near them (`platform::lights`)
    pub dynamic_lights: bool,
    /// sun shadows from what is on screen (`platform::shadows`)
    pub shadows: bool,
    pub shadow_quality: ShadowQuality,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { window: None, fullscreen: false, frame_rate: FrameRate::Display, vsync: true, msaa: 4, texture_filter: 16, widescreen: true, ambient_occlusion: true, dynamic_lights: true, shadows: true, shadow_quality: ShadowQuality::Mid }
    }
}

/// The values from the file (not the environment's): what `update` saves.
static FILE: Mutex<Option<Settings>> = Mutex::new(None);

fn path() -> PathBuf {
    crate::setup::root().join("nstrike.toml")
}

fn file() -> std::sync::MutexGuard<'static, Option<Settings>> {
    let mut f = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if f.is_none() {
        let p = path();
        let s = match std::fs::read_to_string(&p) {
            Ok(text) => parse(&text, |e| eprintln!("{}: {e}", p.display())),
            Err(_) => {
                let s = Settings::default();
                save(&s);
                s
            }
        };
        *f = Some(s);
    }
    f
}

/// The settings in effect: the file's, with `NSTRIKE_*` variables over them.
pub fn get() -> Settings {
    let mut s = file().clone().unwrap();
    let env = |name: &str| std::env::var(name).ok();
    if let Some(v) = env("NSTRIKE_WINDOW") {
        s.window = size(&v);
    }
    if let Some(v) = env("NSTRIKE_FULLSCREEN") {
        s.fullscreen = v == "1";
    }
    if let Some(r) = env("NSTRIKE_FRAMERATE").and_then(|v| frame_rate(&v)) {
        s.frame_rate = r;
    }
    if let Some(v) = env("NSTRIKE_VSYNC") {
        s.vsync = v == "1";
    }
    if let Some(n) = env("NSTRIKE_MSAA").and_then(|v| v.parse().ok()) {
        s.msaa = n;
    }
    if let Some(n) = env("NSTRIKE_FILTER").and_then(|v| v.parse().ok()) {
        s.texture_filter = n;
    }
    if let Some(v) = env("NSTRIKE_WIDESCREEN") {
        s.widescreen = v == "1";
    }
    if let Some(v) = env("NSTRIKE_SSAO") {
        s.ambient_occlusion = v == "1";
    }
    if let Some(v) = env("NSTRIKE_LIGHTS") {
        s.dynamic_lights = v == "1";
    }
    if let Some(v) = env("NSTRIKE_SHADOWS") {
        s.shadows = v == "1";
    }
    if let Some(q) = env("NSTRIKE_SHADOW_QUALITY").and_then(|v| ShadowQuality::parse(&v)) {
        s.shadow_quality = q;
    }
    s
}

/// Change settings and save them (the environment's values are not saved).
pub fn update(f: impl FnOnce(&mut Settings)) {
    let mut g = file();
    let s = g.as_mut().unwrap();
    f(s);
    save(s);
}

fn save(s: &Settings) {
    let p = path();
    if let Err(e) = std::fs::write(&p, render(s)) {
        eprintln!("{}: {e}", p.display());
    }
}

fn size(v: &str) -> Option<(u32, u32)> {
    let (w, h) = v.trim().split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?)).filter(|&(w, h)| w > 0 && h > 0)
}

fn frame_rate(v: &str) -> Option<FrameRate> {
    match v.trim() {
        "display" => Some(FrameRate::Display),
        "unlimited" | "0" => Some(FrameRate::Unlimited),
        n => n.parse().ok().map(FrameRate::Fps),
    }
}

/// The file's text; what is missing or wrong keeps its default (and is reported).
pub fn parse(text: &str, mut warn: impl FnMut(String)) -> Settings {
    let mut s = Settings::default();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, v)) = line.split_once('=') else {
            warn(format!("line {}: not `key = value`", n + 1));
            continue;
        };
        let (key, raw) = (key.trim(), v.trim());
        let v = raw.trim_matches('"');
        let ok = match key {
            "window" => match v {
                "auto" => Some(s.window = None),
                v => size(v).map(|z| s.window = Some(z)),
            },
            "fullscreen" => v.parse().ok().map(|b| s.fullscreen = b),
            "frame_rate" => frame_rate(v).map(|r| s.frame_rate = r),
            "vsync" => v.parse().ok().map(|b| s.vsync = b),
            "msaa" => v.parse().ok().filter(|n| MSAA_CHOICES.contains(n)).map(|n| s.msaa = n),
            "texture_filter" => v.parse().ok().filter(|n| FILTER_CHOICES.contains(n)).map(|n| s.texture_filter = n),
            "widescreen" => v.parse().ok().map(|b| s.widescreen = b),
            "ambient_occlusion" => v.parse().ok().map(|b| s.ambient_occlusion = b),
            "dynamic_lights" => v.parse().ok().map(|b| s.dynamic_lights = b),
            "shadows" => v.parse().ok().map(|b| s.shadows = b),
            "shadow_quality" => ShadowQuality::parse(v).map(|q| s.shadow_quality = q),
            _ => {
                warn(format!("line {}: unknown setting `{key}`", n + 1));
                continue;
            }
        };
        if ok.is_none() {
            warn(format!("line {}: `{raw}` is not a value for `{key}`", n + 1));
        }
    }
    s
}

fn list(c: &[u32]) -> String {
    c.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
}

/// The file written whole: every setting with what it does.
pub fn render(s: &Settings) -> String {
    let window = match s.window {
        Some((w, h)) => format!("{w}x{h}"),
        None => "auto".into(),
    };
    let rate = match s.frame_rate {
        FrameRate::Display => "\"display\"".into(),
        FrameRate::Unlimited => "\"unlimited\"".into(),
        FrameRate::Fps(n) => n.to_string(),
    };
    format!(
        "# Settings of nstrike (the port). The game's own settings are in its Config menu.\n\
         # This file is rewritten when a setting changes in the game: own comments are lost.\n\
         \n\
         # The window's size at the start in pixels (\"1920x1080\"), or \"auto\" (1280x960 at the\n\
         # desktop's scale).\n\
         window = \"{window}\"\n\
         \n\
         # Borderless over the whole display: true or false.\n\
         fullscreen = {}\n\
         \n\
         # Frames a second: \"display\" (the display's refresh rate), \"unlimited\", or a number.\n\
         frame_rate = {rate}\n\
         \n\
         # Vertical sync: the picture changes only at the display's refresh (no tearing; at most\n\
         # the display's refresh rate): true or false.\n\
         vsync = {}\n\
         \n\
         # Anti-aliasing (MSAA), samples per pixel: {} (0 off).\n\
         msaa = {}\n\
         \n\
         # Texture filtering: 0 the original (bilinear), 1 trilinear mipmaps, 2 to 16 also\n\
         # anisotropic. One of {}.\n\
         texture_filter = {}\n\
         \n\
         # Missions as wide as the window (the HUD stays in the 4:3 middle): true or false.\n\
         widescreen = {}\n\
         \n\
         # Ambient occlusion: corners and the ground under things get darker: true or false.\n\
         ambient_occlusion = {}\n\
         \n\
         # Dynamic lights: muzzle flashes and explosions light what is near them: true or false.\n\
         dynamic_lights = {}\n\
         \n\
         # Shadows: the sun casts shadows from what is on screen: true or false.\n\
         shadows = {}\n\
         \n\
         # Shadow quality: \"low\" (only what is on screen casts: fastest), \"mid\", \"high\" or\n\
         # \"ultra\" (sharper edges, more work for the graphics card).\n\
         shadow_quality = \"{}\"\n",
        s.fullscreen,
        s.vsync,
        list(MSAA_CHOICES),
        s.msaa,
        list(FILTER_CHOICES),
        s.texture_filter,
        s.widescreen,
        s.ambient_occlusion,
        s.dynamic_lights,
        s.shadows,
        s.shadow_quality.name()
    )
}
