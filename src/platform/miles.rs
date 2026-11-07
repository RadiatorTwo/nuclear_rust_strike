//! Miles Sound System (`mss32.dll`): the 36 `_AIL_*@n` functions the game imports.
//!
//! Sth_InitAudioSystem (0x4971dc) opens digital output (11025 Hz, stereo, 8-bit; it reads the
//! driver's +0x4c, IDirectSound, for its movie player), MIDI out, a sequence handle, and a timer
//! at 60 Hz (callback 0x497d3c, the game's own music sequencer, which sends channel voice
//! messages). Without all of them it refuses to start ("Unable to initialize audio system").
//!
//! Digital samples play from guest memory through the mixer in `dsound.rs` (`dsound::Sample`).
//! Timers are host threads calling the guest callback. MIDI messages are not played yet
//! (`NSTRIKE_MIDI_LOG=1` logs them). `NSTRIKE_AIL_LOG=1` logs the calls.

use super::{Import, alloc, arg, free, guest_call, log, mem};
use crate::rt::Cpu;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn trace() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var_os("NSTRIKE_AIL_LOG").is_some_and(|v| v != "0"))
}

macro_rules! ail_log {
    ($($t:tt)*) => { if trace() { log!($($t)*) } };
}

// sample status
const SMP_FREE: u32 = 1;
const SMP_DONE: u32 = 2;
const SMP_PLAYING: u32 = 4;
const SMP_STOPPED: u32 = 8;

// sample formats (AIL_set_sample_type) and flags
const DIG_F_16BITS_MASK: u32 = 1;
const DIG_F_STEREO_MASK: u32 = 2;
const DIG_PCM_SIGN: u32 = 1;

/// One sample handle (the key is its guest block, which the game only passes back).
#[derive(Clone)]
struct Sample {
    format: u32,
    flags: u32,
    data: u32,
    bytes: u32,
    rate: u32,
    /// 0...127
    volume: u32,
    /// 0 left ... 64 centre ... 127 right
    pan: u32,
    /// 0: for ever, else times played
    loop_count: u32,
    status: u32,
}

impl Sample {
    fn new() -> Sample {
        Sample { format: 0, flags: 0, data: 0, bytes: 0, rate: 11025, volume: 127, pan: 64, loop_count: 1, status: SMP_DONE }
    }

    /// Left and right gain: volume in proportion, pan as Miles' linear balance.
    fn gain(&self) -> [f32; 2] {
        let v = self.volume.min(127) as f32 / 127.0;
        let p = self.pan.min(127) as f32 / 127.0;
        [v * (2.0 * (1.0 - p)).min(1.0), v * (2.0 * p).min(1.0)]
    }

    fn start(&self, key: u32) {
        super::dsound::sample_start(
            key,
            &super::dsound::Sample {
                data: self.data,
                bytes: self.bytes,
                rate: self.rate,
                channels: if self.format & DIG_F_STEREO_MASK != 0 { 2 } else { 1 },
                bits: if self.format & DIG_F_16BITS_MASK != 0 { 16 } else { 8 },
                signed8: self.flags & DIG_PCM_SIGN != 0,
                gain: self.gain(),
                loops: if self.loop_count == 0 { u32::MAX } else { self.loop_count - 1 },
            },
        );
    }
}

static SAMPLES: Mutex<BTreeMap<u32, Sample>> = Mutex::new(BTreeMap::new());

fn with_sample<R>(h: u32, f: impl FnOnce(&mut Sample) -> R) -> Option<R> {
    SAMPLES.lock().unwrap().get_mut(&h).map(f)
}

// ---- start-up, preferences, drivers -------------------------------------------------------------

static PREFS: Mutex<BTreeMap<u32, u32>> = Mutex::new(BTreeMap::new());

fn startup(_: &mut Cpu) -> u32 {
    ail_log!("AIL_startup");
    1
}

fn shutdown(_: &mut Cpu) -> u32 {
    ail_log!("AIL_shutdown");
    for (&h, _) in SAMPLES.lock().unwrap().iter() {
        super::dsound::sample_end(h);
    }
    for (_, t) in TIMERS.lock().unwrap().iter() {
        t.run.store(false, Ordering::Relaxed);
    }
    0
}

