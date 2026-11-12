//! Three of gl_RenderPoly_'s renderers in Rust: poly type 8 (0x475314, a textured quad with one
//! grey level), type 4 (0x473c4c, the same as a triangle: three corners, drawn as (0, 1, 2);
//! with flag 0x10 the one double at 0x4f4750 added to every u, v; antialiased it calls
//! grAADrawTriangle, ours the same as grDrawTriangle) and type 9 with flag 0x80 (0x475a34, a textured quad with a shade per corner).
//! Lifted they took ~15% of the game thread (2026-11-12, mission 1): ~500 000 quads a second,
//! each with eight calls of the Glide layer's state setters and two imports.
//!
//! A quad (eax) is: flags (dword), the texture page (u16 at +0xc), then four corners. Type 8:
//! grey level at +0xf, corners of 8 bytes from +0x10 (x, y s16 in 1/8 pixel, u, v bytes);
//! type 9: corners of 10 bytes from +0xe (x, y, u, v, -, shade u16 at +8). The corners go into
//! the Glide layer's four GrVertex at 0x523550 (0x3c bytes each): x, y = (position + draw
//! offset `[0x4ede5c]`) * scale `[0x4ee094]`, clamped to -2048..2047; s, t = u, v plus an
//! offset (type 8 with flag 0x10: the doubles 0.001 / 0.999; type 9: the floats
//! `[0x4ede68]`, `[0x4ede6c]`), by corner (first, first), (second, first), (second, second),
//! (first, second); colour: type 8 the grey level, alpha 255; type 9 a shade above 0x80 white
//! with alpha 2 shade - 256, else grey 2 shade with alpha 0. Drawn as (0, 1, 2) and (0, 3, 2).
//! Every operation as the FPU does it under the game's control word: 0x27f here (53 bits,
//! rounding to nearest: a double's operation, the float store rounding to nearest), 0xc7f
//! elsewhere (24 bits, truncating); with another one the original runs.
//!
//! The Glide layer's setters (0x472590...) keep what they set last in guest memory and call
//! the import only for a change; ours do the same with the same variables (the game's other
//! renderers use them too), the calls go to `glide::run` together. A texture page not yet
//! downloaded (0x472a48) runs the original. `NSTRIKE_RENDER_CHECK=1` runs both and compares
//! the vertices and the setters' variables.

use super::glide::{self, Call};
use super::mem;
use crate::rt::{Cpu, EAX, ESP, Memory, call_original};

/// gl_RenderPoly_'s renderer of poly type 8.
pub const GREY_QUAD: u32 = 0x475314;
/// gl_RenderPoly_'s renderer of poly type 4.
pub const GREY_TRIANGLE: u32 = 0x473c4c;
/// gl_RenderPoly_'s renderer of poly type 9 with flag 0x80.
pub const SHADED_QUAD: u32 = 0x475a34;

/// bit 0: antialiased edges wanted, bit 1: filtering
const GLIDE_FLAGS: u32 = 0x523e04;
/// 0: type 9 is never filtered
const SHADED_FILTER: u32 = 0x4ee090;
const DRAW_OFFSET: u32 = 0x4ede5c;
const SCALE: u32 = 0x4ee094;
const UV_OFFSETS_GREY: u32 = 0x4f4788;
const UV_OFFSETS_SHADED: u32 = 0x4ede68;
/// type 4 with flag 0x10: one double for all corners
const UV_OFFSET_TRI: u32 = 0x4f4750;
const VERTS: u32 = 0x523550;
const VERTEX: u32 = 0x3c;
/// per texture page: GrTexInfo (20 bytes; the format at +0xc), start address, downloaded
const PAGE_INFO: u32 = 0x523740;
const PAGE_START: u32 = 0x5236c0;
const PAGE_LOADED: u32 = 0x523dc0;
const KEY_VALUE: u32 = 0x5239c0;
/// GR_TEXFMT_P_8: chroma keyed, the others alpha tested
const FORMAT_P8: u32 = 5;

