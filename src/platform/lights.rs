//! Dynamic lights in missions, as future_rust_cop's `lights.rs`: what glows lights its
//! surroundings. Muzzle flashes, shots and explosions colour the ground, trees and buildings
//! near them as they come and go.
//!
//! Sources: the game's effects are pyros (`pyroCreate_`...; sequences from PYROS.PAK, the same
//! in every level), all drawn by 0x430970 (`PYRO_DRAW`, hooked: the sequence at pyro +0x38).
//! disp_AllocPoly_ (`wide::alloc_poly`) notes for each primitive who made it (`made`: the
//! pyro's sequence, else the return address), gl_RenderPoly_ which is drawn (`drawing`). An
//! additive primitive of the 3D picture (blend SRC_ALPHA or ONE, ONE) from a pyro sequence in
//! `LIGHT_PYROS` (fire, explosions; not smoke, a boat's wake, water spray: told apart by colour
//! they were not) is a light (`glide`'s `add` gives `light`): at its centre and depth (`ao.rs`:
//! the projected points, for a sprite the nearest one, `ao::depth_near`), its colour the
//! primitive's (the mean of its texture rectangle times its vertex or constant colour and
//! alpha, by the alpha combine), its strength and reach by its size in the world. Lights near
//! each other and of a like colour are merged, the strongest `MAX` kept.
//!
//! Applied after the AO (`glide`'s `post_3d`): per pixel the sum of the lights within reach
//! (falling off smoothly to 0 at it, by the angle to the surface's normal from the depth, less
//! at the light's core), the picture times 1 + that sum plus a little of it (dark surfaces got
//! nearly nothing from the factor alone), not where the mask says something glowing or
//! transparent was drawn.
//!
//! Setting `dynamic_lights` (F11), `NSTRIKE_LIGHTS=0/1`; `NSTRIKE_GBUFFER=lights` shows the
//! light alone; tuning `NSTRIKE_LIGHT_STRENGTH`, `NSTRIKE_LIGHT_REACH`; `NSTRIKE_LIGHT_PYROS`
//! (hex list) replaces `LIGHT_PYROS`, `NSTRIKE_LIGHT_LOG=1` logs the glows by sequence.

