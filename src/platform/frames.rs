//! The frame rate and the game's clock.
//!
//! The game reads its clock once a frame in `sysUpdateTick_` (0x4823d0): Watcom's `clock_`,
//! GetTickCount in hundredths of a second. From it come the mission time in 1/60 s
//! (`[0x4ed720]`) and in 1/(60*256) s (`[0x4ed72c]`: each object moves by the time since its
//! last update). The hook reads the clock in µs.
//!
//! In a mission the game's logic runs 60 frames a second, whatever the display: it was made
//! for that, and much of it counts frames, not time (a missile leaves a trail piece a frame,
//! a time step `>> 8` is whole 1/60 s: above 60 fps explosions never ended, the hook's
//! pick-up took the controls for good). The mission loop waits at the start of each frame
//! until it is due, and its clock goes on by exactly 1/60 s a frame (`logic_frame_us`).
//!
//! Elsewhere (menus) frames are paced at grBufferSwap, in place of the card's retrace: the
//! display's refresh rate, or the settings' `frame_rate` (`NSTRIKE_FRAMERATE`: frames a
//! second, 0 unlimited); the clock is read at the time the frame is due.
//!
//! With the setting `vsync` the picture changes only at the display's refresh (no tearing in
//! fullscreen; in a window the desktop's compositor does that anyway). The pace
//! stays: the driver queues a picture and returns at once while the game is slower than the
//! display, so the swap's return says nothing about the refresh; when the pace runs ahead the
//! swap waits and the pace follows (it falls behind and starts again from then).
use super::kernel32::guest_us;
use super::{log, mem};
use crate::rt::{call_original, Cpu, EAX, ESP};
use crate::settings::FrameRate;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Frames a second times 1000 (the settings' `frame_rate`: else the display's refresh rate,
/// else 60); 0: unlimited.
fn rate_mhz() -> u32 {
    let set = *RATE.lock().unwrap().get_or_insert_with(|| {
        let r = crate::settings::get().frame_rate;
        log_rate(r);
        r
    });
    match set {
        FrameRate::Unlimited => 0,
        FrameRate::Fps(n) => n * 1000,
        FrameRate::Display => Some(super::window::refresh_mhz()).filter(|&r| r > 0).unwrap_or(60_000),
    }
}

/// The frame rate in effect (read from the settings at the first frame).
static RATE: Mutex<Option<FrameRate>> = Mutex::new(None);

fn log_rate(r: FrameRate) {
    match r {
        FrameRate::Unlimited => log!("frame rate: unlimited"),
        FrameRate::Fps(n) => log!("frame rate: {n} fps"),
        FrameRate::Display => log!("frame rate: the display's refresh rate"),
    }
}

/// The frame rate changed (our menu): saved, and used from the next picture on.
pub fn set_rate(r: FrameRate) {
    crate::settings::update(|s| s.frame_rate = r);
    *RATE.lock().unwrap() = Some(r);
    log_rate(r);
}

/// The picture changes at the display's refresh (the setting `vsync`).
pub fn vsync() -> bool {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| VSYNC.store(crate::settings::get().vsync, Ordering::Relaxed));
    VSYNC.load(Ordering::Relaxed)
}

static VSYNC: AtomicBool = AtomicBool::new(false);

/// Vsync changed (our menu): saved, and used from the next picture on.
pub fn set_vsync(on: bool) {
    vsync();
    crate::settings::update(|s| s.vsync = on);
    VSYNC.store(on, Ordering::Relaxed);
}

/// The time the next picture is due (None: unlimited, or none shown yet).
static NEXT: Mutex<Option<Instant>> = Mutex::new(None);

/// At grBufferSwap: wait for the picture's turn. A picture at most every `interval` refreshes
/// (the game asks for 1; 0, no wait, when it runs below 15 fps: paced all the same).
pub fn pace(interval: u32) {
    stats();
    super::sample::start();
    let r = rate_mhz();
    if r == 0 {
        *NEXT.lock().unwrap() = None;
        return;
    }
    let period = Duration::from_nanos(1_000_000_000_000 / r as u64) * interval.max(1);
    let now = Instant::now();
    let at = NEXT.lock().unwrap().filter(|&t| t > now);
    if let Some(t) = at {
        // a sleep may last a millisecond longer than asked: the last part is spun (pictures
        // came 3.7 to 4.7 ms apart at 240 fps, now ~4.2)
        const SPIN: Duration = Duration::from_micros(1500);
        if t - now > SPIN {
            std::thread::sleep(t - now - SPIN);
        }
        while Instant::now() < t {
            std::hint::spin_loop();
        }
    }
    // the next one: a period after this, or after now if the game fell behind
    *NEXT.lock().unwrap() = Some(at.unwrap_or(now) + period);
}

