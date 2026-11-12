//! Pictures between the game's logic frames (as future_rust_cop's `fps.rs` does).
//!
//! The mission's logic runs 60 frames a second (`frames.rs`). The display may show more: in
//! the time before the next logic frame is due, only the drawing part of the mission loop
//! runs again and is shown. Every picture, the frame's own drawing too, shows the game a
//! little more than a frame late, between the last two logic frames: objects and the camera
//! are put where they were at that time (interpolated), drawn, and put back. So pictures move
//! smoothly at any rate, and the logic sees only its own places.
//!
//! The mission loop 0x46be9c draws while it runs its logic: the camera (StrkCam_Update_,
//! position `[0x503f54]`, yaw/pitch/roll `[0x503f64]` in 1/4096 turns, the matrices made from
//! them by Camera_SetYPR_ up to 0x503f8e, the sun in camera space `[0x503f2e]` from
//! Actor3D_UpdateSun_), the terrain (Rndr_TerrainRender_), the objects (ObjMgr_ProcessAllObj_:
//! per visible tile first 0x42c000, which moves them (Obj_Process_), then 0x42c018, which
//! draws them; the same for the list of objects updated at a high rate), the HUD
//! (HUDUserDraw_), and hands the ordering table to Glide (Order_DrawOT_); disp_FrameChange_ at
//! the start of the next frame shows it. The camera is interpolated only while drawing (the
//! hooks of the drawing functions swap it in and out), an object only while it is drawn
//! (0x42c018: its place, position `+0x2c` (3 x i32) and matrix `+0x38` (3x3 x i16), and those
//! of its children, `[+8]` the first, `[+0xc]` the next).
//!
//! A picture between frames runs these drawing steps alone; what they change besides the
//! picture is put back: the game's static data, and each drawn object's own memory (drawing
//! turns a rotor). The terrain is drawn from the data the frame's own drawing started from
//! (it prepares the next frame's as it draws: pictures from what it left smeared).

use super::frames;
use super::{guest_call, mem};
use crate::rt::{call_original, Cpu, EAX, EBX, ECX, EDX, ESP};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

pub const CAM_UPDATE: u32 = 0x46cd94;
pub const TERRAIN: u32 = 0x469a34;
pub const DRAW_OBJ: u32 = 0x42c018;
/// ObjMgr_ProcessAllObj_: the objects of the visible tiles moved and drawn (once a logic frame).
pub const PROCESS_ALL: u32 = 0x42c088;
pub const DEATH_DOT: u32 = 0x411b90;
pub const HUD_DRAW: u32 = 0x45eb9c;
pub const FRAME_CHANGE: u32 = 0x47adac;

const SET_POLY_BUFFER: u32 = 0x47aeb4;
const SET_ORDERING_TABLE: u32 = 0x410144;
const DRAW_OT: u32 = 0x47f5b4;
const AFTER_OT: u32 = 0x46b0e8;
const MSGBOX_ACTIVE: u32 = 0x49b85c;
const HUD_PAUSED: u32 = 0x45e7f4;
const UPDATE_SUN: u32 = 0x410310;

/// Where the mission loop calls disp_FrameChange_ (return address).
const LOOP_FRAME_CHANGE: u32 = 0x46c02f;

/// The ordering table, the player, the view's point on the ground.
const OT: u32 = 0x4edebc;
const PLAYER: u32 = 0x4e7ee0;
const VIEW_X: u32 = 0x4ed8f0;
const VIEW_Z: u32 = 0x4ed8f8;

/// The camera as drawing reads it: the sun in camera space; position, angles, matrices.
const CAM_RANGES: [(u32, u32); 2] = [(0x503f2e, 0x503f34), (0x503f54, 0x503f8e)];
const CAM_POS: u32 = 0x503f54;
const CAM_YPR: u32 = 0x503f64;
/// The camera's matrix (3x3 i16, 4.12, rows) made by Camera_SetYPR_; at 0x503f6a its transpose.
const CAM_MATRIX: u32 = 0x503f7c;

/// The game's static data (DGROUP and .bss): a picture's drawing changes much of it besides
/// the picture (the random generator, scratch matrices and results the logic reads later,
/// counters, HUD state...): all of it is put back after a picture. The game's Glide layer
/// keeps no state of the card there that drawing changes (only its settings).
const STATICS: (u32, u32) = (0x4d0000, 0x56c200);

/// Except the memory manager SSMem's (0x440c58...): its heaps' lists of blocks, whose records
/// lie in memory of their own. Drawing a shadow takes memory and gives it back; its lists put
/// back without their records broke the heap (an allocation got 0).
const STATICS_LEFT: [(u32, u32); 2] = [(0x4e8454, 0x4e8478), (0x51d990, 0x51dbe0)];

