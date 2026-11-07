//! The x87 registers cached in Cranelift values (`lift::X87_CACHE`) give what the uncached
//! code gives: random chains of x87 instructions (with fxch, fld sti(i), popping forms, integer
//! instructions between them, a loop and a branch inside it) recompiled both ways and run
//! on the same data. Needs no game files.

mod common;

use common::{Guest, Rng, SCRATCH, Snippet};
use iced_x86::IcedError;
use iced_x86::code_asm::*;
use std::cell::RefCell;

#[derive(Clone, Copy, Debug)]
enum Op {
    LoadMem(u32),
    LoadInt(u32),
    LoadSt(u32),
    Xch(u32),
    /// 0 add, 1 sub, 2 subr, 3 mul, 4 div, 5 divr: st(0) op= sti(i), or sti(i) op= st(0) (to_i)
    Arith(u32, u32, bool),
    /// faddp-style: sti(i) op= st(0), pop
    ArithP(u32, u32),
    ArithMem(u32, u32),
    Chs,
    Abs,
    StoreMem(u32),
    StorePop(u32),
    StoreF32(u32),
    PopTo(u32),
    Int(u32),
    /// a branch over a fxch st(1) (a join inside the loop)
    Skip,
    /// fldcw of control word j of the data block's four (24 bits truncating or to nearest,
    /// 53 bits)
    Cw(u32),
}

thread_local! {
    static PROG: RefCell<Vec<Op>> = const { RefCell::new(Vec::new()) };
}

fn sti(i: u32) -> AsmRegisterSt {
    [st0, st1, st2, st3, st4, st5, st6, st7][i as usize]
}

/// The data block: inputs (doubles at 0..128, dwords at 128..192, control words at 600),
/// outputs from 256.
fn prog(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.mov(eax, dword_ptr(esp + 4))?;
    a.mov(ecx, 3u32)?;
    a.fld(qword_ptr(eax))?;
    a.fld(qword_ptr(eax + 8))?;
    a.fld(qword_ptr(eax + 16))?;
    let mut top = a.create_label();
    a.set_label(&mut top)?;
    let ops = PROG.with_borrow(|p| p.clone());
    for op in ops {
        match op {
            Op::LoadMem(j) => a.fld(qword_ptr(eax + 8 * j))?,
            Op::LoadInt(j) => a.fild(dword_ptr(eax + 128 + 4 * j))?,
            Op::LoadSt(i) => a.fld(sti(i))?,
            Op::Xch(i) => a.fxch(st0, sti(i))?,
            Op::Arith(k, i, to_i) => {
                let (d, s) = if to_i { (sti(i), st0) } else { (st0, sti(i)) };
                match k {
                    0 => a.fadd_2(d, s)?,
                    1 => a.fsub_2(d, s)?,
                    2 => a.fsubr_2(d, s)?,
                    3 => a.fmul_2(d, s)?,
                    4 => a.fdiv_2(d, s)?,
                    _ => a.fdivr_2(d, s)?,
                }
            }
            Op::ArithP(k, i) => match k {
                0 => a.faddp(sti(i), st0)?,
                1 => a.fsubp(sti(i), st0)?,
                2 => a.fsubrp(sti(i), st0)?,
                3 => a.fmulp(sti(i), st0)?,
                4 => a.fdivp(sti(i), st0)?,
                _ => a.fdivrp(sti(i), st0)?,
            },
            Op::ArithMem(k, j) => match k {
                0 => a.fadd(qword_ptr(eax + 8 * j))?,
                1 => a.fsub(qword_ptr(eax + 8 * j))?,
                2 => a.fsubr(qword_ptr(eax + 8 * j))?,
                3 => a.fmul(qword_ptr(eax + 8 * j))?,
                4 => a.fdiv(qword_ptr(eax + 8 * j))?,
                _ => a.fdivr(dword_ptr(eax + 128 + 4 * j))?,
            },
            Op::Chs => a.fchs()?,
            Op::Abs => a.fabs()?,
            Op::StoreMem(j) => a.fst(qword_ptr(eax + 256 + 8 * j))?,
            Op::StorePop(j) => a.fstp(qword_ptr(eax + 256 + 8 * j))?,
            Op::StoreF32(j) => a.fst(dword_ptr(eax + 512 + 4 * j))?,
            Op::PopTo(i) => a.fstp(sti(i))?,
            Op::Int(j) => {
                a.mov(edx, dword_ptr(eax + 128 + 4 * j))?;
                a.add(edx, ecx)?;
                a.mov(dword_ptr(eax + 128 + 4 * ((j + 1) % 16)), edx)?;
            }
            Op::Cw(j) => a.fldcw(word_ptr(eax + 600 + 2 * j))?,
            Op::Skip => {
                let mut skip = a.create_label();
                a.test(ecx, 1u32)?;
                a.jz(skip)?;
                a.fxch(st0, st1)?;
                a.set_label(&mut skip)?;
            }
        }
    }
    a.dec(ecx)?;
    a.jnz(top)?;
    for j in 0..3 {
        a.fstp(qword_ptr(eax + 448 + 8 * j))?;
    }
    a.ret()
}