/// AIL_set_preference(number, value): returns the old value.
fn set_preference(c: &mut Cpu) -> u32 {
    let (n, v) = (arg(c, 0), arg(c, 1));
    ail_log!("AIL_set_preference({n}, {v:#x})");
    PREFS.lock().unwrap().insert(n, v).unwrap_or(0)
}

fn get_preference(c: &mut Cpu) -> u32 {
    let n = arg(c, 0);
    PREFS.lock().unwrap().get(&n).copied().unwrap_or(0)
}

/// The digital driver's block: the game reads +0x4c (IDirectSound for its movies).
const DRIVER_SIZE: u32 = 0x100;

/// AIL_waveOutOpen(&driver, &waveout, device, PCMWAVEFORMAT): 0 when it opens.
fn wave_out_open(c: &mut Cpu) -> u32 {
    let (drv_p, wo_p, fmt) = (arg(c, 0), arg(c, 1), arg(c, 3));
    let m = mem();
    ail_log!("AIL_waveOutOpen({} Hz, {} ch, {} bits)", m.rd32(fmt + 4), m.rd16(fmt + 2), m.rd16(fmt + 14));
    let drv = alloc(DRIVER_SIZE);
    m.wr32(drv + 0x4c, super::dsound::new_direct_sound());
    m.wr32(drv_p, drv);
    if wo_p != 0 {
        m.wr32(wo_p, 0);
    }
    0
}

fn wave_out_close(c: &mut Cpu) -> u32 {
    free(arg(c, 0));
    0
}

/// AIL_midiOutOpen(&driver, &midiout, device): `*midiout` points at the HMIDIOUT (the game
/// reads it).
fn midi_out_open(c: &mut Cpu) -> u32 {
    let (drv_p, mo_p) = (arg(c, 0), arg(c, 1));
    ail_log!("AIL_midiOutOpen");
    let drv = alloc(DRIVER_SIZE);
    let m = mem();
    m.wr32(drv, 0x6d1d); // a HMIDIOUT nobody uses
    m.wr32(drv_p, drv);
    if mo_p != 0 {
        m.wr32(mo_p, drv);
    }
    0
}

/// AIL_minimum_sample_buffer_size(driver, rate, format): bytes for a streaming buffer.
fn minimum_sample_buffer_size(c: &mut Cpu) -> u32 {
    let (rate, format) = (arg(c, 1), arg(c, 2));
    let block = (1 + (format & DIG_F_16BITS_MASK)) * (1 + (format & DIG_F_STEREO_MASK) / 2);
    // 50 ms, in 2 KB steps
    ((rate * block / 20).div_ceil(2048) * 2048).max(2048)
}

// ---- samples ------------------------------------------------------------------------------------

fn allocate_sample_handle(_: &mut Cpu) -> u32 {
    let h = alloc(0x40);
    SAMPLES.lock().unwrap().insert(h, Sample::new());
    ail_log!("AIL_allocate_sample_handle -> {h:#x}");
    h
}

fn release_sample_handle(c: &mut Cpu) -> u32 {
    let h = arg(c, 0);
    if SAMPLES.lock().unwrap().remove(&h).is_some() {
        super::dsound::sample_end(h);
        free(h);
    }
    0
}

fn init_sample(c: &mut Cpu) -> u32 {
    let h = arg(c, 0);
    super::dsound::sample_end(h);
    with_sample(h, |s| *s = Sample::new());
    0
}

fn set_sample_type(c: &mut Cpu) -> u32 {
    let (format, flags) = (arg(c, 1), arg(c, 2));
    with_sample(arg(c, 0), |s| {
        s.format = format;
        s.flags = flags;
    });
    0
}

fn set_sample_address(c: &mut Cpu) -> u32 {
    let (data, bytes) = (arg(c, 1), arg(c, 2));
    with_sample(arg(c, 0), |s| {
        s.data = data;
        s.bytes = bytes;
    });
    0
}

/// Update a playing sample after a change of volume, pan or rate.
fn changed(h: u32) {
    if let Some(s) = SAMPLES.lock().unwrap().get(&h) {
        super::dsound::sample_update(h, s.gain(), s.rate);
    }
}

fn set_sample_playback_rate(c: &mut Cpu) -> u32 {
    let (h, rate) = (arg(c, 0), arg(c, 1));
    with_sample(h, |s| s.rate = rate);
    changed(h);
    0
}

