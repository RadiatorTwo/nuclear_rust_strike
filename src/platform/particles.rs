//! Our own particles: sparks and debris fly from the game's explosions.
//!
//! Sources: every pyro (the game's effects) is made by 0x42ff50 (`PYRO_NEW`, hooked; called by
//! pyroCreate_ and newPyro_) from a block: +0 the sequence (byte; a splash on water turns it
//! into 9..0xb there), +4 flags, +8/+0xc/+0x10 the place x, y (up), z (in 1/256 units), +0x20
//! the size (at most 0x200), +0x24 an object to stand at (flag 0x20). It returns 0 when none
//! was made (else not the object's address). A pyro of the sequences in `SOURCES` (fire and
//! explosions, as the dynamic lights') gives sparks and debris, by its size.
//!
//! Each particle flies in a parabola from its start: its place is worked out for the time of
//! the picture being drawn (`pictures::shown_frame`, in logic frames), not stepped: pictures
//! between logic frames move them smoothly, and a pause stops them. Debris rests where it
//! reaches the ground's height under the explosion (Heights_GetTerrainHeight_); sparks end
//! there. They are drawn by `glide`'s `post_3d` before its passes as quads of the 3D picture
//! with their depth (`ao::view`: the camera of the picture's terrain projection): sparks as
//! short additive streaks along their path, debris as small dark turning squares. The depth
//! buffer hides them behind things, the bloom takes the sparks.
//!
//! Setting `particles` (F11, `NSTRIKE_PARTICLES`); `NSTRIKE_PARTICLE_LOG=1` logs the pyros made
//! (sequence, size, flags).

use super::mem;
use crate::rt::{Cpu, EAX, call_original};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// Makes a pyro from the block at eax; returns it (0: none made).
pub const PYRO_NEW: u32 = 0x42ff50;
/// Heights_GetTerrainHeight_(eax x, edx z, ebx 0): the ground's height there.
const TERRAIN_HEIGHT: u32 = 0x45c4fc;

