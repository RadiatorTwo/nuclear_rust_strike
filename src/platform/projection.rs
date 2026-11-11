//! The renderer's two projections in Rust: vec3fp16_ProjectTerrainListASM_ (0x4b96c0) and
//! vec3fp16_ProjectModelListASM_ (0x4b98c2), hooked by `ao` (which notes the points' depth
//! afterwards). Lifted, the terrain's took ~11% of the game thread (2026-11-11): a
//! software-pipelined x87 loop whose version for the control word 0xc7f switched the SSE
//! rounding twice a point.
//!
//! Both take eax matrix (3x3 floats, then the translation), edx points {x, y, z} s16, ebx out,
//! ecx view (scale x, y, centre x, y as floats; then the screen test: x, y offsets and bounds
//! as integers), the count on the stack (the caller removes it). Under their control word
//! `[0x4d0022]` (0xc7f: 24 bits, truncating) a point is
//!   cz = ((y m5 + x m2) + z m8) + m11, cx = (z m6 + (y m3 + x m0)) + m9,
//!   cy = (z m7 + (y m4 + x m1)) + m10, q = k / cz (k `[0x4d002c]`),
//!   sx = cx q s0 + c0 + magic x, sy = cy q s1 + c1 + magic y,
//! each operation rounded by the FPU (24 bits, truncating), sx and sy stored as floats whose
//! low 16 bits are the screen position in 1/8 pixel (the magic numbers put it there). Out, 8
//! bytes a point: sx, sy (s16), cz as integer (fist, truncating; 4 bytes) with its third byte
//! replaced by "on screen": sx + off x < bound x and sy + off y < bound y (unsigned); the
//! model's also wants the float's bits 16..19 to be those of the 16-bit value sign-extended
//! (no overflow of the magic number's trick), and returns in eax whether any point but the
//! last was on screen (`[0x4d001c]`). The last point's float bits and integer stay in `[0x4d0010]`,
//! `[0x4d0014]`, `[0x4d0018]`, the control word as it was in `[0x4d0020]`.
//!
//! The SSE rounding truncates for the whole list (`rt_mxcsr_rz`): a double result is then the
//! exact one truncated, and cut to 24 bits it is the FPU's (as the lifter's `arith_rz`);
//! zero, subnormal and non-finite results go to `rt_fpu_round`. With a count below 2 the
//! original loops ~4 billion times (its loop runs count - 1 times, counting down past 0): that
//! case, and another control word, run the original. `NSTRIKE_PROJ_CHECK=1` runs both and
//! logs differences.

use super::{arg, mem};
use crate::rt::{Cpu, EAX, EBX, ECX, EDX, ESP, FpuOp, call_original, rt_fpu_round, rt_mxcsr_rz, rt_mxcsr_set};

const CW_SAVED: u32 = 0x4d0020;
const CW: u32 = 0x4d0022;
const LAST_X: u32 = 0x4d0010;
const LAST_Y: u32 = 0x4d0014;
const LAST_Z: u32 = 0x4d0018;
const ANY: u32 = 0x4d001c;
const MAGIC_X: u32 = 0x4d0024;
const MAGIC_Y: u32 = 0x4d0028;
const K: u32 = 0x4d002c;

/// x op y as the FPU gives it under 0xc7f, with the SSE rounding truncating.
#[inline(always)]
fn op(o: FpuOp, x: f64, y: f64, cw: u16) -> f64 {
    let s = match o {
        FpuOp::Add => x + y,
        FpuOp::Mul => x * y,
        _ => x / y,
    };
    let bits = s.to_bits();
    let mag = bits & !(1u64 << 63);
    // normal (and not the largest double: truncating, an overflow gives it)
    if mag.wrapping_sub(0x0010_0000_0000_0000) < 0x7fdf_ffff_ffff_ffff {
        return f64::from_bits(bits & !((1u64 << 29) - 1));
    }
    // zero: exact unless a product or quotient of nonzero numbers fell below the doubles' range
    let exact = match o {
        FpuOp::Mul => x == 0.0 || y == 0.0,
        FpuOp::Div => x == 0.0,
        _ => true,
    };
    if mag == 0 && exact {
        return s;
    }
    rt_fpu_round(o as u32, x, y, cw as u32)
}

/// fist dword under a truncating control word: the "integer indefinite" when out of range.
fn fist(v: f64) -> u32 {
    if v >= -2147483648.0 && v < 2147483648.0 { v as i32 as u32 } else { 0x8000_0000 }
}