/// The time between two pictures (zero: unlimited).
fn picture_period() -> Duration {
    match rate_mhz() {
        0 => Duration::ZERO,
        r => Duration::from_nanos(1_000_000_000_000 / r as u64),
    }
}

/// The game's time in µs (the guest's clock): when paced, the time the frame being made is
/// due, so that each frame moves the world by the same step.
fn game_us() -> u64 {
    let now = Instant::now();
    let us = guest_us();
    match *NEXT.lock().unwrap() {
        Some(t) if t > now => us + (t - now).as_micros() as u64,
        _ => us,
    }
}

pub const UPDATE_TICK: u32 = 0x4823d0;

/// One logic frame of a mission: the game was made for 60 frames a second (a PlayStation's
/// field rate), and much of its logic counts in frames, not in time.
pub const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);

/// Where the mission loop 0x46be9c calls sysUpdateTick_ at the start of each frame (return
/// addresses): playing, and the transition at its end.
const MISSION_TICKS: [u32; 2] = [0x46bfc8, 0x46c2f1];

/// The mission's logic clock: its start (host time, and the guest's clock in µs then) and
/// the logic frames since.
struct Logic {
    start: Instant,
    start_us: u64,
    frames: u64,
}

static LOGIC: Mutex<Option<Logic>> = Mutex::new(None);

/// The newest logic frame of the mission: the time it was due, and its number.
pub fn logic_frame() -> Option<(Instant, u64)> {
    LOGIC.lock().unwrap().as_ref().map(|l| (l.start + FRAME * l.frames as u32, l.frames))
}

/// When the picture being drawn will be shown: at the next swap's turn, or now.
pub fn next_shown() -> Instant {
    let now = Instant::now();
    NEXT.lock().unwrap().filter(|&t| t > now).unwrap_or(now)
}

/// At the start of a mission frame: until it is due, pictures between frames
/// (`pictures.rs`), then the wait (the turn handed over). Its time on the guest's clock in
/// µs: whole 1/60 s from the mission's start. A computer too slow for 60 runs fewer frames,
/// each a whole number of 1/60 s long.
fn logic_frame_us(c: &mut Cpu) -> u64 {
    // Our menu open: the mission stands still (pictures go on). Afterwards the logic clock goes
    // on with the guest's (by whole ticks), and the mission time `[0x4ed728]` and the last tick
    // `[0x54d920]` move by as much, as the game's own pause does (strikeClockPause_). The
    // logic clock lagged the guest's by the pause before (2026-11-08): the game's pause menu
    // (Esc) then set the mission start from the guest's clock, the mission time went back, and
    // a time step below 0 ran Slerp_ (0x413f40) some 4 billion times: the game froze.
    if super::menu::is_open() {
        let paused = super::menu::wait_closed(|| super::pictures::picture(c));
        let ticks = (paused.as_micros() / 10_000) as u32;
        if let Some(l) = LOGIC.lock().unwrap().as_mut() {
            l.start += Duration::from_millis(ticks as u64 * 10);
            l.start_us += ticks as u64 * 10_000;
            let m = mem();
            m.wr32(0x4ed728, m.rd32(0x4ed728).wrapping_add(ticks));
            m.wr32(0x54d920, m.rd32(0x54d920).wrapping_add(ticks));
        }
    }
    let due = {
        let mut l = LOGIC.lock().unwrap();
        let Some(l) = l.as_mut() else {
            let us = guest_us();
            *l = Some(Logic { start: Instant::now(), start_us: us, frames: 0 });
            return us;
        };
        l.start + FRAME * (l.frames + 1) as u32
    };
    // A picture drawn now is shown at the swap after the one pending (`next_shown`): one
    // whose swap comes after the frame is due would show the game later than the newest
    // logic frame, and was drawn at the newest frame's places instead (a step of 0.69 frame,
    // then 0.31, at 120 Hz: a double image). Such a swap gets the new frame instead: the
    // logic runs at once, up to a picture's period before it is due (its clock does not
    // change: whole 1/60 s).
    let period = picture_period();
    let mut n = 0;
    while next_shown() + period < due && n < 16 {
        super::pictures::picture(c);
        n += 1;
    }
    let mut g = LOGIC.lock().unwrap();
    let l = g.as_mut().unwrap();
    l.frames += 1;
    let now = Instant::now();
    let start = due.checked_sub(period).unwrap_or(due);
    if now < start {
        drop(g);
        super::without_turn(|| std::thread::sleep(start - now));
        g = LOGIC.lock().unwrap();
    } else if now > due && now - due > FRAME {
        // behind: on to the frame of now
        let l = g.as_mut().unwrap();
        l.frames = ((now - l.start).as_nanos() / FRAME.as_nanos()) as u64;
    }
    let l = g.as_ref().unwrap();
    let us = l.start_us + l.frames * 1_000_000 / 60;
    drop(g);
    super::wide::logic_frame(c);
    us
}