// what the setters set last
const S_FILTER: u32 = 0x4ede64;
const S_AA: u32 = 0x4ede65;
const S_CHROMA: u32 = 0x4ede70;
const S_BLEND: u32 = 0x4ede74;
const S_ALPHA_COMBINE: u32 = 0x4ede78;
const S_ALPHA_TEST: u32 = 0x4ede7c;
const S_ALPHA_REF: u32 = 0x4ede80;
const S_COLOR_COMBINE: u32 = 0x4ede84;
const S_TEX: u32 = 0x4ede8c;
const STATE: (u32, usize) = (S_FILTER, 0x2c);

const CLAMP: (f32, f32) = (-2048.0, 2047.0);

/// The Glide calls of one quad.
struct Calls {
    v: [Call; 16],
    n: usize,
}

impl Calls {
    fn new() -> Self {
        Calls { v: [Call::Triangle([0; 3]); 16], n: 0 }
    }
    fn push(&mut self, c: Call) {
        self.v[self.n] = c;
        self.n += 1;
    }
}

/// How the FPU rounds: the control word's precision and rounding fields.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// 0x27f: 53 bits, to nearest
    Nearest,
    /// 0xc7f: 24 bits, truncating
    Truncating,
}

fn mode(fcw: u16) -> Option<Mode> {
    match fcw & 0xf00 {
        0x200 => Some(Mode::Nearest),
        0xc00 => Some(Mode::Truncating),
        _ => None,
    }
}

/// An exact sum or product (s, plus the error of s if any: the exact value is s + err) as a
/// float: the FPU's result stored with fstp dword.
fn cut(mode: Mode, s: f64, err: f64) -> f32 {
    let mut f = s as f32;
    if mode == Mode::Nearest {
        // s is the double the FPU had (53 bits, to nearest), the store rounds it to nearest
        return f;
    }
    let fd = f as f64;
    if fd.abs() > s.abs() || (fd == s && err != 0.0 && (err < 0.0) == (s > 0.0)) {
        f = f32::from_bits(f.to_bits() - 1);
    }
    f
}

/// b + c as the FPU: b small (an integer), c a double.
fn add(mode: Mode, b: f64, c: f64) -> f32 {
    let s = b + c;
    // Fast2Sum (|b| >= |c| or b = 0, then s = c exactly)
    let err = if b == 0.0 { 0.0 } else { c - (s - b) };
    cut(mode, s, err)
}

/// A corner's screen position: (coordinate + offset) * scale, clamped. None if the product
/// might not be exact in a double.
fn place(mode: Mode, v: i16, offset: u32, scale: f32) -> Option<f32> {
    let i = (v as i32).wrapping_add(offset as i32);
    if i.unsigned_abs() >= 1 << 29 {
        return None;
    }
    let p = cut(mode, i as f64 * scale as f64, 0.0);
    Some(if p < CLAMP.0 {
        CLAMP.0
    } else if p > CLAMP.1 {
        CLAMP.1
    } else {
        p
    })
}

/// The corners' x, y of a quad whose corners start at `at`, `stride` bytes apart; None: the
/// original is to run.
fn places<const N: usize>(m: &Memory, mode: Mode, at: u32, stride: u32) -> Option<[(f32, f32); N]> {
    let (ox, oy, scale) = (m.rd32(DRAW_OFFSET), m.rd32(DRAW_OFFSET + 4), f32::from_bits(m.rd32(SCALE)));
    let mut p = [(0.0, 0.0); N];
    for (k, q) in p.iter_mut().enumerate() {
        let a = at + stride * k as u32;
        *q = (place(mode, m.rd16(a) as i16, ox, scale)?, place(mode, m.rd16(a + 2) as i16, oy, scale)?);
    }
    Some(p)
}