fn set_sample_volume(c: &mut Cpu) -> u32 {
    let (h, v) = (arg(c, 0), arg(c, 1));
    with_sample(h, |s| s.volume = v);
    changed(h);
    0
}

fn sample_volume(c: &mut Cpu) -> u32 {
    with_sample(arg(c, 0), |s| s.volume).unwrap_or(0)
}

fn set_sample_pan(c: &mut Cpu) -> u32 {
    let (h, p) = (arg(c, 0), arg(c, 1));
    with_sample(h, |s| s.pan = p);
    changed(h);
    0
}

fn set_sample_loop_count(c: &mut Cpu) -> u32 {
    let n = arg(c, 1);
    with_sample(arg(c, 0), |s| s.loop_count = n);
    0
}

fn sample_loop_count(c: &mut Cpu) -> u32 {
    with_sample(arg(c, 0), |s| s.loop_count).unwrap_or(0)
}

fn set_sample_loop_block(c: &mut Cpu) -> u32 {
    log!("warn: AIL_set_sample_loop_block({:#x}, {}, {}) ignored", arg(c, 0), arg(c, 1), arg(c, 2));
    0
}

fn start_sample(c: &mut Cpu) -> u32 {
    let h = arg(c, 0);
    let s = with_sample(h, |s| {
        s.status = SMP_PLAYING;
        s.clone()
    });
    if let Some(s) = s {
        s.start(h);
    }
    0
}

/// AIL_stop_sample: paused where it is (AIL_resume_sample would go on).
fn stop_sample(c: &mut Cpu) -> u32 {
    let h = arg(c, 0);
    with_sample(h, |s| {
        if s.status == SMP_PLAYING {
            s.status = SMP_STOPPED;
        }
    });
    super::dsound::sample_pause(h, true);
    0
}

fn sample_status(c: &mut Cpu) -> u32 {
    let h = arg(c, 0);
    let st = with_sample(h, |s| s.status).unwrap_or(SMP_FREE);
    if st == SMP_PLAYING && !super::dsound::sample_playing(h) {
        with_sample(h, |s| s.status = SMP_DONE);
        return SMP_DONE;
    }
    st
}

/// Double-buffered streaming (AIL_load_sample_buffer / AIL_sample_buffer_ready): the game
/// never calls them.
fn not_used(c: &mut Cpu) -> u32 {
    log!("warn: a Miles streaming function was called ({:#x})", arg(c, 0));
    u32::MAX
}

// ---- timers -------------------------------------------------------------------------------------

struct Timer {
    callback: u32,
    user: u32,
    hz: Arc<AtomicU32>,
    run: Arc<AtomicBool>,
}

static TIMERS: Mutex<BTreeMap<u32, Timer>> = Mutex::new(BTreeMap::new());

fn register_timer(c: &mut Cpu) -> u32 {
    let callback = arg(c, 0);
    let mut t = TIMERS.lock().unwrap();
    let id = t.keys().next_back().map_or(1, |k| k + 1);
    t.insert(id, Timer { callback, user: 0, hz: Arc::new(AtomicU32::new(100)), run: Arc::new(AtomicBool::new(false)) });
    ail_log!("AIL_register_timer({callback:#x}) -> {id}");
    id
}

fn set_timer_user(c: &mut Cpu) -> u32 {
    let (id, user) = (arg(c, 0), arg(c, 1));
    TIMERS.lock().unwrap().get_mut(&id).map_or(0, |t| std::mem::replace(&mut t.user, user))
}

fn set_timer_frequency(c: &mut Cpu) -> u32 {
    let (id, hz) = (arg(c, 0), arg(c, 1));
    if let Some(t) = TIMERS.lock().unwrap().get(&id) {
        t.hz.store(hz.max(1), Ordering::Relaxed);
    }
    0
}

/// AIL_start_timer: callback(user) on its own thread at the timer's frequency.
fn start_timer(c: &mut Cpu) -> u32 {
    let id = arg(c, 0);
    let t = TIMERS.lock().unwrap();
    let Some(t) = t.get(&id) else { return 0 };
    if t.run.swap(true, Ordering::Relaxed) {
        return 0;
    }
    let (callback, user, hz, run) = (t.callback, t.user, t.hz.clone(), t.run.clone());
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let mut tc = super::new_thread_cpu();
            let mut due = Instant::now();
            while run.load(Ordering::Relaxed) {
                due += Duration::from_secs_f64(1.0 / hz.load(Ordering::Relaxed) as f64);
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                } else if now - due > Duration::from_millis(200) {
                    due = now; // far behind (a pause): no catching up
                }
                if run.load(Ordering::Relaxed) {
                    guest_call(&mut tc, callback, &[user]);
                }
            }
        })
        .expect("cannot start a timer thread");
    0
}