use super::ao;
use glow::HasContext;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// At start: the setting.
pub fn init() {
    ENABLED.store(crate::settings::get().dynamic_lights, Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// The F11 menu: saved, used from the next picture on.
pub fn set_enabled(on: bool) {
    crate::settings::update(|s| s.dynamic_lights = on);
    ENABLED.store(on, Relaxed);
}

thread_local! {
    /// who made each primitive (disp_AllocPoly_'s return address), and the one drawn now
    static MADE: std::cell::RefCell<std::collections::HashMap<u32, u32>> = Default::default();
    static SOURCE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Draws a pyro (the game's effects: fire, explosions, water spray...): eax the pyro, its
/// sequence (the effect's kind) at +0x38.
pub const PYRO_DRAW: u32 = 0x430970;

thread_local! {
    static PYRO: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

pub fn pyro_draw(c: &mut crate::rt::Cpu) {
    PYRO.set(Some(super::mem().rd32(c.r[crate::rt::EAX] + 0x38)));
    crate::rt::call_original(c, PYRO_DRAW);
    PYRO.set(None);
}

/// disp_AllocPoly_ gave primitive `p` to the code at `ret` (a pyro's: its sequence | 1 << 31).
pub fn made(p: u32, ret: u32) {
    if p != 0 {
        let tag = PYRO.get().map_or(ret, |s| s | 1 << 31);
        MADE.with_borrow_mut(|m| m.insert(p, tag));
    }
}

/// gl_RenderPoly_ draws primitive `p`.
pub fn drawing(p: u32) {
    SOURCE.set(MADE.with_borrow(|m| m.get(&p).copied().unwrap_or(0)));
}

/// Where the primitive drawn now was made (0 unknown).
pub fn source() -> u32 {
    SOURCE.get()
}

/// The pyro sequences that give light (fire, explosions; not smoke, dust or water spray).
/// `NSTRIKE_LIGHT_PYROS=0,1,15` (hex) replaces the list.
/// Seen in mission 1: 0-7 and 0x15 fire and explosions (1 a burning wreck), 0x18/0x19 grey smoke
/// or dust, 0x1d a boat's wake, 0x1e water spray from shots.
const LIGHT_PYROS: &[u32] = &[0, 1, 2, 3, 4, 5, 6, 7, 0x15];

/// The primitive drawn now gives light: a pyro of a sequence in the list.
pub fn gives_light() -> bool {
    static LIST: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    let list = LIST.get_or_init(|| match std::env::var("NSTRIKE_LIGHT_PYROS") {
        Ok(v) => v.split(',').filter_map(|n| u32::from_str_radix(n.trim(), 16).ok()).collect(),
        Err(_) => LIGHT_PYROS.to_vec(),
    });
    let s = source();
    s >> 31 == 1 && list.contains(&(s & 0x7fff_ffff))
}

/// `NSTRIKE_LIGHT_LOG=1`: the glows (additive primitives) by where they were made (1 << 31 | a pyro
/// sequence), with their mean colour, every 2 s.
pub fn log_source(colour: [f32; 3]) {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static LOG: Mutex<Option<(std::time::Instant, HashMap<u32, (u32, [f32; 3])>)>> = Mutex::new(None);
    let mut l = LOG.lock().unwrap();
    let (t, m) = l.get_or_insert_with(|| (std::time::Instant::now(), HashMap::new()));
    let e = m.entry(source()).or_insert((0, [0.0; 3]));
    e.0 += 1;
    for i in 0..3 {
        e.1[i] += colour[i];
    }
    if t.elapsed().as_secs_f32() > 2.0 {
        let mut v: Vec<_> = m.drain().collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.1 .0));
        for (src, (n, c)) in v {
            eprintln!("additive from {src:#x}: {n}, mean colour {:.2} {:.2} {:.2}", c[0] / n as f32, c[1] / n as f32, c[2] / n as f32);
        }
        eprintln!("--");
        *t = std::time::Instant::now();
    }
}

/// The most lights applied to a picture.
const MAX: usize = 32;
/// The picture times 1 + this much of the light.
const STRENGTH: f32 = 1.6;
/// And plus this much of it, as if every surface were at least this bright.
const ADDED: f32 = 0.12;
/// A light's reach: this many times its size, within the bounds (object units; a tree ~700).
const REACH: f32 = 4.0;
const REACH_MIN: f32 = 300.0;
const REACH_MAX: f32 = 2000.0;

/// A light: where (view space, as the shaders' `P`), its colour (0...1) and its size in the
/// world (object units).
#[derive(Clone, Copy, Debug)]
pub struct Light {
    pub at: [f32; 3],
    pub colour: [f32; 3],
    pub size: f32,
}

/// A light from a glowing primitive: its corners (target pixels from the bottom left) and z
/// (0 unknown), its colour; `cam` the camera (`ao::camera`: focal x, y and centre).
pub fn light(corners: &[(f32, f32)], z: &[f32], colour: [f32; 3], cam: ((f32, f32), (f32, f32))) -> Option<Light> {
    let (n, sum) = z.iter().filter(|&&z| z > 0.0).fold((0, 0.0), |a, &z| (a.0 + 1, a.1 + z));
    if n == 0 || colour.iter().all(|&c| c < 0.02) {
        return None;
    }
    let z = sum / n as f32;
    let k = corners.len() as f32;
    let c = corners.iter().fold((0.0, 0.0), |a, p| (a.0 + p.0 / k, a.1 + p.1 / k));
    // its extent's diagonal in pixels, in the world at its depth
    let (x0, x1) = corners.iter().fold((f32::MAX, f32::MIN), |a, p| (a.0.min(p.0), a.1.max(p.0)));
    let (y0, y1) = corners.iter().fold((f32::MAX, f32::MIN), |a, p| (a.0.min(p.1), a.1.max(p.1)));
    let ((fx, fy), (cx, cy)) = cam;
    let size = (((x1 - x0) * z / fx).hypot((y1 - y0) * z / fy)).max(1.0);
    Some(Light { at: [(c.0 - cx) * z / fx, (c.1 - cy) * z / fy, z], colour, size })
}

/// A light as the shader takes it: place (pulled toward the camera by a fifth of its reach:
/// a glow lies on what it lights), reach, colour times strength.
fn view(l: &Light) -> ([f32; 3], f32, [f32; 3]) {
    let reach = (l.size * ao::tune("NSTRIKE_LIGHT_REACH", REACH)).clamp(REACH_MIN, REACH_MAX);
    let z = l.at[2];
    let k = (z - reach * 0.2).max(z * 0.5) / z;
    // small ones (sparks) weaker, large ones (explosions) up to 1.2
    let mut s = (l.size / 150.0).clamp(0.25, 1.2);
    // whitish ones (muzzle flashes) weaker: they brightened every channel alike
    let (lo, hi) = l.colour.iter().fold((f32::MAX, 0.0f32), |a, &c| (a.0.min(c), a.1.max(c)));
    if hi > 0.0 {
        s *= 1.0 - 0.45 * (lo / hi).powi(2);
    }
    (l.at.map(|c| c * k), reach, l.colour.map(|c| c * s))
}

/// Lights near each other and of a like colour as one (summed colour, their place weighed by
/// it), the strongest `MAX` (by colour times reach).
fn merge(lights: &[Light]) -> Vec<([f32; 3], f32, [f32; 3])> {
    let mut v: Vec<_> = lights.iter().map(view).collect();
    let power = |l: &([f32; 3], f32, [f32; 3])| (l.2[0] + l.2[1] + l.2[2]) * l.1;
    v.sort_by(|a, b| power(b).total_cmp(&power(a)));
    let mut out: Vec<([f32; 3], f32, [f32; 3])> = Vec::new();
    for l in v {
        let near = out.iter().position(|o| {
            let d = (0..3).map(|i| (o.0[i] - l.0[i]).powi(2)).sum::<f32>().sqrt();
            d < 0.4 * o.1.max(l.1) && alike(o.2, l.2)
        });
        match near {
            Some(k) => {
                let o = &mut out[k];
                let (wa, wb) = (o.2.iter().sum::<f32>(), l.2.iter().sum::<f32>());
                let w = wb / (wa + wb).max(1e-6);
                for i in 0..3 {
                    o.0[i] += (l.0[i] - o.0[i]) * w;
                    o.2[i] = (o.2[i] + l.2[i]).min(2.5);
                }
                o.1 = o.1.max(l.1);
            }
            None if out.len() < MAX => out.push(l),
            None => {}
        }
    }
    out
}

/// Two colours of nearly the same hue.
fn alike(a: [f32; 3], b: [f32; 3]) -> bool {
    let n = |c: [f32; 3]| (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt().max(1e-6);
    (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]) / (n(a) * n(b)) > 0.9
}

const LIGHT_FS: &str = r#"
const int MAX = 32;
uniform int count;
uniform vec4 lpos[MAX];   // view space, reach
uniform vec3 lcol[MAX];
uniform float strength;
uniform float added;      // this much of the light is added (dark surfaces get some too)
out vec4 o;
void main() {
    ivec2 q = ivec2(gl_FragCoord.xy);
    vec2 g = G(q);
    float z = g.r;
    vec3 sum = vec3(0.0);
    if (z > 0.0) {
        vec3 p = P(vec2(q), z);
        vec3 n = N(q, z, p, 1);
        for (int i = 0; i < count; i++) {
            vec3 l = lpos[i].xyz - p;
            float d = length(l);
            float r = lpos[i].w;
            if (d >= r) continue;
            float a = 1.0 - d / r;
            // less at its core: what the glow sits on is not flooded
            a *= a * mix(0.3, 1.0, smoothstep(0.0, 0.2 * r, d));
            // mostly by the angle, a little all round (the normals are rough)
            float ndl = d > 0.0 ? max(dot(n, l / d), 0.0) : 1.0;
            sum += lcol[i] * a * (0.2 + 0.8 * ndl);
        }
        sum *= strength * (1.0 - clamp(g.g, 0.0, 1.0));
    }
    o = vec4(sum * added, 1.0);
}
"#;

/// The pass's programs (single-sampled and multisampled depth). Made and used on the game's
/// thread.
pub struct Lighting {
    progs: [glow::Program; 2],
    vao: glow::VertexArray,
}

impl Lighting {
    pub fn new(gl: &glow::Context) -> Lighting {
        let prog = |ms: bool| super::gl::compile(gl, ao::VS, &format!("{}{}{LIGHT_FS}", if ms { "#define MS\n" } else { "" }, ao::COMMON));
        let vao = unsafe { gl.create_vertex_array().expect("GL vertex array") };
        Lighting { progs: [prog(false), prog(true)], vao }
    }

    pub fn delete(&mut self, gl: &glow::Context) {
        unsafe {
            for p in self.progs {
                super::gl::delete_program(gl, p);
            }
            gl.delete_vertex_array(self.vao);
        }
    }

    /// The lights over the 3D picture: `depth` the attachment, `target` its size, `cam` the
    /// camera in target pixels, `out` a framebuffer with only the picture's colour.
    pub fn run(&mut self, gl: &glow::Context, depth: glow::Texture, ms: bool, target: (u32, u32), cam: ((f32, f32), (f32, f32)), out: glow::Framebuffer, lights: &[Light]) {
        let show = ao::debug_view();
        if lights.is_empty() && show != 3 || matches!(show, 1 | 2) {
            return;
        }
        let (w, h) = (target.0 as i32, target.1 as i32);
        let merged = merge(lights);
        let pos: Vec<f32> = merged.iter().flat_map(|l| [l.0[0], l.0[1], l.0[2], l.1]).collect();
        let col: Vec<f32> = merged.iter().flat_map(|l| l.2).collect();
        let ((fx, fy), (cx, cy)) = cam;
        unsafe {
            let p = self.progs[ms as usize];
            gl.use_program(Some(p));
            let u = |n: &'static str| super::gl::uniform(gl, p, n);
            let kind = if ms { glow::TEXTURE_2D_MULTISAMPLE } else { glow::TEXTURE_2D };
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(kind, Some(depth));
            gl.uniform_1_i32(u("gdepth").as_ref(), 0);
            gl.uniform_2_i32(u("size").as_ref(), w, h);
            gl.uniform_2_f32(u("focal").as_ref(), fx, fy);
            gl.uniform_2_f32(u("centre").as_ref(), cx, cy);
            gl.uniform_1_i32(u("count").as_ref(), merged.len() as i32);
            if !merged.is_empty() {
                gl.uniform_4_f32_slice(u("lpos").as_ref(), &pos);
                gl.uniform_3_f32_slice(u("lcol").as_ref(), &col);
            }
            gl.uniform_1_f32(u("strength").as_ref(), ao::tune("NSTRIKE_LIGHT_STRENGTH", STRENGTH));
            gl.uniform_1_f32(u("added").as_ref(), 1.0);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(out));
            gl.viewport(0, 0, w, h);
            gl.disable(glow::SCISSOR_TEST);
            if show == 0 {
                // picture * (1 + light)
                gl.enable(glow::BLEND);
                gl.blend_func(glow::DST_COLOR, glow::ONE);
            } else {
                gl.disable(glow::BLEND);
            }
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            if show == 0 {
                // picture + a share of the light
                gl.blend_func(glow::ONE, glow::ONE);
                gl.uniform_1_f32(u("added").as_ref(), ADDED);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            gl.disable(glow::BLEND);
            gl.bind_texture(kind, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }
}