/// What Rndr_TerrainRender_ works on besides the camera: its data (0x51f000...0x523500: the
/// nine super tiles' states at 0x520510, 0x467 each; `[0x52050c]` the buffer it switches to
/// at every call), `[0x4ed61c]`, and two blocks it points to: `[0x52349c]` (a 0x100c-byte
/// record per super tile) and `[0x520504]` (0x94 bytes per super tile and buffer). The frame's
/// own drawing changes them for the next frame; a picture draws from what that drawing had.
fn terrain_ranges() -> Vec<(u32, u32)> {
    let m = mem();
    let (a, b) = (m.rd32(0x52349c), m.rd32(0x520504));
    let mut r = vec![(0x51f000, 0x523500), (0x4ed618, 0x4ed620)];
    if a != 0 {
        r.push((a, a + 9 * 0x100c));
    }
    if b != 0 {
        r.push((b, b + 9 * 2 * 0x94));
    }
    r
}

/// Of an object drawn in a picture, the memory put back afterwards (what drawing changed:
/// the object's own fields from its header on).
const OBJ_BEFORE: u32 = 0x20;
const OBJ_AFTER: u32 = 0x200;

/// ObjMgr's state: the visible tiles (rows [0x51b65c]..=[0x51b654], columns
/// [0x51b658]..=[0x51b650]; tile lists from 0x507e50, 0x600 a row, 0x18 a tile), the list of
/// objects updated at a high rate ([0x51b680]), its flags ([0x51b690]).
const ROW_LO: u32 = 0x51b65c;
const ROW_HI: u32 = 0x51b654;
const COL_LO: u32 = 0x51b658;
const COL_HI: u32 = 0x51b650;
const TILES: u32 = 0x507e50;
const HIGH_LIST: u32 = 0x51b680;
const OBJ_FLAGS: u32 = 0x51b690;

/// A place jumps (a new mission, a respawn, a camera cut) when it moves more than this in a
/// frame (world units: a tile is 2048): then it is not interpolated.
const JUMP: i64 = 4096;

/// An object's place: position (3 x i32) and matrix (9 x i16), 30 bytes from +0x2c.
const PLACE: u32 = 0x2c;
const PLACE_LEN: usize = 30;
type Place = [u8; PLACE_LEN];

/// The places of the last two logic frames.
#[derive(Clone, Copy)]
struct Hist<T> {
    frame: u64,
    cur: T,
    prev: Option<T>,
}

impl<T: Copy> Hist<T> {
    /// The place `v` at logic frame `f`.
    fn note(h: Option<&mut Self>, f: u64, v: T) -> Self {
        match h {
            Some(h) if h.frame == f => {
                h.cur = v;
                *h
            }
            Some(h) if h.frame + 1 == f => Hist { frame: f, cur: v, prev: Some(h.cur) },
            _ => Hist { frame: f, cur: v, prev: None },
        }
    }
}

/// Camera: position and angles; the angles as StrkCam_Update_ keeps them (16.16 in 1/4096
/// turns), and the point the camera is placed from with them (`base`, see `cam_offset`).
#[derive(Clone, Copy)]
struct Cam {
    pos: [i32; 3],
    fine: [i32; 3],
    base: [f64; 3],
    dist: f64,
    side: f64,
}

/// StrkCam_Update_'s angles (16.16, the parts above 1/4096 turn dropped for Camera_SetYPR_),
/// its distance behind the point looked at (`[0x523030]`, doubled in mode 5 `[0x523064]`) and
/// its sideways shift (`[0x523068]`).
const CAM_FINE: u32 = 0x52301c;
const CAM_DIST: u32 = 0x523030;
const CAM_MODE: u32 = 0x523064;
const CAM_SIDE: u32 = 0x523068;

/// sin and cos (x 4096) of an angle in 1/4096 turns.
fn sincos(a: f64) -> (f64, f64) {
    let (s, c) = (a * std::f64::consts::TAU / 4096.0).sin_cos();
    (s * 4096.0, c * 4096.0)
}

/// BuildRotMatrixYXZ_ (angles in 1/4096 turns, not cut to whole ones): the 3x3 matrix in
/// 4.12, rows.
fn rot_yxz(a: [f64; 3]) -> [f64; 9] {
    let ((s0, c0), (s1, c1), (s2, c2)) = (sincos(a[0]), sincos(a[1]), sincos(a[2]));
    let k = 1.0 / 4096.0;
    [
        (s1 * s0 * k * s2 + c1 * c2) * k,
        (s1 * s0 * k * c2 - c1 * s2) * k,
        s1 * c0 * k,
        s2 * c0 * k,
        c2 * c0 * k,
        -s0,
        (c1 * s0 * k * s2 - s1 * c2) * k,
        (c1 * s0 * k * c2 + s1 * s2) * k,
        c1 * c0 * k,
    ]
}