/// s, t of the corners from their u, v bytes (at `at`, `stride` apart) and the two offsets.
fn texels<const N: usize>(m: &Memory, mode: Mode, at: u32, stride: u32, c: (f64, f64)) -> [(f32, f32); N] {
    let pick = [(c.0, c.0), (c.1, c.0), (c.1, c.1), (c.0, c.1)];
    std::array::from_fn(|k| {
        let a = at + stride * k as u32;
        (add(mode, m.rd8(a) as f64, pick[k].0), add(mode, m.rd8(a + 1) as f64, pick[k].1))
    })
}

fn write_vertex(m: &Memory, k: u32, xy: (f32, f32), rgba: [f32; 4], st: (f32, f32)) {
    let v = VERTS + VERTEX * k;
    let w = |o: u32, f: f32| m.wr32(v + o, f.to_bits());
    w(0, xy.0);
    w(4, xy.1);
    w(0xc, rgba[0]);
    w(0x10, rgba[1]);
    w(0x14, rgba[2]);
    w(0x1c, rgba[3]);
    w(0x24, st.0);
    w(0x28, st.1);
}

fn draw(calls: &mut Calls) {
    let v = |k: u32| VERTS + VERTEX * k;
    calls.push(Call::Triangle([v(0), v(1), v(2)]));
    calls.push(Call::Triangle([v(0), v(3), v(2)]));
}

// ---- the Glide layer's setters --------------------------------------------------------------

/// 0x472590
fn set_filter(m: &Memory, calls: &mut Calls, on: bool) {
    if m.rd8(S_FILTER) != on as u8 {
        calls.push(Call::Filter(on));
        m.wr8(S_FILTER, on as u8);
    }
}

/// 0x4725cc
fn set_aa(m: &Memory, on: bool) {
    m.wr8(S_AA, on as u8);
}

/// 0x4725e8
fn set_chroma(m: &Memory, calls: &mut Calls, on: u32) {
    if m.rd32(S_CHROMA) != on {
        calls.push(Call::Chromakey(on != 0));
        m.wr32(S_CHROMA, on);
    }
}

/// 0x472710: the reference is not set with the function ALWAYS (7)
fn set_alpha_test(m: &Memory, calls: &mut Calls, f: u32, reference: u8) {
    if m.rd32(S_ALPHA_TEST) != f {
        calls.push(Call::AlphaTestFunction(f));
        m.wr32(S_ALPHA_TEST, f);
    }
    if m.rd8(S_ALPHA_REF) != reference && f != 7 {
        calls.push(Call::AlphaTestReference(reference as u32));
        m.wr8(S_ALPHA_REF, reference);
    }
}

/// 0x472780 (n: its case, `args`: that case's grColorCombine), 0x47268c (grAlphaCombine),
/// 0x472618 (grAlphaBlendFunction).
fn set_cached(m: &Memory, calls: &mut Calls, var: u32, n: u32, call: Call) {
    if m.rd32(var) != n {
        calls.push(call);
        m.wr32(var, n);
    }
}

/// 0x4729d0 (the page downloaded: checked before)
fn set_page(m: &Memory, calls: &mut Calls, page: u32) {
    if m.rd32(S_TEX) != page {
        calls.push(Call::TexSource(m.rd32(PAGE_START + 4 * page)));
        m.wr32(S_TEX, page);
    }
}

/// 0x472938: the transparency of the page's texels (flags bit 1 without bit 15: wanted).
fn set_keying(m: &Memory, calls: &mut Calls, flags: u32, page: u32) {
    let plain = flags & 0x8000 != 0 || flags & 2 == 0;
    if m.rd32(PAGE_INFO + 20 * page + 0xc) != FORMAT_P8 {
        if plain { set_alpha_test(m, calls, 7, 0) } else { set_alpha_test(m, calls, 4, 0xa0) }
    } else if plain {
        set_chroma(m, calls, 0);
    } else {
        set_chroma(m, calls, 1);
        calls.push(Call::ChromakeyValue(m.rd32(KEY_VALUE)));
    }
}

