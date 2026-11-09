//! The platform layer: the Windows and DirectX functions the game imports, implemented in
//! Rust on guest memory, and everything around them (process start, guest heap, handles,
//! threads that take turns, calls back into game code).
//!
//! Imports are reached through thunks: every IAT slot holds an address in the thunk area,
//! a call there ends in `rt_foreign` and our event handler, which runs the Rust function,
//! puts its result in eax and removes the return address and (stdcall) arguments.

pub mod advapi32;
pub mod ddraw;
pub mod dinput;
pub mod dsound;
pub mod frames;
pub mod gdi32;
pub mod gl;
pub mod glide;
pub mod kernel32;
pub mod menu;
pub mod midi;
mod font;
pub mod miles;
pub mod misc;
pub mod pictures;
pub mod sample;
pub mod text;
pub mod user32;
pub mod wide;
pub mod window;

pub use crate::image::ResId;
use crate::image::Image;
use crate::rt::{self, Cpu, EAX, ESP, Event, Memory};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

// guest address space
pub const TIB: u32 = 0x0001_0000;
pub const STACK_LO: u32 = 0x0010_0000;
pub const STACK_HI: u32 = 0x0040_0000;
pub const IMAGE_BASE: u32 = 0x0040_0000;
const ARENA_LO: u32 = 0x0100_0000;
const ARENA_HI: u32 = 0x4000_0000;
const THUNK_LO: u32 = 0x7f00_0000;
const THUNK_N: u32 = 0x4000;
/// return address of host calls into guest code (never jumped to: `ret` returns to the host)
const RET_MAGIC: u32 = 0x7ffe_0000;

static MEM: OnceLock<Memory> = OnceLock::new();

/// Guest memory.
pub fn mem() -> &'static Memory {
    MEM.get().expect("guest memory not set up")
}

macro_rules! log {
    ($($t:tt)*) => { eprintln!($($t)*) };
}
pub(crate) use log;