/// Where StrkCam_Update_ puts the camera from the point it follows: back by the distance
/// along the matrix's third column (Camera_Move_), then aside by `side` at the heading
/// (Camera_SetOrigin_).
fn cam_offset(m: &[f64; 9], heading: f64, dist: f64, side: f64) -> [f64; 3] {
    let (s, c) = sincos(heading);
    [(m[2] * dist - s * side) / 4096.0, m[5] * dist / 4096.0, (m[8] * dist - c * side) / 4096.0]
}

fn fine_angles(f: [i32; 3]) -> [f64; 3] {
    f.map(|v| v as f64 / 65536.0)
}

/// Fine angles the short way round (16.16, a turn is 1 << 28).
fn lerp_fine(a: i32, b: i32, alpha: u32) -> i32 {
    let d = ((b as i64 - a as i64 + (1 << 27)) & ((1 << 28) - 1)) - (1 << 27);
    (a as i64 + (d * alpha as i64 >> 8)) as i32
}

struct State {
    /// objects by address: their class (`[obj]`) and places
    objs: BTreeMap<u32, (u32, Hist<Place>)>,
    cam: Option<Hist<Cam>>,
    /// the camera block to draw with, for (logic frame, fraction)
    view: Option<(u64, u32, Vec<u8>)>,
    /// disp_FrameChange_'s argument in the mission loop
    frame_change: u32,
    /// the fraction of the frame being drawn (0...256), None: not interpolating
    alpha: Option<u32>,
    /// a picture between logic frames is being drawn
    in_picture: bool,
    /// the terrain's data before the frame's own drawing
    terrain: Option<(Vec<(u32, u32)>, Vec<u8>)>,
}

static STATE: Mutex<State> = Mutex::new(State { objs: BTreeMap::new(), cam: None, view: None, frame_change: 2, alpha: None, in_picture: false, terrain: None });

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Registers and x87 state around our own calls into guest code.
struct Saved {
    r: [u32; 8],
    flags: [u8; 6],
    st: [f64; 8],
    st_int: [i64; 8],
    st_int_of: [u64; 8],
    top: u32,
    fsw: u16,
    fcw: u16,
}

fn save(c: &Cpu) -> Saved {
    Saved {
        r: c.r,
        flags: [c.cf, c.pf, c.zf, c.sf, c.of, c.df],
        st: c.st,
        st_int: c.st_int,
        st_int_of: c.st_int_of,
        top: c.top,
        fsw: c.fsw,
        fcw: c.fcw,
    }
}

fn restore(c: &mut Cpu, s: &Saved) {
    c.r = s.r;
    [c.cf, c.pf, c.zf, c.sf, c.of, c.df] = s.flags;
    c.st = s.st;
    c.st_int = s.st_int;
    c.st_int_of = s.st_int_of;
    c.top = s.top;
    c.fsw = s.fsw;
    c.fcw = s.fcw;
}

/// Call a Watcom register-convention function (eax, edx, ebx, ecx); the caller's registers
/// are kept.
pub(super) fn call(c: &mut Cpu, addr: u32, args: [u32; 4]) -> u32 {
    let s = save(c);
    (c.r[EAX], c.r[EDX], c.r[EBX], c.r[ECX]) = (args[0], args[1], args[2], args[3]);
    let r = guest_call(c, addr, &[]);
    restore(c, &s);
    r
}

fn read(a: u32, n: usize) -> Vec<u8> {
    mem().slice(a, n).to_vec()
}

fn write(a: u32, b: &[u8]) {
    mem().slice(a, b.len()).copy_from_slice(b);
}

/// The ranges' bytes one after another (copied a range at a time: collected byte by byte
/// through an iterator, the terrain's ~54 KB took ~4% of the game thread).
fn ranges_read(rs: &[(u32, u32)]) -> Vec<u8> {
    let mut v = Vec::with_capacity(rs.iter().map(|&(a, b)| (b - a) as usize).sum());
    for &(a, b) in rs {
        v.extend_from_slice(mem().slice(a, (b - a) as usize));
    }
    v
}

fn ranges_write(rs: &[(u32, u32)], mut b: &[u8]) {
    for &(lo, hi) in rs {
        let n = (hi - lo) as usize;
        write(lo, &b[..n]);
        b = &b[n..];
    }
}

fn lerp(a: i64, b: i64, alpha: u32) -> i64 {
    a + ((b - a) * alpha as i64 >> 8)
}

/// The fraction (in 1/256) of the way from the last logic frame's places to the newest at
/// which a picture shown at `shown` is: a frame behind.
fn alpha_at(shown: Instant) -> Option<u32> {
    let (due, _) = frames::logic_frame()?;
    let d = shown.saturating_duration_since(due).as_nanos() * 256 / frames::FRAME.as_nanos();
    Some(d.min(256) as u32)
}

// ---- the camera ---------------------------------------------------------------------------------