/// A random chain that starts and ends with three registers on the stack.
fn random_ops(rng: &mut Rng) -> Vec<Op> {
    let mut ops = Vec::new();
    let mut depth = 3u32;
    let r = |rng: &mut Rng, n: u32| (rng.next() % n as u64) as u32;
    for _ in 0..(20 + r(rng, 40)) {
        let op = match r(rng, 16) {
            0 if depth < 8 => Op::LoadMem(r(rng, 16)),
            1 if depth < 8 => Op::LoadInt(r(rng, 16)),
            2 if depth < 8 => Op::LoadSt(r(rng, depth)),
            3 | 4 => Op::Xch(1 + r(rng, depth - 1).min(depth - 2)),
            5 | 6 => Op::Arith(r(rng, 6), r(rng, depth), r(rng, 2) == 0),
            7 if depth > 3 => Op::ArithP(r(rng, 6), 1 + r(rng, depth - 1)),
            8 => Op::ArithMem(r(rng, 6), r(rng, 16)),
            9 => Op::Chs,
            10 => Op::Abs,
            11 => Op::StoreMem(r(rng, 16)),
            12 if depth > 3 => Op::StorePop(r(rng, 16)),
            13 => Op::StoreF32(r(rng, 16)),
            14 if depth > 3 => Op::PopTo(r(rng, depth)),
            15 => match r(rng, 3) {
                0 => Op::Skip,
                1 => Op::Int(r(rng, 16)),
                _ => Op::Cw(r(rng, 4)),
            },
            _ => continue,
        };
        match op {
            Op::LoadMem(_) | Op::LoadInt(_) | Op::LoadSt(_) => depth += 1,
            Op::ArithP(..) | Op::StorePop(_) | Op::PopTo(_) => depth -= 1,
            _ => {}
        }
        ops.push(op);
    }
    while depth > 3 {
        ops.push(Op::StorePop(r(rng, 16)));
        depth -= 1;
    }
    ops
}

const SNIPPETS: [(&str, Snippet); 1] = [("prog", prog)];

fn run(rng: &mut Rng, ops: &[Op], cached: bool) -> Vec<u8> {
    nstrike::lift::X87_CACHE.store(cached, std::sync::atomic::Ordering::Relaxed);
    PROG.with_borrow_mut(|p| *p = ops.to_vec());
    let g = Guest::new(&SNIPPETS);
    let m = &g.mem;
    for k in 0..16 {
        m.slice(SCRATCH + 8 * k, 8).copy_from_slice(&rng.f64().to_le_bytes());
        m.wr32(SCRATCH + 128 + 4 * k, rng.next() as u32 >> (rng.next() % 32));
    }
    for k in 192..640 {
        m.wr8(SCRATCH + k, 0);
    }
    for (j, cw) in [0x37fu16, 0xc7f, 0x07f, 0xe7f].iter().enumerate() {
        m.slice(SCRATCH + 600 + 2 * j as u32, 2).copy_from_slice(&cw.to_le_bytes());
    }
    g.call(0, &[SCRATCH]);
    m.slice(SCRATCH, 640).to_vec()
}

#[test]
fn x87_cache() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    for n in 0..300 {
        let ops = random_ops(&mut rng);
        let seed = rng.next();
        let a = run(&mut Rng(seed), &ops, false);
        let b = run(&mut Rng(seed), &ops, true);
        assert!(a == b, "program {n} differs with the cache: {ops:?}");
    }
}