/// One imported function: name, number of 32-bit argument slots it removes (stdcall; -1 for
/// cdecl), and the implementation, which returns eax.
pub type ImpFn = fn(&mut Cpu) -> u32;
pub type Import = (&'static str, i32, ImpFn);

/// The n-th 32-bit argument of the import being called.
pub fn arg(c: &Cpu, n: u32) -> u32 {
    mem().rd32(c.r[ESP] + 4 + 4 * n)
}

// ---- guest memory helpers ---------------------------------------------------------------------

/// A NUL-terminated guest string (bytes as Latin-1).
pub fn cstr(a: u32) -> String {
    let m = mem();
    (0..).map(|k| m.rd8(a + k)).take_while(|&b| b != 0).map(|b| b as char).collect()
}

/// Copy a string into a guest buffer of `n` bytes, NUL-terminated and cut to fit; returns the
/// number of bytes copied without the NUL.
pub fn put_cstr(a: u32, s: &str, n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    let b: Vec<u8> = s.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect();
    let k = b.len().min(n as usize - 1);
    let m = mem();
    m.slice(a, k).copy_from_slice(&b[..k]);
    m.wr8(a + k as u32, 0);
    k as u32
}

/// A new guest string (from the heap, never freed).
pub fn new_cstr(s: &str) -> u32 {
    let p = alloc(s.len() as u32 + 1);
    put_cstr(p, s, s.len() as u32 + 1);
    p
}

// ---- guest heap -------------------------------------------------------------------------------

/// Blocks from one arena, 16-byte aligned; free blocks are merged with their neighbours.
#[derive(Clone)]
struct Heap {
    top: u32,
    mapped: u32,
    used: BTreeMap<u32, u32>,
    free_at: BTreeMap<u32, u32>,
    free_size: BTreeMap<(u32, u32), ()>,
}

static HEAP: Mutex<Heap> =
    Mutex::new(Heap { top: ARENA_LO, mapped: ARENA_LO, used: BTreeMap::new(), free_at: BTreeMap::new(), free_size: BTreeMap::new() });

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Heap {
    fn take_free(&mut self, at: u32, size: u32) {
        self.free_at.remove(&at);
        self.free_size.remove(&(size, at));
    }
    fn put_free(&mut self, mut at: u32, mut size: u32) {
        if let Some((&a, &s)) = self.free_at.range(..at).next_back() {
            if a + s == at {
                self.take_free(a, s);
                at = a;
                size += s;
            }
        }
        if let Some(&s) = self.free_at.get(&(at + size)) {
            self.take_free(at + size, s);
            size += s;
        }
        if at + size == self.top {
            self.top = at;
        } else {
            self.free_at.insert(at, size);
            self.free_size.insert((size, at), ());
        }
    }
    fn alloc(&mut self, size: u32, align: u32) -> u32 {
        let size = (size.max(1) + 15) & !15;
        let align = align.max(16);
        if align == 16 {
            if let Some((&(s, a), _)) = self.free_size.range((size, 0)..).next() {
                self.take_free(a, s);
                if s - size >= 64 {
                    self.put_free(a + size, s - size);
                    self.used.insert(a, size);
                } else {
                    self.used.insert(a, s);
                }
                return a;
            }
        }
        let a = (self.top + align - 1) & !(align - 1);
        if a as u64 + size as u64 > ARENA_HI as u64 {
            return 0;
        }
        if a > self.top {
            let gap = a - self.top;
            let t = self.top;
            self.top = a;
            self.put_free_gap(t, gap);
        }
        self.top = a + size;
        if self.top > self.mapped {
            let hi = (self.top + 0xf_ffff) & !0xf_ffff;
            mem().map(self.mapped, hi);
            self.mapped = hi;
        }
        self.used.insert(a, size);
        a
    }
    fn put_free_gap(&mut self, at: u32, size: u32) {
        self.free_at.insert(at, size);
        self.free_size.insert((size, at), ());
    }
}

/// Zeroed guest memory, 16-byte aligned; 0 when the arena is full.
pub fn alloc(size: u32) -> u32 {
    alloc_aligned(size, 16)
}

pub fn alloc_aligned(size: u32, align: u32) -> u32 {
    let p = lock(&HEAP).alloc(size, align);
    if p != 0 {
        mem().slice(p, size as usize).fill(0);
    }
    p
}

/// Free a block from `alloc`; anything else is ignored (returns false).
pub fn free(p: u32) -> bool {
    let mut h = lock(&HEAP);
    match h.used.remove(&p) {
        Some(s) => {
            h.put_free(p, s);
            true
        }
        None => false,
    }
}

/// The usable size of a block from `alloc` (0 if it is none).
pub fn block_size(p: u32) -> u32 {
    lock(&HEAP).used.get(&p).copied().unwrap_or(0)
}

/// Grow or shrink a block, keeping its contents; new bytes are zero.
pub fn realloc(p: u32, size: u32) -> u32 {
    if p == 0 {
        return alloc(size);
    }
    let old = block_size(p);
    if old == 0 {
        return 0;
    }
    if old >= size {
        return p;
    }
    let q = alloc(size);
    if q != 0 {
        mem().slice(q, old as usize).copy_from_slice(&mem().slice(p, old as usize).to_vec());
        free(p);
    }
    q
}

// ---- thunks -------------------------------------------------------------------------------------

struct Thunk {
    name: String,
    nargs: i32,
    f: Option<ImpFn>,
    /// not implemented: returns 0, and its first call is logged
    stub: bool,
    /// never waits (Glide's drawing and state): the caller keeps the turn (`on_event`)
    quick: bool,
}

static THUNKS: Mutex<Vec<Thunk>> = Mutex::new(Vec::new());

fn thunk_add(name: String, nargs: i32, f: Option<ImpFn>) -> u32 {
    let mut t = lock(&THUNKS);
    assert!((t.len() as u32) < THUNK_N, "out of thunks");
    let quick = name.strip_prefix("glide2x!").is_some_and(|n| !SLOW_GLIDE.contains(&n));
    t.push(Thunk { name, nargs, stub: false, quick, f });
    THUNK_LO + 4 * (t.len() as u32 - 1)
}

/// Glide functions that may wait (the swap's pace, the window) or make the window: the turn is
/// given up during them.
const SLOW_GLIDE: &[&str] = &["grBufferSwap", "grSstWinOpen", "grSstWinClose", "grGlideInit", "grGlideShutdown", "grSstQueryHardware", "grSstQueryBoards"];

fn ret0(_: &mut Cpu) -> u32 {
    0
}

/// A guest address for a function without an implementation yet: it returns 0, and its first
/// call is logged by name.
pub fn thunk_not_implemented(name: &str, nargs: i32) -> u32 {
    let a = thunk_add(name.to_string(), nargs, Some(ret0));
    lock(&THUNKS).last_mut().unwrap().stub = true;
    a
}

/// A guest address that runs `f` when called.
pub fn thunk(name: &str, nargs: i32, f: ImpFn) -> u32 {
    thunk_add(name.to_string(), nargs, Some(f))
}

// ---- COM objects --------------------------------------------------------------------------------

static VTABLES: Mutex<Vec<(usize, u32)>> = Mutex::new(Vec::new());

/// A new COM object: a guest block whose first dword points at a guest vtable of thunks for
/// `methods` (one vtable per method table, shared by all objects). `extra` bytes follow the
/// vtable pointer for the caller's use.
pub fn com_new(methods: &'static [Import], extra: u32) -> u32 {
    let vt = {
        let key = methods.as_ptr() as usize;
        let found = lock(&VTABLES).iter().find(|e| e.0 == key).map(|e| e.1);
        match found {
            Some(v) => v,
            None => {
                let v = alloc(4 * methods.len() as u32);
                for (k, &(name, n, f)) in methods.iter().enumerate() {
                    mem().wr32(v + 4 * k as u32, thunk(name, n, f));
                }
                lock(&VTABLES).push((key, v));
                v
            }
        }
    };
    let obj = alloc(4 + extra);
    mem().wr32(obj, vt);
    obj
}

// ---- guest threads take turns ------------------------------------------------------------------

/// Only one thread runs guest code at a time: the game's threads race when they truly run in
/// parallel. Host code (imports) runs without the turn. Waiting threads get it by priority
/// (SetThreadPriority), then in the order they came; a thread that gave it up at a loop head
/// only because its slice ran out (`polite`) comes after all others.
struct Turn {
    state: Mutex<TurnState>,
    cv: Condvar,
}

struct TurnState {
    held: bool,
    next: u64,
    waiting: Vec<Waiter>,
}

#[derive(Clone, Copy)]
struct Waiter {
    prio: i32,
    polite: bool,
    ticket: u64,
}

impl TurnState {
    /// The ticket of the waiter that gets the turn next.
    fn first(&self) -> Option<u64> {
        self.waiting.iter().max_by_key(|w| (!w.polite, w.prio, std::cmp::Reverse(w.ticket))).map(|w| w.ticket)
    }

    /// The highest priority among the waiters, polite ones included (`BEST`).
    fn best(&self) -> i32 {
        self.waiting.iter().map(|w| w.prio).max().unwrap_or(i32::MIN)
    }
}

static TURN: Turn = Turn { state: Mutex::new(TurnState { held: false, next: 0, waiting: Vec::new() }), cv: Condvar::new() };

/// The highest priority of a thread waiting for the turn (i32::MIN: none), read at loop heads.
static BEST: AtomicI32 = AtomicI32::new(i32::MIN);

thread_local! {
    static HELD: Cell<bool> = const { Cell::new(false) };
    pub static LAST_ERROR: Cell<u32> = const { Cell::new(0) };
}

fn turn_take() {
    turn_take_as(false)
}

fn turn_take_as(polite: bool) {
    let ctl = my_thread();
    // SuspendThread: a thread stops before it runs guest code again
    ctl.wait_resumed();
    let mut t = lock(&TURN.state);
    let me = t.next;
    t.next += 1;
    t.waiting.push(Waiter { prio: ctl.prio.load(Ordering::Relaxed), polite, ticket: me });
    BEST.store(t.best(), Ordering::Relaxed);
    while t.held || t.first() != Some(me) {
        // the running thread hands over at its next loop head
        rt::POLL.store(1, Ordering::Relaxed);
        t = TURN.cv.wait(t).unwrap_or_else(|e| e.into_inner());
    }
    t.waiting.retain(|w| w.ticket != me);
    t.held = true;
    BEST.store(t.best(), Ordering::Relaxed);
    rt::POLL.store(!t.waiting.is_empty() as u32, Ordering::Relaxed);
    HELD.set(true);
    TAKEN.set(tsc());
}

fn turn_give() {
    HELD.set(false);
    let mut t = lock(&TURN.state);
    t.held = false;
    rt::POLL.store(!t.waiting.is_empty() as u32, Ordering::Relaxed);
    drop(t);
    TURN.cv.notify_all();
}

// ---- guest threads: suspension and priority -----------------------------------------------------

/// What other threads may do to a guest thread: suspend it (SuspendThread / ResumeThread, or
/// CreateThread's CREATE_SUSPENDED) and set its priority. A suspended thread stops at its next
/// loop head, or before it runs guest code after an import: never inside host code, so it holds
/// none of our locks (on Windows it stops anywhere).
pub struct GuestThread {
    suspend: Mutex<u32>,
    cv: Condvar,
    /// THREAD_PRIORITY_* (-15..15)
    pub prio: AtomicI32,
}

impl GuestThread {
    fn wait_resumed(&self) {
        let mut n = lock(&self.suspend);
        while *n > 0 {
            n = self.cv.wait(n).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn suspended(&self) -> bool {
        *lock(&self.suspend) > 0
    }

    /// SuspendThread: the count before. The thread stops at its next loop head.
    pub fn suspend(&self) -> u32 {
        let mut n = lock(&self.suspend);
        let was = *n;
        *n += 1;
        rt::POLL.store(1, Ordering::Relaxed);
        was
    }

    /// ResumeThread: the count before; the thread goes on when it reaches 0.
    pub fn resume(&self) -> u32 {
        let mut n = lock(&self.suspend);
        let was = *n;
        if was > 0 {
            *n -= 1;
            if *n == 0 {
                self.cv.notify_all();
            }
        }
        was
    }

    /// The calling thread: if it suspended itself, it waits here (it has not the turn).
    pub fn wait_if_self(self: &std::sync::Arc<Self>) {
        if std::sync::Arc::ptr_eq(self, &my_thread()) {
            self.wait_resumed();
        }
    }
}

static GUEST_THREADS: Mutex<BTreeMap<u32, std::sync::Arc<GuestThread>>> = Mutex::new(BTreeMap::new());

/// The guest thread with this id (kernel32's thread_id); made at its first use.
pub fn guest_thread(id: u32) -> std::sync::Arc<GuestThread> {
    lock(&GUEST_THREADS)
        .entry(id)
        .or_insert_with(|| std::sync::Arc::new(GuestThread { suspend: Mutex::new(0), cv: Condvar::new(), prio: AtomicI32::new(0) }))
        .clone()
}

thread_local! {
    static MY_THREAD: std::sync::Arc<GuestThread> = guest_thread(kernel32::thread_id());
}

pub fn my_thread() -> std::sync::Arc<GuestThread> {
    MY_THREAD.with(|t| t.clone())
}

/// Run host code that waits (a sleep) from a hook, with the turn handed over meanwhile.
pub fn without_turn<R>(f: impl FnOnce() -> R) -> R {
    let held = HELD.get();
    if held {
        turn_give();
    }
    let r = f();
    if held {
        turn_take();
    }
    r
}

/// Whether another guest thread runs or waits for the turn (asked by a thread without it).
pub fn others_running() -> bool {
    let t = lock(&TURN.state);
    t.held || !t.waiting.is_empty()
}

/// How long a thread keeps the turn at loop heads while another waits. Handing it over at
/// every loop head made the game thread and the controller thread (DirectInput polled every
/// 5 ms) trade it 60 000 times a second, each time waking a thread: 15% of the game
/// thread's time.
const SLICE: std::time::Duration = std::time::Duration::from_micros(500);

thread_local! {
    /// When this thread took the turn (`tsc`).
    static TAKEN: Cell<u64> = const { Cell::new(0) };
}

/// The processor's time stamp counter: loop heads ask for the time while another thread waits,
/// and Instant::now cost 8% of the game thread's time there.
fn tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    return unsafe { std::arch::x86_64::_rdtsc() };
    #[cfg(not(target_arch = "x86_64"))]
    return std::time::Instant::now().duration_since(*START.get_or_init(std::time::Instant::now)).as_nanos() as u64;
}

#[cfg(not(target_arch = "x86_64"))]
static START: OnceLock<std::time::Instant> = OnceLock::new();

/// SLICE in `tsc` ticks (measured once).
fn slice_ticks() -> u64 {
    static T: OnceLock<u64> = OnceLock::new();
    *T.get_or_init(|| {
        let (t0, c0) = (std::time::Instant::now(), tsc());
        std::thread::sleep(std::time::Duration::from_millis(20));
        let (dt, dc) = (t0.elapsed(), tsc() - c0);
        (dc as f64 * SLICE.as_secs_f64() / dt.as_secs_f64()) as u64
    })
}

thread_local! {
    /// This thread keeps the turn at loop heads (`hold_turn`).
    static HOLD: Cell<bool> = const { Cell::new(false) };
}

/// Keep the turn at loop heads (true) until released (false): for guest code whose changes
/// are put back afterwards (a picture between frames), so no other thread's change is lost.
pub fn hold_turn(on: bool) {
    HOLD.set(on);
}

/// At a loop head with another thread waiting: let it run first, at once if its priority is
/// higher (as Windows preempts), else after this thread's slice (a higher priority thread
/// spinning does not starve the others: Windows lifts starved threads after a few seconds).
/// A suspended thread stops here.
fn turn_yield() {
    if !HELD.get() || HOLD.get() {
        return;
    }
    let me = my_thread();
    if me.suspended() {
        turn_give();
        turn_take();
        return;
    }
    let best = BEST.load(Ordering::Relaxed);
    if best == i32::MIN {
        // nobody waits (POLL was set for a suspension of another thread, or is stale)
        let t = lock(&TURN.state);
        rt::POLL.store(!t.waiting.is_empty() as u32, Ordering::Relaxed);
        return;
    }
    if best > me.prio.load(Ordering::Relaxed) {
        turn_give();
        turn_take();
    } else if tsc().wrapping_sub(TAKEN.get()) >= slice_ticks() {
        turn_give();
        turn_take_as(true);
    }
}

pub fn set_last_error(e: u32) {
    LAST_ERROR.set(e);
}

// ---- calls between guest and host ---------------------------------------------------------------

/// Call guest code at `addr` with 32-bit arguments (cdecl or stdcall); returns eax.
pub fn guest_call(c: &mut Cpu, addr: u32, args: &[u32]) -> u32 {
    let m = mem();
    let esp = c.r[ESP];
    for &a in args.iter().rev() {
        c.r[ESP] -= 4;
        m.wr32(c.r[ESP], a);
    }
    c.r[ESP] -= 4;
    m.wr32(c.r[ESP], RET_MAGIC);
    let took = !HELD.get();
    if took {
        turn_take();
    }
    rt::run(c, addr);
    if took {
        turn_give();
    }
    c.r[ESP] = esp;
    c.r[EAX]
}

/// NSTRIKE_TRACE_API=1 logs every import called; any other value only those whose name
/// contains it (`NSTRIKE_TRACE_API=glide2x`).
fn trace_api(name: &str) -> bool {
    static T: OnceLock<Option<String>> = OnceLock::new();
    match T.get_or_init(|| std::env::var("NSTRIKE_TRACE_API").ok()) {
        None => false,
        Some(f) => f == "1" || name.contains(f.as_str()),
    }
}

/// `NSTRIKE_PROF=1`: time spent in imports (and waiting for the turn after them), per import,
/// logged every 2 s with the share of the wall time.
fn prof() -> bool {
    static P: OnceLock<bool> = OnceLock::new();
    *P.get_or_init(|| std::env::var_os("NSTRIKE_PROF").is_some_and(|v| v != "0"))
}

fn prof_add(k: usize, call: std::time::Duration, wait: std::time::Duration) {
    use std::time::{Duration, Instant};
    static P: Mutex<Option<(Instant, BTreeMap<usize, (u32, Duration, Duration)>)>> = Mutex::new(None);
    let mut p = lock(&P);
    let (t0, per) = p.get_or_insert_with(|| (Instant::now(), BTreeMap::new()));
    let e = per.entry(k).or_default();
    e.0 += 1;
    e.1 += call;
    e.2 += wait;
    let wall = t0.elapsed();
    if wall.as_secs_f32() < 2.0 {
        return;
    }
    let mut v: Vec<_> = per.iter().collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.1.1 + e.1.2));
    let names = lock(&THUNKS);
    let pct = |d: Duration| 100.0 * d.as_secs_f32() / wall.as_secs_f32();
    let (call, wait) = v.iter().fold((Duration::ZERO, Duration::ZERO), |a, e| (a.0 + e.1.1, a.1 + e.1.2));
    log!("prof: imports {:.1}% (+{:.1}% waiting for the turn) of {:.1} s", pct(call), pct(wait), wall.as_secs_f32());
    for (k, (n, call, wait)) in v.iter().take(8) {
        log!("prof:   {:<28} {:>8} calls {:>5.1}% +{:.1}%", names[**k].name, n, pct(*call), pct(*wait));
    }
    *p = None;
}