/// StrkCam_Update_: the camera moved; its place for this logic frame.
pub fn cam_update(c: &mut Cpu) {
    call_original(c, CAM_UPDATE);
    let Some((_, f)) = frames::logic_frame() else { return };
    let m = mem();
    let pos = [0, 4, 8].map(|o| m.rd32(CAM_POS + o) as i32);
    let ypr = [0, 2, 4].map(|o| m.rd16(CAM_YPR + o));
    let fine = [0, 4, 8].map(|o| m.rd32(CAM_FINE + o) as i32);
    let dist = ((m.rd32(CAM_DIST) as i32) << (m.rd32(CAM_MODE) == 5) as u32) as f64;
    let side = m.rd32(CAM_SIDE) as i32 as f64;
    // the point followed: the camera less its offset at the whole angles it was placed with,
    // with the game's own matrix (ours differs by its rounding: 2/4096 times the distance)
    let game: [f64; 9] = std::array::from_fn(|k| m.rd16(CAM_MATRIX + 2 * k as u32) as i16 as f64);
    let off = cam_offset(&game, ypr[1] as f64, dist, side);
    let base: [f64; 3] = std::array::from_fn(|i| pos[i] as f64 - off[i]);
    let cam = Cam { pos, fine, base, dist, side };
    let mut s = state();
    let h = Hist::note(s.cam.as_mut(), f, cam);
    s.cam = Some(h);
    s.view = None;
}

/// The camera block for the fraction being drawn (made once per picture with the game's
/// own Camera_SetYPR_ and Actor3D_UpdateSun_), or None to draw with the camera as it is.
fn view(c: &mut Cpu) -> Option<Vec<u8>> {
    let s = state();
    let alpha = s.alpha?;
    let h = s.cam?;
    // also at the newest frame (alpha 256) and without the frame before: the camera made as
    // ours, not the game's (that one differs a little: things near it drew ~0.7% smaller
    // and shifted in such pictures, a flicker standing still)
    let alpha = alpha.min(256);
    let mut prev = h.prev.unwrap_or(h.cur);
    if let Some((f, a, v)) = &s.view {
        if *f == h.frame && *a == alpha {
            return Some(v.clone());
        }
    }
    if (0..3).any(|i| (h.cur.pos[i] as i64 - prev.pos[i] as i64).abs() > JUMP) {
        prev = h.cur;
    }
    drop(s);
    // The angles from StrkCam_Update_'s own, finer than the whole 1/4096 turns the logic's
    // camera is made with: those step unevenly (a turn went 7, 8, 6, 8 a frame), and the
    // camera placed with them swung around the point it follows (the picture shook in turns).
    let t = alpha as f64 / 256.0;
    let fine: [i32; 3] = std::array::from_fn(|i| lerp_fine(prev.fine[i], h.cur.fine[i], alpha));
    let a = fine_angles(fine);
    let rot = rot_yxz(a);
    let mix = |x: f64, y: f64| x + (y - x) * t;
    let off = cam_offset(&rot, a[1], mix(prev.dist, h.cur.dist), mix(prev.side, h.cur.side));
    let pos: [i32; 3] = std::array::from_fn(|i| (mix(prev.base[i], h.cur.base[i]) + off[i]).round() as i32);
    let actual = ranges_read(&CAM_RANGES);
    // as Camera_SetYPR_ makes it: angles, position, the matrix, its transpose with the
    // middle row negated
    let m = mem();
    for i in 0..3 {
        m.wr16(CAM_YPR + 2 * i as u32, ((fine[i] >> 16) & 0xfff) as u16);
        m.wr32(CAM_POS + 4 * i as u32, pos[i] as u32);
    }
    let q = rot.map(|v| v.round().clamp(-32768.0, 32767.0) as i16);
    for r in 0..3 {
        for k in 0..3 {
            m.wr16(CAM_MATRIX + 2 * (3 * r + k) as u32, q[3 * r + k] as u16);
            let v = if r == 1 { -q[3 * k + r] } else { q[3 * k + r] };
            m.wr16(0x503f6a + 2 * (3 * r + k) as u32, v as u16);
        }
    }
    call(c, UPDATE_SUN, [0; 4]);
    let v = ranges_read(&CAM_RANGES);
    ranges_write(&CAM_RANGES, &actual);
    state().view = Some((h.frame, alpha, v.clone()));
    Some(v)
}

/// Run a drawing function with the interpolated camera.
fn with_view(c: &mut Cpu, f: impl FnOnce(&mut Cpu)) {
    match view(c) {
        Some(v) => {
            let actual = ranges_read(&CAM_RANGES);
            ranges_write(&CAM_RANGES, &v);
            f(c);
            ranges_write(&CAM_RANGES, &actual);
        }
        None => f(c),
    }
}