// ---- the renderers ----------------------------------------------------------------------------

/// Type 8 (N 4) or type 4 (N 3) into guest memory; the Glide calls, or None if the original
/// is to run.
fn grey<const N: usize>(c: &Cpu) -> Option<Calls> {
    let m = mem();
    let q = c.r[EAX];
    let (flags, page) = (m.rd32(q), m.rd16(q + 0xc) as u32);
    let mode = mode(c.fcw)?;
    if m.rd8(PAGE_LOADED + page) == 0 {
        return None;
    }
    let g = m.rd8(GLIDE_FLAGS);
    let xy = places::<N>(m, mode, q + 0x10, 8)?;
    let st = if flags & 0x10 != 0 {
        let d = |a: u32| f64::from_bits(m.rd32(a) as u64 | (m.rd32(a + 4) as u64) << 32);
        let c = if N == 4 { (d(UV_OFFSETS_GREY), d(UV_OFFSETS_GREY + 8)) } else { (d(UV_OFFSET_TRI), d(UV_OFFSET_TRI)) };
        texels::<N>(m, mode, q + 0x14, 8, c)
    } else {
        texels::<N>(m, mode, q + 0x14, 8, (0.0, 0.0))
    };
    let mut calls = Calls::new();
    set_filter(m, &mut calls, g & 2 != 0 && flags & 0x1000 != 0);
    set_aa(m, g & 1 != 0 && flags & 0x2000 != 0);
    set_keying(m, &mut calls, flags, page);
    set_page(m, &mut calls, page);
    set_cached(m, &mut calls, S_COLOR_COMBINE, 7, Call::ColorCombine([3, 1, 0, 1, 0]));
    if g & 1 != 0 && m.rd8(S_AA) == 1 {
        set_alpha_test(m, &mut calls, 4, 0);
        set_cached(m, &mut calls, S_ALPHA_COMBINE, 5, Call::AlphaCombine([3, 1, 0, 1, 0]));
        set_cached(m, &mut calls, S_BLEND, 4, Call::AlphaBlend(1, 5));
    } else {
        set_cached(m, &mut calls, S_ALPHA_COMBINE, 4, Call::AlphaCombine([3, 8, 1, 1, 0]));
        set_cached(m, &mut calls, S_BLEND, 2, Call::AlphaBlend(4, 0));
    }
    let grey = m.rd8(q + 0xf) as f32;
    for k in 0..N {
        write_vertex(m, k as u32, xy[k], [grey, grey, grey, 255.0], st[k]);
    }
    if N == 4 {
        draw(&mut calls);
    } else {
        calls.push(Call::Triangle([VERTS, VERTS + VERTEX, VERTS + 2 * VERTEX]));
    }
    Some(calls)
}

/// Type 9 into guest memory; the Glide calls, or None if the original is to run.
fn shaded(c: &Cpu) -> Option<Calls> {
    let m = mem();
    let q = c.r[EAX];
    let (mut flags, page) = (m.rd32(q), m.rd16(q + 0xc) as u32);
    let mode = mode(c.fcw)?;
    if m.rd8(PAGE_LOADED + page) == 0 {
        return None;
    }
    let xy = places::<4>(m, mode, q + 0xe, 10)?;
    let f = |a: u32| f32::from_bits(m.rd32(a)) as f64;
    let st = texels::<4>(m, mode, q + 0x12, 10, (f(UV_OFFSETS_SHADED), f(UV_OFFSETS_SHADED + 4)));
    if m.rd32(SHADED_FILTER) == 0 {
        flags &= !0x1000;
    }
    let g = m.rd8(GLIDE_FLAGS);
    let mut calls = Calls::new();
    set_filter(m, &mut calls, g & 2 != 0 && flags & 0x1000 != 0);
    set_aa(m, g & 1 != 0 && flags & 0x2000 != 0);
    set_page(m, &mut calls, page);
    set_cached(m, &mut calls, S_ALPHA_COMBINE, 9, Call::AlphaCombine([3, 8, 0, 1, 0]));
    set_alpha_test(m, &mut calls, 7, 0);
    set_cached(m, &mut calls, S_BLEND, 2, Call::AlphaBlend(4, 0));
    set_cached(m, &mut calls, S_COLOR_COMBINE, 9, Call::ColorCombine([5, 1, 0, 1, 0]));
    set_chroma(m, &mut calls, 0);
    for k in 0..4 {
        let shade = m.rd16(q + 0x16 + 10 * k) as u32;
        let rgba = if shade > 0x80 {
            [255.0, 255.0, 255.0, (2 * shade) as i32 as f32 - 256.0]
        } else {
            let v = (2 * shade) as f32;
            [v, v, v, 0.0]
        };
        write_vertex(m, k, xy[k as usize], rgba, st[k as usize]);
    }
    draw(&mut calls);
    Some(calls)
}