fn on_event(c: &mut Cpu, e: Event) {
    match e {
        Event::Foreign(a) if (THUNK_LO..THUNK_LO + 4 * THUNK_N).contains(&a) => {
            let k = ((a - THUNK_LO) / 4) as usize;
            let (f, nargs, quick) = {
                let t = lock(&THUNKS);
                let Some(th) = t.get(k) else { fatal(c, &format!("call to unused thunk {a:#x}")) };
                if trace_api(&th.name) {
                    log!("api {} from {:#x}", th.name, mem().rd32(c.r[ESP]));
                }
                match th.f {
                    Some(f) => {
                        if th.stub {
                            static SEEN: Mutex<BTreeMap<String, ()>> = Mutex::new(BTreeMap::new());
                            if lock(&SEEN).insert(th.name.clone(), ()).is_none() {
                                log!("warn: {} is not implemented (returns 0)", th.name);
                            }
                        }
                        (f, th.nargs, th.quick)
                    }
                    None => fatal(c, &format!("import {} is not implemented yet", th.name)),
                }
            };
            LAST_CALL.store(mem().rd32(c.r[ESP]), std::sync::atomic::Ordering::Relaxed);
            let t0 = prof().then(std::time::Instant::now);
            // 400 000 Glide calls a second: handing the turn over at each made the game thread
            // wait for the others again and again. Those that never wait keep it, once the
            // thread has its GL context (getting it waits for the window's thread).
            // `hold_turn`: all keep it (what another thread changed meanwhile would be undone)
            let keep = (quick && window::gl_ready_here()) || HOLD.get();
            if !keep {
                turn_give();
            }
            let r = f(c);
            let t1 = prof().then(std::time::Instant::now);
            if !keep {
                turn_take();
            }
            if let (Some(t0), Some(t1)) = (t0, t1) {
                prof_add(k, t1 - t0, t1.elapsed());
            }
            c.r[EAX] = r;
            c.r[ESP] += 4 + 4 * nargs.max(0) as u32;
        }
        Event::Foreign(a) => fatal(c, &format!("call to unknown address {a:#x}")),
        // the PlayStation's division by zero does not trap, and the port's code relies on it
        // (a point at the camera's depth): reported once per place, then divided by 1
        Event::Trap(a, 0) => {
            static SEEN: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());
            if SEEN.lock().unwrap().insert(a) {
                log!("division by zero at {a:#x} ({}): divided by 1", crate::image::named(a));
            }
        }
        Event::Trap(a, _) => fatal(c, &format!("int3/hlt/ud2 at {a:#x}")),
        Event::Unimpl(a) => fatal(c, &format!("unsupported instruction at {a:#x}")),
    }
}