pub fn terrain(c: &mut Cpu) {
    let in_picture = state().in_picture;
    if in_picture {
        // the data the frame's own drawing started from; afterwards the frame's again
        let Some((r, before)) = state().terrain.clone() else { return };
        let after = ranges_read(&r);
        ranges_write(&r, &before);
        with_view(c, |c| call_original(c, TERRAIN));
        ranges_write(&r, &after);
    } else {
        if frames::logic_frame().is_some() {
            let r = terrain_ranges();
            let before = ranges_read(&r);
            state().terrain = Some((r, before));
        }
        with_view(c, |c| call_original(c, TERRAIN));
    }
}

pub fn death_dot(c: &mut Cpu) {
    with_view(c, |c| call_original(c, DEATH_DOT));
}

pub fn hud_draw(c: &mut Cpu) {
    super::wide::hud(true);
    with_view(c, |c| call_original(c, HUD_DRAW));
    super::wide::hud(false);
}

// ---- objects ------------------------------------------------------------------------------------

/// An object and its children (`[+0xc]` the first, `[+8]` the next), as
/// Class_WalkHeirarchy_ walks them.
fn subtree(obj: u32, out: &mut Vec<u32>) {
    out.push(obj);
    let mut ch = mem().rd32(obj + 0xc);
    while ch != 0 && out.len() < 256 {
        subtree(ch, out);
        ch = mem().rd32(ch + 8);
    }
}

fn place_at(p: &Place, k: usize) -> i64 {
    i32::from_le_bytes(p[4 * k..4 * k + 4].try_into().unwrap()) as i64
}

fn matrix(p: &Place) -> [f64; 9] {
    std::array::from_fn(|k| i16::from_le_bytes([p[12 + 2 * k], p[13 + 2 * k]]) as f64 / 4096.0)
}

/// The lengths of a matrix's columns (the model's axes).
fn column_lengths(m: &[f64; 9]) -> [f64; 3] {
    std::array::from_fn(|c| (m[c] * m[c] + m[3 + c] * m[3 + c] + m[6 + c] * m[6 + c]).sqrt())
}

fn unit_columns(m: &[f64; 9], l: &[f64; 3]) -> [f64; 9] {
    std::array::from_fn(|k| m[k] / l[k % 3].max(1e-9))
}

/// Whether a matrix of unit columns is a rotation: columns at right angles (within the 4.12
/// format's error) and not mirrored.
fn orthogonal(u: &[f64; 9]) -> bool {
    let dot = |c: usize, d: usize| (0..3).map(|i| u[3 * i + c] * u[3 * i + d]).sum::<f64>().abs();
    let det = u[0] * (u[4] * u[8] - u[5] * u[7]) - u[1] * (u[3] * u[8] - u[5] * u[6]) + u[2] * (u[3] * u[7] - u[4] * u[6]);
    dot(0, 1) < 0.03 && dot(0, 2) < 0.03 && dot(1, 2) < 0.03 && det > 0.0
}

/// A rotation matrix's quaternion (w, x, y, z).
fn quat(m: &[f64; 9]) -> [f64; 4] {
    let (t, q);
    if m[8] < 0.0 {
        if m[0] > m[4] {
            t = 1.0 + m[0] - m[4] - m[8];
            q = [m[7] - m[5], t, m[1] + m[3], m[6] + m[2]];
        } else {
            t = 1.0 - m[0] + m[4] - m[8];
            q = [m[2] - m[6], m[1] + m[3], t, m[5] + m[7]];
        }
    } else if m[0] < -m[4] {
        t = 1.0 - m[0] - m[4] + m[8];
        q = [m[3] - m[1], m[6] + m[2], m[5] + m[7], t];
    } else {
        t = 1.0 + m[0] + m[4] + m[8];
        q = [t, m[7] - m[5], m[2] - m[6], m[3] - m[1]];
    }
    let n = 0.5 / t.sqrt();
    q.map(|v| v * n)
}

fn from_quat(q: [f64; 4]) -> [f64; 9] {
    let [w, x, y, z] = q;
    [
        1.0 - 2.0 * (y * y + z * z),
        2.0 * (x * y - w * z),
        2.0 * (x * z + w * y),
        2.0 * (x * y + w * z),
        1.0 - 2.0 * (x * x + z * z),
        2.0 * (y * z - w * x),
        2.0 * (x * z - w * y),
        2.0 * (y * z + w * x),
        1.0 - 2.0 * (x * x + y * y),
    ]
}

/// The place at `alpha` between `a` and `b`: positions in a line, the rotation the short way
/// (quaternions); None when it jumps, or turns more than this in a frame (a rotor: which
/// way it turned cannot be told).
const MAX_TURN_COS: f64 = 0.866; // cos 30°: the quaternions' angle is half the turn