fn stop_timer(c: &mut Cpu) -> u32 {
    if let Some(t) = TIMERS.lock().unwrap().get(&arg(c, 0)) {
        t.run.store(false, Ordering::Relaxed);
    }
    0
}

fn release_timer_handle(c: &mut Cpu) -> u32 {
    if let Some(t) = TIMERS.lock().unwrap().remove(&arg(c, 0)) {
        t.run.store(false, Ordering::Relaxed);
    }
    0
}

// ---- MIDI ---------------------------------------------------------------------------------------

fn midi_log() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var_os("NSTRIKE_MIDI_LOG").is_some_and(|v| v != "0"))
}

fn allocate_sequence_handle(_: &mut Cpu) -> u32 {
    alloc(0x40)
}

fn release_sequence_handle(c: &mut Cpu) -> u32 {
    free(arg(c, 0));
    0
}

/// AIL_send_channel_voice_message(driver, sequence, status, data1, data2): not played yet.
fn send_channel_voice_message(c: &mut Cpu) -> u32 {
    if midi_log() {
        log!("midi {:02x} {:02x} {:02x}", arg(c, 2), arg(c, 3), arg(c, 4));
    }
    0
}

fn ret0(_: &mut Cpu) -> u32 {
    0
}

pub const IMPORTS: &[Import] = &[
    ("_AIL_startup@0", 0, startup),
    ("_AIL_shutdown@0", 0, shutdown),
    ("_AIL_set_preference@8", 2, set_preference),
    ("_AIL_get_preference@4", 1, get_preference),
    ("_AIL_waveOutOpen@16", 4, wave_out_open),
    ("_AIL_waveOutClose@4", 1, wave_out_close),
    ("_AIL_midiOutOpen@12", 3, midi_out_open),
    ("_AIL_midiOutClose@4", 1, wave_out_close),
    ("_AIL_register_timer@4", 1, register_timer),
    ("_AIL_release_timer_handle@4", 1, release_timer_handle),
    ("_AIL_set_timer_frequency@8", 2, set_timer_frequency),
    ("_AIL_set_timer_user@8", 2, set_timer_user),
    ("_AIL_start_timer@4", 1, start_timer),
    ("_AIL_stop_timer@4", 1, stop_timer),
    ("_AIL_allocate_sequence_handle@4", 1, allocate_sequence_handle),
    ("_AIL_release_sequence_handle@4", 1, release_sequence_handle),
    ("_AIL_set_sequence_volume@12", 3, ret0),
    ("_AIL_send_channel_voice_message@20", 5, send_channel_voice_message),
    ("_AIL_allocate_sample_handle@4", 1, allocate_sample_handle),
    ("_AIL_release_sample_handle@4", 1, release_sample_handle),
    ("_AIL_init_sample@4", 1, init_sample),
    ("_AIL_set_sample_type@12", 3, set_sample_type),
    ("_AIL_set_sample_address@12", 3, set_sample_address),
    ("_AIL_load_sample_buffer@16", 4, not_used),
    ("_AIL_sample_buffer_ready@4", 1, not_used),
    ("_AIL_minimum_sample_buffer_size@12", 3, minimum_sample_buffer_size),
    ("_AIL_set_sample_playback_rate@8", 2, set_sample_playback_rate),
    ("_AIL_set_sample_volume@8", 2, set_sample_volume),
    ("_AIL_sample_volume@4", 1, sample_volume),
    ("_AIL_set_sample_pan@8", 2, set_sample_pan),
    ("_AIL_set_sample_loop_count@8", 2, set_sample_loop_count),
    ("_AIL_sample_loop_count@4", 1, sample_loop_count),
    ("_AIL_set_sample_loop_block@12", 3, set_sample_loop_block),
    ("_AIL_start_sample@4", 1, start_sample),
    ("_AIL_stop_sample@4", 1, stop_sample),
    ("_AIL_sample_status@4", 1, sample_status),
];