/// Return address of the last import call, for crash reports.
static LAST_CALL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Report an access violation in guest memory (a bad game pointer) before the process dies.
fn report_fault(kind: &str, a: usize, pc: usize) {
    let base = mem().base as usize;
    if a >= base && a - base < rt::GUEST_SPAN {
        // the recompiled function the host code belongs to: the nearest start below it
        let func = rt::DISPATCH.get().and_then(|d| d.table.iter().filter(|e| e.1 <= pc).max_by_key(|e| e.1).map(|e| e.0));
        log!(
            "fatal: guest memory {kind} at {:#x} in the function at {} (last import call returned to {:#x})",
            a - base,
            func.map_or("?".into(), crate::image::named),
            LAST_CALL.load(std::sync::atomic::Ordering::Relaxed)
        );
    } else {
        log!("fatal: host memory {kind} at {a:#x} (code {pc:#x})");
    }
}

#[cfg(windows)]
fn catch_faults() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{AddVectoredExceptionHandler, EXCEPTION_POINTERS};
    unsafe extern "system" fn on_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
        let rec = unsafe { &*(*info).ExceptionRecord };
        if rec.ExceptionCode == EXCEPTION_ACCESS_VIOLATION as NTSTATUS {
            let kind = ["read", "write", "execute"].get(rec.ExceptionInformation[0]).unwrap_or(&"access");
            report_fault(kind, rec.ExceptionInformation[1], rec.ExceptionAddress as usize);
        }
        0 // EXCEPTION_CONTINUE_SEARCH
    }
    crate::image::symbols(); // read now, not while handling a fault
    unsafe { AddVectoredExceptionHandler(0, Some(on_fault)) };
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn catch_faults() {
    extern "C" fn on_fault(sig: i32, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
        unsafe {
            let g = &(*(ctx as *const libc::ucontext_t)).uc_mcontext.gregs;
            // page fault error code: bit 1 a write, bit 4 an instruction fetch
            let err = g[libc::REG_ERR as usize];
            let kind = if err & 0x10 != 0 { "execute" } else if err & 2 != 0 { "write" } else { "read" };
            report_fault(kind, (*info).si_addr() as usize, g[libc::REG_RIP as usize] as usize);
            // the default action at the next try: the process ends as it would have
            libc::signal(sig, libc::SIG_DFL);
        }
    }
    crate::image::symbols(); // read now, not while handling a fault
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_fault as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
    }
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
fn catch_faults() {}