fn mix(a: &Place, b: &Place, alpha: u32) -> Option<Place> {
    if (0..3).any(|k| (place_at(b, k) - place_at(a, k)).abs() > JUMP) {
        return None;
    }
    let mut out = *b;
    for k in 0..3 {
        let v = lerp(place_at(a, k), place_at(b, k), alpha) as i32;
        out[4 * k..4 * k + 4].copy_from_slice(&v.to_le_bytes());
    }
    let (ma, mb) = (matrix(a), matrix(b));
    if ma == mb {
        return Some(out);
    }
    // the game's matrices are rotations scaled along the model's axes: columns ~0.97..1.03
    // long, each its own. The rotation is taken from the columns made 1 long and their
    // lengths put back afterwards (as a rotation, a picture between two frames drew the
    // object 1-2% smaller; rows made 1 long, turned a little: a boat rocking flickered)
    let (la, lb) = (column_lengths(&ma), column_lengths(&mb));
    let (ua, ub) = (unit_columns(&ma, &la), unit_columns(&mb, &lb));
    if !(orthogonal(&ua) && orthogonal(&ub)) {
        // not a rotation scaled: element by element
        for k in 0..9 {
            let o = 12 + 2 * k;
            let (x, y) = (i16::from_le_bytes([a[o], a[o + 1]]), i16::from_le_bytes([b[o], b[o + 1]]));
            out[o..o + 2].copy_from_slice(&(lerp(x as i64, y as i64, alpha) as i16).to_le_bytes());
        }
    } else {
        let (qa, mut qb) = (quat(&ua), quat(&ub));
        let mut dot: f64 = (0..4).map(|i| qa[i] * qb[i]).sum();
        if dot < 0.0 {
            qb = qb.map(|v| -v);
            dot = -dot;
        }
        if dot < MAX_TURN_COS {
            return Some(out);
        }
        let t = alpha as f64 / 256.0;
        let q: [f64; 4] = std::array::from_fn(|i| qa[i] + (qb[i] - qa[i]) * t);
        let n = q.iter().map(|v| v * v).sum::<f64>().sqrt();
        let m = from_quat(q.map(|v| v / n));
        for k in 0..9 {
            let l = la[k % 3] + (lb[k % 3] - la[k % 3]) * t;
            let v = (m[k] * l * 4096.0).round().clamp(-32768.0, 32767.0) as i16;
            out[12 + 2 * k..14 + 2 * k].copy_from_slice(&v.to_le_bytes());
        }
    }
    Some(out)
}

/// 0x42c018 (eax object): ObjMgr's drawing of an object, with it and its children where they
/// were at the fraction drawn.
pub fn draw_obj(c: &mut Cpu) {
    let m = mem();
    let Some((_, f)) = frames::logic_frame() else { return call_original(c, DRAW_OBJ) };
    let mut nodes = Vec::new();
    subtree(c.r[EAX], &mut nodes);
    let mut put = Vec::new();
    let in_picture;
    {
        let mut s = state();
        let alpha = s.alpha;
        in_picture = s.in_picture;
        for &o in &nodes {
            let cur: Place = m.slice(o + PLACE, PLACE_LEN).try_into().unwrap();
            let class = m.rd32(o);
            // the place noted at the frame's first drawing only: drawing changes some (the
            // boat's draw turns its own matrix), drawn again from there the change came twice
            // (pictures at the newest frame drew the boat turned and smaller: a flicker)
            let e = s.objs.get_mut(&o).filter(|e| e.0 == class).map(|e| &mut e.1);
            let h = match e {
                Some(h) if h.frame == f => *h,
                e => {
                    let h = Hist::note(e, f, cur);
                    s.objs.insert(o, (class, h));
                    h
                }
            };
            if let Some(a) = alpha {
                let p = match h.prev {
                    Some(prev) if a < 256 => mix(&prev, &h.cur, a).unwrap_or(h.cur),
                    _ => h.cur,
                };
                if p != cur {
                    put.push((o, cur, p));
                }
            }
        }
    }
    // in a picture: the objects as they were (drawing changes some: a rotor turns)
    let kept: Vec<(u32, Vec<u8>)> =
        if in_picture { nodes.iter().map(|&o| (o - OBJ_BEFORE, read(o - OBJ_BEFORE, (OBJ_BEFORE + OBJ_AFTER) as usize))).collect() } else { Vec::new() };
    for (o, _, p) in &put {
        write(o + PLACE, p);
    }
    let was = super::wide::object(true);
    with_view(c, |c| call_original(c, DRAW_OBJ));
    super::wide::object(was);
    // back to the real place, unless drawing moved the object itself (a load on the hook
    // follows it there: put back, it never arrived, and the pick-up kept the controls)
    for (o, cur, p) in &put {
        if mem().slice(o + PLACE, PLACE_LEN) == &p[..] {
            write(o + PLACE, cur);
        }
    }
    for (a, b) in &kept {
        write(*a, b);
    }
}

// ---- pictures -----------------------------------------------------------------------------------