/// The pyro sequences that give particles (fire and explosions).
const SOURCES: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7, 0x15];
/// The most particles at once.
const MAX: usize = 3000;
/// Below this depth a particle is not drawn (the depth buffer's near end is at 100).
const NEAR: f32 = 150.0;

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn init() {
    ENABLED.store(crate::settings::get().particles, Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// The F11 menu: saved, used from the next picture on.
pub fn set_enabled(on: bool) {
    crate::settings::update(|s| s.particles = on);
    ENABLED.store(on, Relaxed);
    if !on {
        LIVE.lock().unwrap().list.clear();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Spark,
    Debris,
}

/// A particle: start (units, y up), speed (units a frame), gravity (units a frame²), the
/// frame it starts at and its life in frames, the ground's height, its size (units), its
/// colour and turn (debris).
#[derive(Clone, Copy)]
struct Particle {
    kind: Kind,
    p0: [f32; 3],
    v: [f32; 3],
    g: f32,
    born: f64,
    life: f32,
    ground: f32,
    size: f32,
    rgb: [f32; 3],
    spin: (f32, f32),
}

impl Particle {
    /// When it reaches the ground (frames after its start).
    fn landing(&self) -> f32 {
        let h = (self.p0[1] - self.ground).max(0.0);
        (self.v[1] + (self.v[1] * self.v[1] + 2.0 * self.g * h).sqrt()) / self.g
    }

    /// Its place `t` frames after its start (resting on the ground after landing).
    fn at(&self, t: f32) -> [f32; 3] {
        let t = t.min(self.landing());
        [self.p0[0] + self.v[0] * t, (self.p0[1] + self.v[1] * t - 0.5 * self.g * t * t).max(self.ground), self.p0[2] + self.v[2] * t]
    }
}

struct Live {
    list: Vec<Particle>,
    rng: u32,
    /// the places that gave particles in the newest frame (an explosion is made of several
    /// pyros: one gives them)
    recent: Vec<(f64, [f32; 3])>,
    /// the explosions (debris given) of the last `AFTER` frames
    blasts: Vec<(f64, [f32; 3])>,
}

static LIVE: Mutex<Live> = Mutex::new(Live { list: Vec::new(), rng: 0x1234_5678, recent: Vec::new(), blasts: Vec::new() });

/// Pyros nearer than this (units) in one frame are one explosion.
const SAME: f32 = 400.0;
/// Pyros within `NEAR_BLAST` units of an explosion of the last `AFTER` frames (a wreck flying
/// burning makes them all the way) give one spark, no debris, and count as one themselves
/// (the blast follows the wreck).
const AFTER: f64 = 45.0;
const NEAR_BLAST: f32 = 1500.0;

impl Live {
    /// 0..1
    fn rand(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }

    fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.rand()
    }

    /// A direction of the upper half, at least `up` upward (0..1).
    fn dir(&mut self, up: f32) -> [f32; 3] {
        let a = self.range(0.0, std::f32::consts::TAU);
        let y = self.range(up, 1.0);
        let r = (1.0 - y * y).max(0.0).sqrt();
        [r * a.cos(), y, r * a.sin()]
    }
}

fn logging() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NSTRIKE_PARTICLE_LOG").is_some_and(|v| v != "0"))
}

/// Hook of 0x42ff50: an explosion or fire made gives particles.
pub fn pyro_new(c: &mut Cpu) {
    let block = c.r[EAX];
    call_original(c, PYRO_NEW);
    let pyro = c.r[EAX];
    if pyro == 0 || !enabled() || super::pictures::in_picture() {
        return;
    }
    let Some(now) = super::frames::logic_frame().map(|(_, f)| f as f64) else { return };
    let m = mem();
    let (seq, flags, size) = (m.rd8(block), m.rd16(block + 4), m.rd32(block + 0x20) as i32);
    if logging() {
        super::log!("particles: pyro {seq:#x} size {size:#x} flags {flags:#x}");
    }
    if !SOURCES.contains(&seq) {
        return;
    }
    // at an object's place (flag 0x20): not used by explosions seen so far
    if flags & 0x20 != 0 {
        return;
    }
    // the block's place (y raised to the ground by the original)
    let p = [8, 0xc, 0x10].map(|o| m.rd32(block + o) as i32 >> 8);
    let ground = super::pictures::call(c, TERRAIN_HEIGHT, [p[0] as u32, p[2] as u32, 0, 0]) as i32;
    spawn(now, p.map(|v| v as f32), ground as f32, (size as f32 / 256.0).clamp(0.25, 2.0));
}

fn spawn(now: f64, p: [f32; 3], ground: f32, s: f32) {
    let mut l = LIVE.lock().unwrap();
    // a mission begun anew: its frames start again
    if l.list.first().is_some_and(|q| q.born > now + 1.0) {
        l.list.clear();
        l.blasts.clear();
    }
    l.list.retain(|q| now - q.born < q.life as f64);
    l.recent.retain(|r| r.0 == now);
    if l.recent.iter().any(|r| (0..3).map(|i| (r.1[i] - p[i]).powi(2)).sum::<f32>() < SAME * SAME) {
        return;
    }
    l.recent.push((now, p));
    let dist2 = |a: [f32; 3]| (0..3).map(|i| (a[i] - p[i]).powi(2)).sum::<f32>();
    l.blasts.retain(|b| now - b.0 < AFTER && b.0 <= now);
    let after_blast = l.blasts.iter().any(|b| dist2(b.1) < NEAR_BLAST * NEAR_BLAST);
    // hits (smaller than 1): a few sparks; explosions: more, and debris; what follows one: a spark
    let (sparks, debris) = if after_blast {
        // the wreck keeps burning: the blast follows it
        l.blasts.push((now, p));
        (1, 0)
    } else if s < 1.0 {
        (5, 0)
    } else {
        l.blasts.push((now, p));
        ((24.0 * s) as usize, (9.0 * s) as usize)
    };
    for k in 0..sparks + debris {
        if l.list.len() >= MAX {
            break;
        }
        let spark = k < sparks;
        let q = if spark {
            let d = l.dir(-0.2);
            let speed = l.range(5.0, 16.0) * s.sqrt();
            let heat = l.range(0.0, 1.0);
            Particle {
                kind: Kind::Spark,
                p0: p,
                v: d.map(|d| d * speed),
                g: 0.35,
                born: now + l.range(0.0, 3.0) as f64,
                life: l.range(25.0, 60.0),
                ground,
                size: l.range(5.0, 9.0),
                rgb: [255.0, 190.0 + 60.0 * heat, 80.0 + 120.0 * heat],
                spin: (0.0, 0.0),
            }
        } else {
            let d = l.dir(0.35);
            let speed = l.range(4.0, 11.0) * s.sqrt();
            let shade = l.range(25.0, 70.0);
            Particle {
                kind: Kind::Debris,
                p0: p,
                v: d.map(|d| d * speed),
                g: 0.45,
                born: now,
                life: l.range(100.0, 170.0),
                ground,
                size: l.range(14.0, 30.0) * s.sqrt(),
                rgb: [shade * 1.1, shade, shade * 0.85],
                spin: (l.range(0.0, std::f32::consts::TAU), l.range(-0.4, 0.4)),
            }
        };
        l.list.push(q);
    }
}

/// A quad of ours for `glide`: corners (game pixels, the Glide layer's), their depth, colour
/// (0..255 floats), additive or alpha-blended.
pub struct Quad {
    pub xy: [(f32, f32); 4],
    pub z: [f32; 4],
    pub rgba: [f32; 4],
    pub additive: bool,
}

/// The quads of the picture being drawn.
pub fn quads() -> Vec<Quad> {
    let (Some(t), Some(v)) = (super::pictures::shown_frame(), super::ao::view()) else { return Vec::new() };
    let l = LIVE.lock().unwrap();
    let mut out = Vec::new();
    // into the camera's space, onto the screen
    let cam = v.cam.map(|c| c as f32);
    let view = |p: [f32; 3]| {
        let d = [p[0] - cam[0], p[1] - cam[1], p[2] - cam[2]];
        let r = &v.rot;
        [r[0] * d[0] + r[1] * d[1] + r[2] * d[2], r[3] * d[0] + r[4] * d[1] + r[5] * d[2], r[6] * d[0] + r[7] * d[1] + r[8] * d[2]]
    };
    let screen = |c: [f32; 3]| (c[0] / c[2] * v.focal.0 + v.centre.0, c[1] / c[2] * v.focal.1 + v.centre.1);
    for q in &l.list {
        let age = (t - q.born) as f32;
        if age < 0.0 || age >= q.life {
            continue;
        }
        let fade = ((q.life - age) / 20.0).min(1.0);
        match q.kind {
            Kind::Spark => {
                if age > q.landing() {
                    continue;
                }
                let (a, b) = (view(q.at((age - 1.5).max(0.0))), view(q.at(age)));
                if a[2] < NEAR || b[2] < NEAR {
                    continue;
                }
                let (pa, pb) = (screen(a), screen(b));
                // at least 1.2 pixels wide
                let w = (q.size * v.focal.0 / b[2]).max(1.2) * 0.5;
                let (dx, dy) = (pb.0 - pa.0, pb.1 - pa.1);
                let n = (dx * dx + dy * dy).sqrt();
                let (ux, uy) = if n > 0.01 { (dx / n, dy / n) } else { (1.0, 0.0) };
                // the head a little beyond the place, the tail at the earlier one
                let (hx, hy) = (pb.0 + ux * w, pb.1 + uy * w);
                let (nx, ny) = (-uy * w, ux * w);
                let cool = (age / q.life).min(1.0);
                let rgb = [q.rgb[0], q.rgb[1] * (1.0 - 0.6 * cool), q.rgb[2] * (1.0 - cool)];
                out.push(Quad {
                    xy: [(pa.0 + nx, pa.1 + ny), (hx + nx, hy + ny), (hx - nx, hy - ny), (pa.0 - nx, pa.1 - ny)],
                    z: [a[2], b[2], b[2], a[2]],
                    rgba: [rgb[0], rgb[1], rgb[2], 255.0 * fade],
                    additive: true,
                });
            }
            Kind::Debris => {
                let c = view(q.at(age));
                if c[2] < NEAR {
                    continue;
                }
                let (x, y) = screen(c);
                let r = (q.size * v.focal.0 / c[2]).max(1.0) * 0.5;
                let turn = q.spin.0 + q.spin.1 * age.min(q.landing());
                let (s, k) = turn.sin_cos();
                // a square a little flattened, turning
                let corner = |i: usize| {
                    let (a, b) = [(-1.0, -0.6), (1.0, -0.6), (1.0, 0.6), (-1.0, 0.6)][i];
                    (x + (a * k - b * s) * r, y + (a * s + b * k) * r)
                };
                out.push(Quad {
                    xy: std::array::from_fn(corner),
                    z: [c[2]; 4],
                    rgba: [q.rgb[0], q.rgb[1], q.rgb[2], 255.0 * fade],
                    additive: false,
                });
            }
        }
    }
    out
}