fn fatal(c: &Cpu, msg: &str) -> ! {
    let m = mem();
    let sp = c.r[ESP];
    let stack: Vec<String> = (0..8).map(|k| format!("{:08x}", m.rd32(sp + 4 * k))).collect();
    log!("fatal: {msg}");
    log!("  eax {:08x} ecx {:08x} edx {:08x} ebx {:08x}", c.r[0], c.r[1], c.r[2], c.r[3]);
    log!("  esp {:08x} ebp {:08x} esi {:08x} edi {:08x}", c.r[4], c.r[5], c.r[6], c.r[7]);
    log!("  stack: {}", stack.join(" "));
    std::process::exit(101)
}

// ---- threads ------------------------------------------------------------------------------------

/// A thread information block at `tib` for a stack [lo, hi).
fn init_tib(tib: u32, lo: u32, hi: u32) {
    let m = mem();
    m.wr32(tib, 0xffff_ffff); // end of the SEH chain
    m.wr32(tib + 4, hi);
    m.wr32(tib + 8, lo);
    m.wr32(tib + 0x18, tib);
}

/// Guest state for a new thread: its own TIB and a 1 MB stack from the heap.
pub fn new_thread_cpu() -> Cpu {
    let tib = alloc(0x1000);
    let lo = alloc_aligned(1 << 20, 0x1000);
    let hi = lo + (1 << 20);
    init_tib(tib, lo, hi);
    let mut c = Cpu::new(mem());
    c.fs_base = tib;
    c.fcw = 0x27f;
    c.r[ESP] = hi - 64;
    c
}

