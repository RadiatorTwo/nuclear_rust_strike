//! A timing of recompiled x87 code like the game's projection routines: points of three
//! 16-bit integers through a 3x4 float matrix, divided by their depth, stored as floats
//! and integers, with the renderer's control word (24 bits, truncating) and with the usual
//! one (53 bits to nearest). Not a check: run it on its own when working on the x87 code,
//! `cargo test --release --test x87_bench -- --ignored --nocapture`. Needs no game files.

mod common;

use common::{Guest, SCRATCH, Snippet};
use iced_x86::IcedError;
use iced_x86::code_asm::*;
use std::time::Instant;

/// Points per call.
const POINTS: u32 = 100;
/// The data block: the matrix (12 floats) at 0, the points at 0x100, the results (x y as
/// floats, z as an integer) at 0x400, the control word at 0xf00.
const MATRIX: u32 = 0;
const PTS: u32 = 0x100;
const OUT: u32 = 0x400;
const CW: u32 = 0xf00;

fn project(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.push(esi)?;
    a.push(edi)?;
    a.mov(esi, dword_ptr(esp + 12))?;
    a.lea(edx, dword_ptr(esi + PTS))?;
    a.lea(edi, dword_ptr(esi + OUT))?;
    a.mov(ecx, POINTS)?;
    a.fldcw(word_ptr(esi + CW))?;
    let mut top = a.create_label();
    a.set_label(&mut top)?;
    a.fild(word_ptr(edx))?;
    a.fild(word_ptr(edx + 2))?;
    a.fild(word_ptr(edx + 4))?;
    // rows: x' = m0 x + m1 y + m2 z + m3, likewise y' and z'
    for row in 0..3u32 {
        let m = esi + MATRIX + 16 * row;
        a.fld(st2)?;
        a.fmul(dword_ptr(m))?;
        a.fld(st2)?;
        a.fmul(dword_ptr(m + 4))?;
        a.faddp(st1, st0)?;
        a.fld(st1)?;
        a.fmul(dword_ptr(m + 8))?;
        a.faddp(st1, st0)?;
        a.fadd(dword_ptr(m + 12))?;
    }
    // st0 z', st1 y', st2 x', st3..5 the point
    a.fist(dword_ptr(edi + 8))?;
    a.fld1()?;
    a.fdivrp(st1, st0)?;
    a.fmul_2(st1, st0)?;
    a.fmulp(st2, st0)?;
    a.fstp(dword_ptr(edi + 4))?;
    a.fstp(dword_ptr(edi))?;
    a.fstp(st0)?;
    a.fstp(st0)?;
    a.fstp(st0)?;
    a.add(edx, 6)?;
    a.add(edi, 12)?;
    a.dec(ecx)?;
    a.jnz(top)?;
    a.pop(edi)?;
    a.pop(esi)?;
    a.ret()?;
    Ok(())
}

const SNIPPETS: [(&str, Snippet); 1] = [("project", project)];

#[test]
#[ignore]
fn x87_bench() {
    let g = Guest::new(&SNIPPETS);
    let m = [0.8f32, -0.6, 0.1, 100.0, 0.6, 0.8, 0.05, -40.0, -0.1, 0.02, 0.99, 2000.0];
    for (k, v) in m.iter().enumerate() {
        g.mem.wr32(SCRATCH + MATRIX + 4 * k as u32, v.to_bits());
    }
    for k in 0..POINTS * 3 {
        g.mem.wr16(SCRATCH + PTS + 2 * k, ((k * 7919) % 2001) as u16 - 1000);
    }
    for (name, cw) in [("24 bits truncating (0xc7f)", 0xc7fu16), ("53 bits to nearest (0x27f)", 0x27f)] {
        g.mem.wr16(SCRATCH + CW, cw);
        // the fastest of a few rounds: the others had other work of the machine in them
        let calls = 10_000;
        let mut best = f64::MAX;
        for _ in 0..7 {
            let t = Instant::now();
            for _ in 0..calls {
                g.call(0, &[SCRATCH]);
            }
            best = best.min(t.elapsed().as_nanos() as f64 / (calls * POINTS) as f64);
        }
        println!("{name}: {best:.1} ns a point");
    }
}