/// The renderer at `addr`: ours, the original if ours cannot.
fn run(c: &mut Cpu, addr: u32, ours: fn(&Cpu) -> Option<Calls>) {
    static CHECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *CHECK.get_or_init(|| std::env::var_os("NSTRIKE_RENDER_CHECK").is_some()) {
        return check(c, addr, ours);
    }
    match ours(c) {
        Some(calls) => {
            glide::run(&calls.v[..calls.n]);
            // as the original's last import leaves it; ret
            c.r[EAX] = 0;
            c.r[ESP] += 4;
        }
        None => call_original(c, addr),
    }
}

pub fn grey_quad(c: &mut Cpu) {
    run(c, GREY_QUAD, grey::<4>);
}

pub fn grey_triangle(c: &mut Cpu) {
    run(c, GREY_TRIANGLE, grey::<3>);
}

pub fn shaded_quad(c: &mut Cpu) {
    run(c, SHADED_QUAD, shaded);
}

/// Both: ours computed (the setters' variables put back), then the original; the vertices
/// and variables compared (logged: the first differences).
fn check(c: &mut Cpu, addr: u32, ours: fn(&Cpu) -> Option<Calls>) {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static QUADS: AtomicU64 = AtomicU64::new(0);
    static BAD: AtomicU64 = AtomicU64::new(0);
    static TRIANGLES: AtomicU64 = AtomicU64::new(0);
    let m = mem();
    let before = m.slice(STATE.0, STATE.1).to_vec();
    if ours(c).is_none() {
        return call_original(c, addr);
    }
    let our_verts = m.slice(VERTS, 4 * VERTEX as usize).to_vec();
    let our_state = m.slice(STATE.0, STATE.1).to_vec();
    m.slice(STATE.0, STATE.1).copy_from_slice(&before);
    call_original(c, addr);
    let quads = QUADS.fetch_add(1, Relaxed) + 1;
    if addr == GREY_TRIANGLE {
        TRIANGLES.fetch_add(1, Relaxed);
    }
    let (verts, state) = (m.slice(VERTS, 4 * VERTEX as usize), m.slice(STATE.0, STATE.1));
    if (verts != our_verts.as_slice() || state != our_state.as_slice()) && BAD.fetch_add(1, Relaxed) < 20 {
        let at = (0..our_verts.len() / 4).find(|&i| verts[4 * i..4 * i + 4] != our_verts[4 * i..4 * i + 4]);
        super::log!(
            "render {addr:#x}: differs (quad {quads}): vertex dword {at:?} ours {:x?} original {:x?}, state ours {our_state:02x?} original {state:02x?}",
            at.map(|i| &our_verts[4 * i..4 * i + 4]),
            at.map(|i| &verts[4 * i..4 * i + 4])
        );
    }
    if quads % 200_000 == 0 {
        super::log!("render: {quads} checked ({} triangles), {} differ", TRIANGLES.load(Relaxed), BAD.load(Relaxed));
    }
}