// ---- files --------------------------------------------------------------------------------------

static GAME_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The folder with NUKEPC.EXE.
pub fn game_dir() -> &'static Path {
    GAME_DIR.get().expect("game folder not set")
}

/// One path below `root`, matching each part case-insensitively against what exists.
fn host_path_in(root: &Path, parts: &[&str]) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in parts {
        let found = std::fs::read_dir(&p)
            .ok()
            .and_then(|d| d.flatten().map(|e| e.file_name()).find(|n| n.to_string_lossy().eq_ignore_ascii_case(part)));
        match found {
            Some(n) => p.push(n),
            None => p.push(part),
        }
    }
    p
}

/// The game's current directory, as parts below the game folder (SetCurrentDirectoryA).
static CWD: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A guest path as parts below the game folder. The game sees itself in C:\NUKEPC and the CD
/// in D: (its `worlds` are the installed `data\Worlds`, its `video` ours); relative paths are
/// below the current directory.
fn guest_parts(guest: &str) -> Vec<String> {
    let b = guest.as_bytes();
    let drive = (b.len() >= 2 && b[1] == b':').then(|| b[0].to_ascii_uppercase());
    let rest = if drive.is_some() { &guest[2..] } else { guest };
    let absolute = rest.starts_with(['\\', '/']);
    let mut parts: Vec<String> = match (drive, absolute) {
        (_, false) => lock(&CWD).clone(),
        _ => Vec::new(),
    };
    for p in rest.split(['\\', '/']).filter(|s| !s.is_empty() && *s != ".") {
        if p == ".." {
            parts.pop();
        } else {
            parts.push(p.to_string());
        }
    }
    if absolute {
        if drive.is_none_or(|d| d == b'C') && parts.first().is_some_and(|p| p.eq_ignore_ascii_case("NUKEPC")) {
            parts.remove(0);
        } else if drive == Some(b'D') && parts.first().is_some_and(|p| p.eq_ignore_ascii_case("worlds")) {
            parts.splice(0..1, ["data".to_string(), "Worlds".to_string()]);
        }
    }
    parts
}

/// SetCurrentDirectoryA.
pub fn set_current_dir(guest: &str) {
    let parts = guest_parts(guest);
    *lock(&CWD) = parts;
}

/// A guest path ("C:\NUKEPC\data\XA.RES", "XA.RES" in the current directory) as a host path
/// inside the game folder.
pub fn host_path(guest: &str) -> PathBuf {
    let parts = guest_parts(guest);
    let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    host_path_in(game_dir(), &parts)
}

// ---- process start -------------------------------------------------------------------------------