/// disp_FrameChange_ (eax: 1 or 2): the mission loop's argument is noted for pictures, and
/// the frame drawn next gets its fraction.
pub fn frame_change(c: &mut Cpu) {
    let from_loop = mem().rd32(c.r[ESP]) == LOOP_FRAME_CHANGE;
    if from_loop {
        state().frame_change = c.r[EAX];
    }
    call_original(c, FRAME_CHANGE);
    if from_loop {
        let a = alpha_at(frames::next_shown());
        let mut s = state();
        s.alpha = a;
        // forget objects not seen for a while
        if let Some((_, f)) = frames::logic_frame() {
            if f % 64 == 0 {
                s.objs.retain(|_, e| e.1.frame + 2 >= f);
            }
        }
    }
}

/// Whether pictures may be drawn now: playing, not paused, no map screen or message box.
fn drawing_allowed(c: &mut Cpu) -> bool {
    let m = mem();
    m.rd32(0x522f2c) == 0
        && m.rd32(0x54de38) == 0
        && m.rd32(0x4ed718) == 1
        && call(c, HUD_PAUSED, [0; 4]) & 0xff == 0
        && call(c, MSGBOX_ACTIVE, [0; 4]) == 0
}

/// One picture: the mission loop's drawing alone.
pub fn picture(c: &mut Cpu) {
    if !drawing_allowed(c) {
        return;
    }
    let m = mem();
    let arg = state().frame_change;
    // disp_FrameChange_ shows the last picture (its pace waits for the display, with the turn
    // handed over: other threads run); this one is shown at the next swap
    let t0 = std::time::Instant::now();
    call(c, FRAME_CHANGE, [arg, 0, 0, 0]);
    let t1 = std::time::Instant::now();
    // from here on the turn is held: what changes in the static data is the picture's
    super::wide::apply();
    // kept in one buffer for all pictures (a new one of 625 KB each time came fresh from the
    // system: page faults and system calls), brought up to date by the pages written since
    // the last picture (copying all of it there and back was ~3% of the game thread)
    thread_local! {
        static KEPT: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let mut kept = KEPT.take();
    let len = (STATICS.1 - STATICS.0) as usize;
    match mem().written(STATICS.0, STATICS.1) {
        Some(pages) if kept.len() == len => {
            for p in pages {
                let (a, b) = (p.max(STATICS.0), (p + 0x1000).min(STATICS.1));
                let at = (a - STATICS.0) as usize;
                kept[at..at + (b - a) as usize].copy_from_slice(mem().slice(a, (b - a) as usize));
            }
        }
        w => {
            if w.is_none() {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| super::log!("pictures: no write watch: the static data is copied whole"));
            }
            kept.clear();
            kept.extend_from_slice(mem().slice(STATICS.0, len));
        }
    }
    state().in_picture = true;
    // no other thread may run until it is put back (the audio timer's changes were lost)
    super::hold_turn(true);
    call(c, SET_POLY_BUFFER, [0; 4]);
    call(c, SET_ORDERING_TABLE, [m.rd32(OT), 0, 0, 0]);
    state().alpha = alpha_at(frames::next_shown());
    call(c, TERRAIN, [m.rd32(VIEW_X), m.rd32(VIEW_Z), (m.rd32(CAM_YPR) as i32 >> 16) as u32, 1]);
    call(c, SET_ORDERING_TABLE, [m.rd32(OT), 0, 0, 0]);
    // ObjMgr's drawing: the visible tiles, then the high-rate list
    let flags = m.rd8(OBJ_FLAGS);
    m.wr8(OBJ_FLAGS, flags | 1);
    let (r0, r1, c0, c1) = (m.rd32(ROW_LO) as i32, m.rd32(ROW_HI) as i32, m.rd32(COL_LO) as i32, m.rd32(COL_HI) as i32);
    for r in r0..=r1 {
        for q in c0..=c1 {
            let tile = TILES.wrapping_add((r * 0x600 + q * 0x18) as u32);
            draw_list(c, m.rd32(tile));
        }
    }
    m.wr8(OBJ_FLAGS, flags | 3);
    draw_list(c, m.rd32(HIGH_LIST));
    m.wr8(OBJ_FLAGS, flags);
    shadow_ring(c);
    call(c, DEATH_DOT, [0; 4]);
    call(c, HUD_DRAW, [m.rd32(PLAYER), 0, 0, 0]);
    call(c, DRAW_OT, [m.rd32(OT), 1, 0, 0]);
    m.wr16(0x522f34, 0);
    call(c, AFTER_OT, [0; 4]);
    for (lo, hi) in STATICS_LEFT {
        let at = (lo - STATICS.0) as usize;
        kept[at..at + (hi - lo) as usize].copy_from_slice(mem().slice(lo, (hi - lo) as usize));
    }
    match mem().written(STATICS.0, STATICS.1) {
        Some(pages) => {
            for p in pages {
                let (a, b) = (p.max(STATICS.0), (p + 0x1000).min(STATICS.1));
                let at = (a - STATICS.0) as usize;
                write(a, &kept[at..at + (b - a) as usize]);
            }
            // putting back writes too: the watch starts after it
            let _ = mem().written(STATICS.0, STATICS.1);
        }
        None => write(STATICS.0, &kept),
    }
    KEPT.set(kept);
    super::hold_turn(false);
    timing(t1 - t0, t1.elapsed());
    state().in_picture = false;
}

