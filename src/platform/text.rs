//! The text before and after a mission, written letter by letter (`trantextDisplay_` starts
//! the thread 0x49d4d8 for it). The thread paces itself by Watcom's `clock_` (1/100 s): a
//! picture every 10 ticks, a letter a picture, 30 ticks after a line, 50 at the end; between
//! pictures it sleeps the rest (`Sleep(ticks * 10)`). It is written `SPEED` times as fast: in
//! that thread `clock_` runs `SPEED` times as fast, and its sleeps are as many times shorter.
use crate::rt::{call_original, Cpu, EAX};
use std::cell::Cell;

/// The thread writing the text.
pub const THREAD: u32 = 0x49d4d8;
/// clock_: ticks of 1/100 s since start-up.
pub const CLOCK: u32 = 0x4a5c7e;
/// How many times faster than the original (10 letters a second).
const SPEED: u32 = 4;

thread_local! {
    /// In the text's thread: the real ticks at its first `clock_` (0 before).
    static START: Cell<Option<u32>> = const { Cell::new(None) };
}

pub fn thread(c: &mut Cpu) {
    START.set(Some(0));
    call_original(c, THREAD);
    START.set(None);
}

pub fn clock(c: &mut Cpu) {
    call_original(c, CLOCK);
    let Some(start) = START.get() else { return };
    let now = c.r[EAX];
    let start = if start == 0 { now } else { start };
    START.set(Some(start));
    c.r[EAX] = start.wrapping_add(now.wrapping_sub(start).wrapping_mul(SPEED));
}

/// Sleep(ms) in the text's thread: as many times shorter as its clock runs faster.
pub fn sleep_ms(ms: u32) -> u32 {
    if START.get().is_some() { ms / SPEED } else { ms }
}