fn find_import(dll: &str, name: &str) -> Option<&'static Import> {
    let tables: &[&[Import]] = match dll.to_ascii_uppercase().as_str() {
        "KERNEL32.DLL" => &[kernel32::IMPORTS, kernel32::IMPORTS_MORE],
        "ADVAPI32.DLL" => &[advapi32::IMPORTS],
        "USER32.DLL" => &[user32::IMPORTS],
        "GDI32.DLL" => &[gdi32::IMPORTS],
        "DDRAW.DLL" => &[ddraw::IMPORTS],
        "DSOUND.DLL" => &[dsound::IMPORTS],
        "DINPUT.DLL" => &[dinput::IMPORTS],
        "MSS32.DLL" => &[miles::IMPORTS],
        "WINMM.DLL" => &[dsound::IMPORTS, misc::IMPORTS],
        _ => &[misc::IMPORTS],
    };
    tables.iter().flat_map(|t| t.iter()).find(|i| i.0 == name)
}

/// Game functions replaced by our own (`jit::compile`).
/// Changes to the game's code before it is compiled: (address, its bytes, the new ones).
pub const PATCHES: &[(u32, &[u8], &[u8])] = wide::PATCHES;

pub const HOOKS: &[(u32, crate::rt::Hook)] = &[(SELECTOR_FROM_LINEAR, selector_from_linear), (glide::COPY_BITMAP, glide::copy_bitmap),
    (frames::UPDATE_TICK, frames::update_tick),
    (frames::AUTOPILOT, frames::autopilot),
    (dinput::ATTACH_CONTEXT, dinput::attach_context),
    (dinput::DETACH_CONTEXT, dinput::detach_context),
    (dinput::SEND_HOOK_UP, dinput::send_hook_up),
    (dinput::HOOK_STATES, dinput::hook_states),
    (pictures::CAM_UPDATE, pictures::cam_update),
    (pictures::TERRAIN, pictures::terrain),
    (pictures::DRAW_OBJ, pictures::draw_obj),
    (pictures::DEATH_DOT, pictures::death_dot),
    (pictures::HUD_DRAW, pictures::hud_draw),
    (pictures::FRAME_CHANGE, pictures::frame_change),
    (text::THREAD, text::thread),
    (text::CLOCK, text::clock),
    (wide::SET_WINDOW, wide::set_window),
    (wide::VIEWPORT_INIT, wide::viewport_init),
    (wide::SMFD_ENTER, wide::smfd_enter),
    (wide::SMFD_EXIT, wide::smfd_exit),
    (wide::HUD_MODE_SET, wide::hud_mode_set),
    (wide::TRAPEZOID_ROWS, wide::trapezoid_rows),
    (wide::TRAPEZOID_TILES, wide::trapezoid_tiles),
    (wide::ALLOC_POLY, wide::alloc_poly),
    (wide::RENDER_POLY, wide::render_poly),
];

/// SelectorFromLinearAddress_(eax linear address, ...): Watcom's helper makes an LDT selector
/// for VGA memory (0xa0000) by reading the GDT and LDT (sgdt/sldt). There is no VGA memory
/// here: no selector (0).
const SELECTOR_FROM_LINEAR: u32 = 0x4bf6c0;

fn selector_from_linear(c: &mut Cpu) {
    log!("SelectorFromLinearAddress({:#x}): none", c.r[EAX]);
    c.r[EAX] = 0;
    c.r[ESP] += 4;
}

/// Load the EXE into a new guest address space, bind its imports, and run it on this thread
/// (which needs a large stack: guest calls nest on it). Returns the exit code.
pub fn run_game(img: &Image, game_dir: PathBuf) -> u32 {
    let _ = GAME_DIR.set(game_dir);
    let _ = MEM.set(Memory::reserve());
    let m = mem();
    m.map(IMAGE_BASE, IMAGE_BASE + 0x1000);
    let hdr = &img.data[..0x1000.min(img.data.len())];
    m.slice(IMAGE_BASE, hdr.len()).copy_from_slice(hdr);
    for sec in &img.sections {
        let size = sec.vsize.max(sec.rsize);
        m.map(sec.va, sec.va + size);
        let bytes = img.read(sec.va, size as usize).unwrap();
        m.slice(sec.va, bytes.len()).copy_from_slice(&bytes);
    }
    glide::init();
    m.map(TIB, TIB + 0x1000);
    m.map(STACK_LO, STACK_HI);
    m.map(THUNK_LO, THUNK_LO + 4 * THUNK_N);
    m.map(RET_MAGIC, RET_MAGIC + 0x1000);
    init_tib(TIB, STACK_LO, STACK_HI);

    let mut missing = Vec::new();
    for i in img.imports() {
        let a = match find_import(&i.dll, &i.name) {
            Some(&(name, n, f)) => thunk(name, n, f),
            None => {
                missing.push(i.name.clone());
                thunk_add(format!("{}!{}", i.dll, i.name), 0, None)
            }
        };
        m.wr32(i.iat, a);
    }
    if !missing.is_empty() {
        log!("{} imports not implemented yet: {}", missing.len(), missing.join(" "));
    }

    rt::set_handler(on_event);
    slice_ticks(); // measured now, not at a loop head
    rt::set_yield(turn_yield);
    catch_faults();
    if let Ok(w) = std::env::var("NSTRIKE_WATCH") {
        // where the game is, every 2 s (for hangs); "NSTRIKE_WATCH=568b14,4eee50": also the
        // dwords at these addresses
        let addrs: Vec<u32> = w.split(',').filter_map(|a| u32::from_str_radix(a.trim_start_matches("0x"), 16).ok()).filter(|&a| a >= 0x10000).collect();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            for &a in &addrs {
                log!("watch: [{a:#x}] = {:#010x}", mem().rd32(a));
            }
            let (held, waiting) = { let t = lock(&TURN.state); (t.held, t.waiting.len()) };
            // a thread spinning in guest code yields at its loop heads when another one waits
            rt::POLL.store(1, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(50));
            let y = rt::LAST_YIELD.load(Ordering::Relaxed);
            log!("watch: last loop head {}", crate::image::named(y));
            log!("watch: last import call returned to {:#x}, turn held {held}, {waiting} waiting, poll {}", LAST_CALL.load(Ordering::Relaxed), rt::POLL.load(Ordering::Relaxed));
        });
    }
    if let Some(path) = rt::count_file() {
        // calls per function, every second: "second address calls [name]" lines
        std::thread::spawn(move || {
            use std::io::Write;
            let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("NSTRIKE_COUNT file"));
            for sec in 1.. {
                std::thread::sleep(std::time::Duration::from_secs(1));
                for (k, n) in rt::COUNTS.iter().enumerate() {
                    let n = n.swap(0, Ordering::Relaxed);
                    if n != 0 {
                        let a = crate::discover::TEXT_START + k as u32;
                        let name = crate::image::symbols().get(&a).map_or("", |n| n.as_str());
                        let _ = writeln!(out, "{sec} {a:x} {n} {name}");
                    }
                }
                let _ = out.flush();
            }
        });
    }
    let mut c = Cpu::new(m);
    c.fs_base = TIB;
    c.r[ESP] = STACK_HI - 64;
    turn_take();
    c.r[ESP] -= 4;
    m.wr32(c.r[ESP], RET_MAGIC);
    rt::run(&mut c, img.entry);
    turn_give();
    c.r[EAX]
}