/// sysUpdateTick_ (see the module's comment). Ticks are 1/100 s: `[0x54d914]` the ticks since
/// the last call (up to 25; may be 0 here: the original made it at least 1, which ran the
/// menus' timers fast above 100 fps), `[0x54d904]` their sum; unless paused (`[0x4ed71d]`),
/// the mission time from its start `[0x4ed728]` (ticks) in 1/60 s (`[0x4ed720]`) and in
/// 1/(60*256) s (`[0x4ed72c]`). Returns the old frame count `[0x54d900]`, which it increments.
pub fn update_tick(c: &mut Cpu) {
    let m = mem();
    let us = if MISSION_TICKS.contains(&m.rd32(c.r[ESP])) {
        super::dinput::watch_player();
        logic_frame_us(c)
    } else {
        *LOGIC.lock().unwrap() = None;
        game_us()
    };
    // clock_: GetTickCount / 10 minus its value at start-up
    let cs = us as f64 / 10_000.0 - m.rd32(0x569190) as f64;
    let clock = cs.floor() as i64 as u32;
    let frac = cs - cs.floor();
    m.wr32(0x54d91c, clock);
    let d = (clock as u16).wrapping_sub(m.rd16(0x54d920));
    m.wr32(0x54d920, clock);
    m.wr32(0x54d904, m.rd32(0x54d904).wrapping_add(d as u32));
    m.wr16(0x54d914, d.min(25));
    if m.rd8(0x4ed71d) == 0 {
        let t = clock.wrapping_sub(m.rd32(0x4ed728)) as i32 as f64 + frac;
        // in a mission whole frames (but for the float's last bits)
        let frames = (t * 3.0 / 5.0 * 256.0).round() / 256.0;
        m.wr32(0x4ed720, frames.floor() as i64 as u32);
        m.wr32(0x4ed724, frames.floor() as i64 as u32);
        m.wr32(0x4ed72c, ((frames + 3.0) * 256.0).round() as i64 as u32);
    }
    m.wr16(0x54d916, m.rd16(0x54d916) ^ 1);
    let n = m.rd32(0x54d900);
    m.wr32(0x54d900, n.wrapping_add(1));
    c.r[EAX] = n;
    c.r[ESP] += 4;
}

/// `NSTRIKE_FPS=1`: logs the pictures shown per second and the slowest frame's time.
fn stats() {
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("NSTRIKE_FPS").is_some_and(|v| v != "0")) {
        return;
    }
    // (second's start, last swap, frames, slowest frame)
    static S: Mutex<Option<(Instant, Instant, u32, Duration)>> = Mutex::new(None);
    let now = Instant::now();
    let mut s = S.lock().unwrap();
    let (start, last, n, worst) = s.get_or_insert((now, now, 0, Duration::ZERO));
    *worst = (*worst).max(now - *last);
    *last = now;
    *n += 1;
    if now - *start >= Duration::from_secs(1) {
        use super::glide::{DRAWS, PALETTES, SAME_ROWS, REFRESHES, UPLOADS, UPLOAD_TEXELS};
        let t = |a: &std::sync::atomic::AtomicU32| a.swap(0, std::sync::atomic::Ordering::Relaxed);
        log!("fps {} (slowest frame {:.1} ms), draws {}, uploads {} ({} texels), refreshes {}, palettes {}, rows unchanged {}", *n, worst.as_secs_f64() * 1000.0, t(&DRAWS), t(&UPLOADS), t(&UPLOAD_TEXELS), t(&REFRESHES), t(&PALETTES), t(&SAME_ROWS));
        *s = Some((now, now, 0, Duration::ZERO));
    }
}

// The chopper's autopilot (flynVehiIntrBehavior_, eax chopper, edx the time since its last
// move in 1/(60*256) s): it moves towards the place aimed at (picking up a load with the
// hook) by whole steps of 0x114 (1/55.6 s), the time divided and the rest dropped. Frames
// of 1/60 s (256) made no step at all: the autopilot held the controls and never arrived.
// The original's clock went in 1/100 s, 153 or 307 a frame: some frames made a step. Here
// the rest is carried on to the chopper's next move.

pub const AUTOPILOT: u32 = 0x4251dc;
const AUTOPILOT_STEP: u32 = 0x114;

pub fn autopilot(c: &mut Cpu) {
    static REST: Mutex<Option<std::collections::BTreeMap<u32, u32>>> = Mutex::new(None);
    let obj = c.r[EAX];
    let dt = c.r[crate::rt::EDX];
    let total = {
        let mut r = REST.lock().unwrap();
        let rest = r.get_or_insert_with(Default::default).entry(obj).or_insert(0);
        let total = rest.saturating_add(dt);
        *rest = total % AUTOPILOT_STEP;
        total
    };
    c.r[crate::rt::EDX] = total - total % AUTOPILOT_STEP;
    call_original(c, AUTOPILOT);
}