/// `NSTRIKE_FPS=1`: pictures a second, and the average time of their swap (with its wait
/// for the display and for the turn) and their drawing.
fn timing(swap: std::time::Duration, draw: std::time::Duration) {
    use std::time::Duration;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("NSTRIKE_FPS").is_some_and(|v| v != "0")) {
        return;
    }
    static T: Mutex<Option<(Instant, u32, Duration, Duration)>> = Mutex::new(None);
    let mut t = T.lock().unwrap();
    let (start, n, s, d) = t.get_or_insert((Instant::now(), 0, Duration::ZERO, Duration::ZERO));
    *n += 1;
    *s += swap;
    *d += draw;
    if start.elapsed() >= Duration::from_secs(1) {
        super::log!("pictures {} a second, swap {:.2} ms, drawing {:.2} ms", n, s.as_secs_f64() * 1000.0 / *n as f64, d.as_secs_f64() * 1000.0 / *n as f64);
        *t = None;
    }
}

/// The objects of a list (nodes: `[node]` the next, the object at +8).
/// ObjMgr_ProcessAllObj_, then the shadows' object ring.
pub fn process_all(c: &mut Cpu) {
    call_original(c, PROCESS_ALL);
    shadow_ring(c);
}

/// The projection's zoom for the ring: a tile (2048 units) around a picture ~5700 wide fits.
const RING_ZOOM: (f32, f32) = (2.0, 2.0);

/// The shadows' objects: all objects of the visible tiles, the tiles around them and the
/// high-rate list are drawn again with the projection zoomed out (the game leaves out what is
/// off its screen, whole objects and an object's triangles); these primitives only cast
/// shadows (`wide::casting`, `glide::set_cast_only`), and the objects' primitives in the
/// picture do not (`wide::object`, `glide::set_no_cast`): a shadow no longer pops when its
/// object reaches the screen's edge, and trees and vehicles just outside cast into it. Drawing an object changes it (a rotor turns): it is
/// put back afterwards, as in a picture (`draw_obj`).
fn shadow_ring(c: &mut Cpu) {
    if !super::shadows::ring() {
        return;
    }
    let m = mem();
    let (r0, r1, c0, c1) = (m.rd32(ROW_LO) as i32, m.rd32(ROW_HI) as i32, m.rd32(COL_LO) as i32, m.rd32(COL_HI) as i32);
    if r0 > r1 || c0 > c1 {
        return;
    }
    super::wide::zoom_out(RING_ZOOM);
    super::glide::set_ring_zoom(RING_ZOOM);
    super::wide::casting(true);
    let was = std::mem::replace(&mut state().in_picture, true);
    let flags = m.rd8(OBJ_FLAGS);
    m.wr8(OBJ_FLAGS, flags | 1);
    for r in (r0 - 1).max(0)..=(r1 + 1).min(63) {
        for q in (c0 - 1).max(0)..=(c1 + 1).min(63) {
            draw_list(c, m.rd32(TILES.wrapping_add((r * 0x600 + q * 0x18) as u32)));
        }
    }
    m.wr8(OBJ_FLAGS, flags | 3);
    draw_list(c, m.rd32(HIGH_LIST));
    m.wr8(OBJ_FLAGS, flags);
    state().in_picture = was;
    super::wide::casting(false);
    super::wide::apply();
}

fn draw_list(c: &mut Cpu, mut node: u32) {
    let m = mem();
    let mut n = 0;
    while node != 0 && m.rd32(node) != 0 && n < 10_000 {
        let obj = node + 8;
        let next = m.rd32(node);
        call(c, DRAW_OBJ, [obj, 0, 0, 0]);
        node = next;
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rotation about an axis (unit) by an angle.
    fn rot(ax: [f64; 3], a: f64) -> [f64; 9] {
        let (s, c) = a.sin_cos();
        let [x, y, z] = ax;
        let t = 1.0 - c;
        [
            t * x * x + c,
            t * x * y - s * z,
            t * x * z + s * y,
            t * x * y + s * z,
            t * y * y + c,
            t * y * z - s * x,
            t * x * z - s * y,
            t * y * z + s * x,
            t * z * z + c,
        ]
    }

    #[test]
    fn quaternion_round_trip() {
        let n = (1.0f64 + 4.0 + 9.0).sqrt();
        for ax in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0 / n, 2.0 / n, -3.0 / n]] {
            for k in 0..24 {
                let m = rot(ax, k as f64 * 0.27);
                let back = from_quat(quat(&m));
                for i in 0..9 {
                    assert!((m[i] - back[i]).abs() < 1e-9, "{ax:?} {k}: {m:?} {back:?}");
                }
            }
        }
    }
}