/// C `printf` formatting with 32-bit arguments (`arg(k)` is the k-th one): flags, width,
/// precision, h/l, and d i u x X o c s p (what wsprintf knows).
pub fn printf(fmt: &str, mut arg: impl FnMut(u32) -> u32) -> String {
    let f: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut next = 0;
    let mut take = move || {
        let v = arg(next);
        next += 1;
        v
    };
    let mut i = 0;
    while i < f.len() {
        if f[i] != '%' {
            out.push(f[i]);
            i += 1;
            continue;
        }
        i += 1;
        let (mut left, mut zero, mut plus, mut space, mut alt) = (false, false, false, false, false);
        while i < f.len() && "-0+ #".contains(f[i]) {
            match f[i] {
                '-' => left = true,
                '0' => zero = true,
                '+' => plus = true,
                ' ' => space = true,
                _ => alt = true,
            }
            i += 1;
        }
        let num = |i: &mut usize, take: &mut dyn FnMut() -> u32| -> Option<usize> {
            if *i < f.len() && f[*i] == '*' {
                *i += 1;
                return Some(take() as usize);
            }
            let s = *i;
            while *i < f.len() && f[*i].is_ascii_digit() {
                *i += 1;
            }
            (s != *i).then(|| f[s..*i].iter().collect::<String>().parse().unwrap_or(0))
        };
        let width = num(&mut i, &mut take).unwrap_or(0);
        let prec = if i < f.len() && f[i] == '.' {
            i += 1;
            Some(num(&mut i, &mut take).unwrap_or(0))
        } else {
            None
        };
        while i < f.len() && "hlL".contains(f[i]) {
            i += 1;
        }
        let Some(&conv) = f.get(i) else { break };
        i += 1;
        let mut sign = String::new();
        let mut body = match conv {
            'd' | 'i' => {
                let v = take() as i32;
                if v < 0 {
                    sign.push('-');
                } else if plus {
                    sign.push('+');
                } else if space {
                    sign.push(' ');
                }
                v.unsigned_abs().to_string()
            }
            'u' => take().to_string(),
            'x' => format!("{}{:x}", if alt { "0x" } else { "" }, take()),
            'X' => format!("{}{:X}", if alt { "0X" } else { "" }, take()),
            'o' => format!("{:o}", take()),
            'p' => format!("{:08X}", take()),
            'c' => (take() as u8 as char).to_string(),
            's' => {
                let p = take();
                let s = if p == 0 { "(null)".to_string() } else { cstr(p) };
                match prec {
                    Some(n) => s.chars().take(n).collect(),
                    None => s,
                }
            }
            other => other.to_string(),
        };
        if let (Some(p), true) = (prec, "diuxXo".contains(conv)) {
            while body.len() < p {
                body.insert(0, '0');
            }
        }
        let len = sign.len() + body.len();
        let pad = width.saturating_sub(len);
        if left {
            out += &sign;
            out += &body;
            out.extend(std::iter::repeat_n(' ', pad));
        } else if zero && prec.is_none() && !"sc%".contains(conv) {
            out += &sign;
            out.extend(std::iter::repeat_n('0', pad));
            out += &body;
        } else {
            out.extend(std::iter::repeat_n(' ', pad));
            out += &sign;
            out += &body;
        }
    }
    out
}

/// A resource of the loaded EXE (first language): guest address and size of its data.
pub fn resource(ty: ResId, id: ResId) -> Option<(u32, u32)> {
    let m = mem();
    crate::image::find_resource(&|a| m.rd8(a), IMAGE_BASE, ty, id)
}