/// Project the list into guest memory as the original does; false if the original is to run.
/// Registers are not touched (`finish`).
fn compute(c: &Cpu, model: bool) -> bool {
    let m = mem();
    let cw = m.rd16(CW);
    let n = arg(c, 0);
    if cw & 0xf00 != 0xc00 || n < 2 {
        return false;
    }
    let (mat, pts, out, view) = (c.r[EAX], c.r[EDX], c.r[EBX], c.r[ECX]);
    let f = |a: u32| f32::from_bits(m.rd32(a)) as f64;
    let mx: [f64; 12] = std::array::from_fn(|k| f(mat + 4 * k as u32));
    let (s0, s1, c0, c1) = (f(view), f(view + 4), f(view + 8), f(view + 0xc));
    let (off_x, off_y, bound_x, bound_y) = (m.rd32(view + 0x10), m.rd32(view + 0x14), m.rd32(view + 0x18), m.rd32(view + 0x1c));
    let (k, magic_x, magic_y) = (f(K), f(MAGIC_X), f(MAGIC_Y));
    m.wr16(CW_SAVED, c.fcw);
    use FpuOp::{Add, Div, Mul};
    let old = rt_mxcsr_rz();
    let (mut any, mut last) = (0u8, (0u32, 0u32, 0u32));
    for i in 0..n {
        let p = pts.wrapping_add(6 * i);
        let (x, y, z) = (m.rd16(p) as i16 as f64, m.rd16(p + 2) as i16 as f64, m.rd16(p + 4) as i16 as f64);
        let cz = op(Add, op(Add, op(Add, op(Mul, y, mx[5], cw), op(Mul, x, mx[2], cw), cw), op(Mul, z, mx[8], cw), cw), mx[11], cw);
        let cx = op(Add, op(Add, op(Mul, z, mx[6], cw), op(Add, op(Mul, y, mx[3], cw), op(Mul, x, mx[0], cw), cw), cw), mx[9], cw);
        let cy = op(Add, op(Add, op(Mul, z, mx[7], cw), op(Add, op(Mul, y, mx[4], cw), op(Mul, x, mx[1], cw), cw), cw), mx[10], cw);
        let zi = fist(cz);
        let q = op(Div, k, cz, cw);
        let sx = op(Add, op(Add, op(Mul, op(Mul, cx, q, cw), s0, cw), c0, cw), magic_x, cw);
        let sy = op(Add, op(Add, op(Mul, op(Mul, q, cy, cw), s1, cw), c1, cw), magic_y, cw);
        // fstp dword: the SSE conversion truncates as the control word does
        let (rx, ry) = ((sx as f32).to_bits(), (sy as f32).to_bits());
        let (ix, iy) = (rx as u16 as i16 as i32 as u32, ry as u16 as i16 as i32 as u32);
        let mut on = ix.wrapping_add(off_x) < bound_x && iy.wrapping_add(off_y) < bound_y;
        if model {
            on &= ix & 0xf_0000 == rx & 0xf_0000 && iy & 0xf_0000 == ry & 0xf_0000;
        }
        let r = out.wrapping_add(8 * i);
        m.wr16(r, ix as u16);
        m.wr16(r + 2, iy as u16);
        m.wr32(r + 4, zi);
        m.wr8(r + 6, on as u8);
        // (the original's last point, stored after its loop, is not counted)
        if i + 1 < n {
            any |= on as u8;
        }
        last = (rx, ry, zi);
    }
    m.wr32(LAST_X, last.0);
    m.wr32(LAST_Y, last.1);
    m.wr32(LAST_Z, last.2);
    if model {
        m.wr32(ANY, any as u32);
    }
    rt_mxcsr_set(old);
    true
}

/// The registers as the original leaves them (what its callers may use): edx past the
/// points; the model's eax its result and edx its last test of x (the float's bits 16..19 and
/// whether they were those of the 16-bit value); returned (`ret`, the count left to the
/// caller).
fn finish(c: &mut Cpu, model: bool) {
    let m = mem();
    c.r[EDX] = expected_edx(c.r[EDX], arg(c, 0), model);
    if model {
        c.r[EAX] = m.rd32(ANY);
    }
    c.r[ESP] += 4;
}

fn expected_edx(edx: u32, n: u32, model: bool) -> u32 {
    if !model {
        return edx.wrapping_add(6 * n);
    }
    let rx = mem().rd32(LAST_X);
    let ix = rx as u16 as i16 as i32 as u32;
    (rx & 0xf_0000) | (ix & 0xf_0000 == rx & 0xf_0000) as u32
}

/// The projection at `addr` (`ao::PROJECT_TERRAIN` or `ao::PROJECT_MODEL`).
pub fn run(c: &mut Cpu, addr: u32, model: bool) {
    static CHECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *CHECK.get_or_init(|| std::env::var_os("NSTRIKE_PROJ_CHECK").is_some()) {
        return check(c, addr, model);
    }
    if compute(c, model) {
        finish(c, model);
    } else {
        call_original(c, addr);
    }
}

/// Both, the original's output compared with ours (logged: the first differences).
fn check(c: &mut Cpu, addr: u32, model: bool) {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static LISTS: AtomicU64 = AtomicU64::new(0);
    static BAD: AtomicU64 = AtomicU64::new(0);
    let m = mem();
    let (out, n) = (c.r[EBX], arg(c, 0));
    if !compute(c, model) {
        return call_original(c, addr);
    }
    let globals = |m: &crate::rt::Memory| [LAST_X, LAST_Y, LAST_Z, ANY].map(|a| m.rd32(a));
    let ours: Vec<u8> = m.slice(out, 8 * n as usize).to_vec();
    let (our_g, edx) = (globals(m), expected_edx(c.r[EDX], n, model));
    call_original(c, addr);
    let theirs = m.slice(out, 8 * n as usize);
    let lists = LISTS.fetch_add(1, Relaxed) + 1;
    let diff = (0..n as usize).find(|&i| ours[8 * i..8 * i + 8] != theirs[8 * i..8 * i + 8]);
    let g_diff = our_g[..3] != globals(m)[..3] || (model && our_g[3] != globals(m)[3]);
    let edx_diff = c.r[EDX] != edx;
    if (diff.is_some() || g_diff || edx_diff) && BAD.fetch_add(1, Relaxed) < 20 {
        let at = diff.unwrap_or(0);
        super::log!(
            "projection {addr:#x}: differs (list {lists}, {n} points), point {at}: ours {:02x?} original {:02x?}, globals {:x?} / {:x?}, edx {edx_diff}",
            &ours[8 * at..8 * at + 8],
            &theirs[8 * at..8 * at + 8],
            our_g,
            globals(m)
        );
    }
    if lists % 20_000 == 0 {
        super::log!("projection: {lists} lists checked, {} differ", BAD.load(Relaxed));
    }
}
