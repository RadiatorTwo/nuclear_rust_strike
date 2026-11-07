//! The frame rate and the game's clock.
//!
//! The game reads its clock once a frame in `sysUpdateTick_` (0x4823d0): Watcom's `clock_`,
//! GetTickCount in hundredths of a second. From it come the mission time in 1/60 s
//! (`[0x4ed720]`: the camera, effects) and in 1/(60*256) s (`[0x4ed72c]`: physics and AI, each
//! object moves by the time since its last update). In steps of 10 ms a frame of 16.7 ms moved
//! the world by 10 or 20 ms in turn: the picture shook. The hook below reads the clock in µs,
//! at the time the frame is due rather than the moment of the call: frames paced at a steady
//! rate get steady steps, at any rate (the game's logic goes by time, not by frames).
//!
//! The pace replaces the card's retrace the game waits for at grBufferSwap: the display's
//! refresh rate, or `NSTRIKE_FRAMERATE` (frames a second; 0: unlimited).
use super::kernel32::guest_us;
use super::{log, mem};
use crate::rt::{call_original, Cpu, EAX, EDX, ESP};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Frames a second times 1000 (`NSTRIKE_FRAMERATE`, else the display's refresh rate, else
/// 60); 0: unlimited.
fn rate_mhz() -> u32 {
    static SET: OnceLock<Option<u32>> = OnceLock::new();
    let set = *SET.get_or_init(|| {
        let r = std::env::var("NSTRIKE_FRAMERATE").ok().and_then(|v| v.parse::<u32>().ok());
        match r {
            Some(0) => log!("frame rate: unlimited"),
            Some(r) => log!("frame rate: {r} fps"),
            None => log!("frame rate: the display's refresh rate"),
        }
        r
    });
    match set {
        Some(r) => r * 1000,
        None => Some(super::window::refresh_mhz()).filter(|&r| r > 0).unwrap_or(60_000),
    }
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
        std::thread::sleep(t - now);
    }
    // the next one: a period after this, or after now if the game fell behind
    *NEXT.lock().unwrap() = Some(at.unwrap_or(now) + period);
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

/// The mission time in 1/60 s with its fraction (`[0x4ed720]` without one), as f64 bits.
static FRAMES: AtomicU64 = AtomicU64::new(0);

pub const UPDATE_TICK: u32 = 0x4823d0;

/// sysUpdateTick_ with the clock in µs (see the module's comment). Ticks are 1/100 s:
/// `[0x54d914]` the ticks since the last call (up to 25; may be 0 here: the original made it
/// at least 1, which ran the menus' timers fast above 100 fps), `[0x54d904]` their sum;
/// unless paused (`[0x4ed71d]`), the mission time from its start `[0x4ed728]` (ticks).
/// Returns the old frame count `[0x54d900]`, which it increments.
pub fn update_tick(c: &mut Cpu) {
    let m = mem();
    // clock_: GetTickCount / 10 minus its value at start-up
    let cs = game_us() as f64 / 10_000.0 - m.rd32(0x569190) as f64;
    let clock = cs.floor() as i64 as u32;
    let frac = cs - cs.floor();
    m.wr32(0x54d91c, clock);
    let d = (clock as u16).wrapping_sub(m.rd16(0x54d920));
    m.wr32(0x54d920, clock);
    m.wr32(0x54d904, m.rd32(0x54d904).wrapping_add(d as u32));
    m.wr16(0x54d914, d.min(25));
    if m.rd8(0x4ed71d) == 0 {
        let t = clock.wrapping_sub(m.rd32(0x4ed728)) as i32 as f64 + frac;
        let frames = t * 3.0 / 5.0;
        FRAMES.store(frames.to_bits(), Ordering::Relaxed);
        m.wr32(0x4ed720, frames.floor() as i64 as u32);
        m.wr32(0x4ed724, frames.floor() as i64 as u32);
        m.wr32(0x4ed72c, ((t + 5.0) * 768.0 / 5.0).floor() as i64 as u32);
    }
    m.wr16(0x54d916, m.rd16(0x54d916) ^ 1);
    let n = m.rd32(0x54d900);
    m.wr32(0x54d900, n.wrapping_add(1));
    c.r[EAX] = n;
    c.r[ESP] += 4;
}

// The camera. StrkCam_Update_ moves its angles and distance towards their aims by the whole
// 1/60 s since its last call (`[0x4ed720] - [0x523018]`): Slerp_ and Rlerp_ take a step per
// 1/60 s. Above 60 fps some frames got no step, and it moved in jerks. Here it moves by the
// time with its fraction: the whole steps, then the part of one more step.

pub const CAM_UPDATE: u32 = 0x46cd94;
const CAM_END: u32 = 0x46d068;
pub const SLERP: u32 = 0x413f40;
pub const RLERP: u32 = 0x413fb4;

/// (the time of the camera's last move with its fraction, the `[0x523018]` it left)
static CAM_LAST: Mutex<Option<(f64, u32)>> = Mutex::new(None);
/// While StrkCam_Update_ runs: the time it moves the camera by, in 1/60 s.
static CAM_DT: Mutex<Option<f64>> = Mutex::new(None);

pub fn cam_update(c: &mut Cpu) {
    let m = mem();
    let now = f64::from_bits(FRAMES.load(Ordering::Relaxed));
    let whole = m.rd32(0x4ed720).wrapping_sub(m.rd32(0x523018)) as i32 as f64;
    let dt = match *CAM_LAST.lock().unwrap() {
        Some((t, left)) if left == m.rd32(0x523018) => (now - t).clamp(0.0, whole.max(0.0) + 1.0),
        // the game set the camera's time itself (a new mission): its own steps
        _ => whole,
    };
    *CAM_DT.lock().unwrap() = Some(dt);
    call_original(c, CAM_UPDATE);
    *CAM_DT.lock().unwrap() = None;
    *CAM_LAST.lock().unwrap() = Some((now, m.rd32(0x523018)));
}

/// Slerp_(eax aim, edx -> value, ebx divisor, ecx least step; most step, steps): a value
/// moved towards its aim, a step per 1/60 s.
pub fn slerp(c: &mut Cpu) {
    lerp(c, SLERP, 2);
}

/// Rlerp_(eax aim, edx -> angle, ebx divisor, ecx least step; most step, steps, full turn):
/// the same for an angle.
pub fn rlerp(c: &mut Cpu) {
    lerp(c, RLERP, 3);
}

/// `args` stack arguments, the steps the second; from StrkCam_Update_ the steps are its time.
fn lerp(c: &mut Cpu, f: u32, args: u32) {
    let m = mem();
    let esp = c.r[ESP];
    let dt = *CAM_DT.lock().unwrap();
    let Some(dt) = dt.filter(|_| (CAM_UPDATE..CAM_END).contains(&m.rd32(esp))) else {
        return call_original(c, f);
    };
    let regs = c.r;
    let stack: Vec<u32> = (0..=args).map(|i| m.rd32(esp + 4 * i)).collect();
    let call = |c: &mut Cpu, steps: u32| {
        c.r = regs;
        for (i, &v) in stack.iter().enumerate() {
            m.wr32(esp + 4 * i as u32, v);
        }
        m.wr32(esp + 8, steps);
        call_original(c, f);
    };
    let p = regs[EDX];
    let whole = dt.floor();
    call(c, whole as u32);
    let part = dt - whole;
    if part > 0.0 {
        let a = m.rd32(p);
        let after = c.r;
        call(c, 1);
        let b = m.rd32(p);
        let v = if args == 3 {
            // an angle: the short way round
            let full = stack[3];
            let d = (b.wrapping_sub(a).wrapping_add(full / 2) & (full - 1)) as i32 - (full / 2) as i32;
            a.wrapping_add((d as f64 * part).round() as i32 as u32) & (full - 1)
        } else {
            a.wrapping_add((b.wrapping_sub(a) as i32 as f64 * part).round() as i32 as u32)
        };
        m.wr32(p, v);
        c.r = after;
    }
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
        log!("fps {} (slowest frame {:.1} ms)", *n, worst.as_secs_f64() * 1000.0);
        *s = Some((now, now, 0, Duration::ZERO));
    }
}
