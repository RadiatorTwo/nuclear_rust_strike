//! Depth for the modern effects (ambient occlusion first), as future_rust_cop's `ao.rs`.
//!
//! The game draws like the PlayStation: Glide vertices carry no depth (oow 1, ooz 0.1), the
//! primitives are sorted farthest first. The projections tell it: vec3fp16_ProjectTerrainListASM_
//! (0x4b96c0) and vec3fp16_ProjectModelListASM_ (0x4b98c2) take (eax matrix, edx points
//! {x, y, z} s16, ebx out, ecx view, stack count) and write 8 bytes a point: sx, sy (s16, 1/8
//! pixel), z (u16), on screen (u8 at +6). Both are hooked and every projected point's z is kept
//! by its screen position (`point`); the Glide renderer finds each 3D vertex's z by its
//! position (`depths`). Cleared at every picture shown.

use super::arg;
use crate::rt::{Cpu, EBX, ECX, call_original};
use glow::HasContext;
use std::sync::Mutex;
use std::cell::RefCell;
use std::collections::HashMap;

pub const PROJECT_TERRAIN: u32 = 0x4b96c0;
pub const PROJECT_MODEL: u32 = 0x4b98c2;
/// The draw offset x, y (1/8 pixels) the Glide layer adds to a primitive's points (gl_* 0x472f94).
const DRAW_OFFSET: u32 = 0x4ede5c;

/// A fast hash for the points' keys.
#[derive(Default, Clone, Copy)]
struct KeyHash(u64);

impl std::hash::Hasher for KeyHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u32(b as u32);
        }
    }
    fn write_u32(&mut self, k: u32) {
        self.0 = (self.0.rotate_left(5) ^ k as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

type KeyMap<V> = HashMap<u32, V, std::hash::BuildHasherDefault<KeyHash>>;

/// This picture's projections. The projections and the renderer run on the game's thread.
#[derive(Default)]
struct Frame {
    /// screen position (x | y << 16, 1/8 pixels) -> up to two z (0: none)
    points: KeyMap<[u16; 2]>,
    found: Vec<[u16; 2]>,
    sure: Vec<u16>,
}

thread_local! {
    static FRAME: RefCell<Frame> = RefCell::new(Frame::default());
}

fn key(x: i16, y: i16) -> u32 {
    x as u16 as u32 | (y as u16 as u32) << 16
}

fn point(x: i16, y: i16, z: u16) {
    if z == 0 {
        return;
    }
    FRAME.with_borrow_mut(|f| {
        let e = f.points.entry(key(x, y)).or_insert([0, 0]);
        if e[0] == 0 || e[0] == z {
            e[0] = z;
        } else if e[1] == 0 {
            e[1] = z;
        }
    });
}

/// The camera of the 3D picture (its terrain's projection): pixels (of the game's resolution)
/// per unit at z = 1 in x and y, and the screen's centre in pixels. The projection gives 1/8
/// pixels: sx = x * [0x4d002c] / z * [view] + [view + 8] (at 800x600: 24000, centre 3200; a
/// narrow camera far away).
static CAMERA: Mutex<[f32; 4]> = Mutex::new([0.0; 4]);

fn project(c: &mut Cpu, addr: u32) {
    let (out, n) = (c.r[EBX], arg(c, 0));
    let m = super::mem();
    if addr == PROJECT_TERRAIN {
        let f = |a: u32| f32::from_bits(m.rd32(a));
        let (k, v) = (f(0x4d002c), c.r[ECX]);
        *CAMERA.lock().unwrap() = [k * f(v) / 8.0, k * f(v + 4) / 8.0, f(v + 8) / 8.0, f(v + 0xc) / 8.0];
    }
    call_original(c, addr);
    for k in 0..n.min(0x10000) {
        let r = out + 8 * k;
        point(m.rd16(r) as i16, m.rd16(r + 2) as i16, m.rd16(r + 4));
    }
}

pub fn project_terrain(c: &mut Cpu) {
    project(c, PROJECT_TERRAIN);
}

pub fn project_model(c: &mut Cpu) {
    project(c, PROJECT_MODEL);
}

/// At every picture shown: the next one projects anew.
pub fn clear() {
    FRAME.with_borrow_mut(|f| f.points.clear());
}

/// The z of a primitive's vertices (x, y in pixels of the game's resolution) into `out`, 0
/// where none is known. True when every vertex is a projected point.
pub fn depths(verts: impl ExactSizeIterator<Item = (f32, f32)>, out: &mut Vec<f32>) -> bool {
    out.clear();
    let n = verts.len();
    FRAME.with_borrow_mut(|f| {
        let Frame { points, found, sure } = f;
        found.clear();
        // the renderer adds the draw offset (the 3D window's corner: (10, 10) pixels at 800x600
        // in the cockpit HUD modes) to the projected points
        let m = super::mem();
        let (ox, oy) = (m.rd32(DRAW_OFFSET) as i32, m.rd32(DRAW_OFFSET + 4) as i32);
        found.extend(verts.map(|(x, y)| {
            let (x, y) = ((x * 8.0).round() as i32 - ox, (y * 8.0).round() as i32 - oy);
            points.get(&key(x as i16, y as i16)).copied().unwrap_or([0, 0])
        }));
        // a reference: the vertices with one candidate (their median), else any candidate
        sure.clear();
        sure.extend(found.iter().filter(|f| f[0] != 0 && f[1] == 0).map(|f| f[0]));
        sure.sort_unstable();
        let reference = sure.get(sure.len() / 2).copied().or_else(|| found.iter().find(|f| f[0] != 0).map(|f| f[0]));
        let Some(r) = reference else {
            out.resize(n, 0.0);
            return false;
        };
        out.extend(found.iter().map(|f| match *f {
            [0, _] => r,
            [a, 0] => a,
            [a, b] => if a.abs_diff(r) <= b.abs_diff(r) { a } else { b },
        } as f32));
        found.iter().all(|f| f[0] != 0)
    })
}

static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// At start: the setting.
pub fn init() {
    ENABLED.store(crate::settings::get().ambient_occlusion, std::sync::atomic::Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The F10 menu: saved, used from the next picture on.
pub fn set_enabled(on: bool) {
    crate::settings::update(|s| s.ambient_occlusion = on);
    ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// `NSTRIKE_GBUFFER`: 0 the picture, 1 the depth, 2 the AO alone.
fn debug_view() -> i32 {
    static V: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *V.get_or_init(|| match std::env::var("NSTRIKE_GBUFFER").as_deref() {
        Ok("depth") => 1,
        Ok("ao") => 2,
        _ => 0,
    })
}

/// A number from the environment, for tuning.
fn tune(name: &str, default: f32) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The occlusion's reach in object units (a tree is ~700 high), and how strongly it darkens.
const RADIUS: f32 = 500.0;
const STRENGTH: f32 = 1.0;

const VS: &str = r#"
void main() {
    gl_Position = vec4(float((gl_VertexID & 1) * 4 - 1), float((gl_VertexID >> 1) * 4 - 1), 0.0, 1.0);
}
"#;

/// Shared by the passes: the depth attachment (multisampled: sample 0) and view positions.
const COMMON: &str = r#"
#ifdef MS
uniform sampler2DMS gdepth;
#else
uniform sampler2D gdepth;
#endif
uniform ivec2 size;     // the target in pixels
uniform vec2 focal;     // pixels per unit at z = 1, x and y
uniform vec2 centre;    // the projection's centre in pixels (GL's, from below)
vec2 G(ivec2 q) {
    return texelFetch(gdepth, clamp(q, ivec2(0), size - 1), 0).rg;
}
vec3 P(vec2 q, float z) {
    return vec3((q - centre) * z / focal, z);
}
// the normal at q (depth z, place p) from the neighbours s pixels away on the side nearer in
// depth (not across an edge), facing the camera
vec3 N(ivec2 q, float z, vec3 p, int s) {
    float zr = G(q + ivec2(s, 0)).r, zl = G(q - ivec2(s, 0)).r;
    float zu = G(q + ivec2(0, s)).r, zd = G(q - ivec2(0, s)).r;
    vec3 dx = (zl <= 0.0 || zr > 0.0 && abs(zr - z) < abs(z - zl)) ? P(vec2(q + ivec2(s, 0)), zr > 0.0 ? zr : z) - p : p - P(vec2(q - ivec2(s, 0)), zl);
    vec3 dy = (zd <= 0.0 || zu > 0.0 && abs(zu - z) < abs(z - zd)) ? P(vec2(q + ivec2(0, s)), zu > 0.0 ? zu : z) - p : p - P(vec2(q - ivec2(0, s)), zd);
    vec3 n = cross(dx, dy);
    n = length(n) > 0.0 ? normalize(n) : vec3(0.0, 0.0, -1.0);
    return dot(n, p) > 0.0 ? -n : n;
}
"#;

const AO_FS: &str = r#"
uniform float radius;
uniform float max_px;
out vec4 o;
const int SAMPLES = 12;
void main() {
    ivec2 q = ivec2(gl_FragCoord.xy) * 2;
    float z = G(q).r;
    if (z <= 0.0) { o = vec4(1.0); return; }
    vec3 p = P(vec2(q), z);
    vec3 n = N(q, z, p, 2);
    // a different turn of the samples at every pixel (interleaved gradient noise), blurred after
    float ign = fract(52.9829189 * fract(dot(gl_FragCoord.xy, vec2(0.06711056, 0.00583715))));
    float rpx = min(radius * focal.y / z, max_px);
    if (rpx < 2.0) { o = vec4(1.0); return; }
    float a = 6.2831853 * ign;
    float occ = 0.0;
    for (int i = 0; i < SAMPLES; i++) {
        float t = (float(i) + 0.5) / float(SAMPLES);
        float ang = a + float(i) * 2.3999632;
        vec2 s = vec2(q) + vec2(cos(ang), sin(ang)) * t * rpx;
        float zs = G(ivec2(s)).r;
        if (zs <= 0.0) continue;
        vec3 v = P(s, zs) - p;
        float d = length(v);
        if (d < 1e-3) continue;
        occ += max(dot(v / d, n) - 0.05, 0.0) * (1.0 - smoothstep(0.5 * radius, radius, d));
    }
    o = vec4(pow(clamp(1.0 - 4.0 * occ / float(SAMPLES), 0.0, 1.0), 2.0));
}
"#;

/// The blur and the upsampling weigh neighbours by how near their depth is: within `edge`
/// units (the camera is far and narrow: the depth's spread in a picture is small).
const BLUR_FS: &str = r#"
uniform sampler2D ao;
uniform ivec2 dir;
uniform float edge;
out vec4 o;
void main() {
    ivec2 p = ivec2(gl_FragCoord.xy);
    ivec2 hs = textureSize(ao, 0);
    float z = G(p * 2).r;
    float sum = 0.0, wsum = 0.0;
    for (int k = -4; k <= 4; k++) {
        ivec2 s = clamp(p + dir * k, ivec2(0), hs - 1);
        float zs = G(s * 2).r;
        float w = exp(-float(k * k) / 10.0);
        if (z > 0.0) w *= max(1.0 - abs(zs - z) / edge, 0.0);
        sum += texelFetch(ao, s, 0).r * w;
        wsum += w;
    }
    o = vec4(wsum > 0.0 ? sum / wsum : 1.0);
}
"#;

const APPLY_FS: &str = r#"
uniform sampler2D ao;
uniform float strength;
uniform float edge;
uniform int show;       // 0 the AO as a factor, 1 the depth, 2 the AO alone
out vec4 o;
void main() {
    ivec2 q = ivec2(gl_FragCoord.xy);
    vec2 g = G(q);
    float z = g.r;
    if (show == 1) { o = vec4(vec3(z > 0.0 ? clamp(1.0 - (z - 9000.0) / 9000.0, 0.05, 1.0) : 0.0), 1.0); return; }
    float v = 1.0;
    if (z > 0.0) {
        // the four nearest half-resolution values, weighed by how near their depth is
        ivec2 hs = textureSize(ao, 0);
        vec2 h = (vec2(q) + 0.5) * 0.5 - 0.5;
        ivec2 b = ivec2(floor(h));
        vec2 f = h - floor(h);
        float sum = 0.0, wsum = 0.0;
        for (int k = 0; k < 4; k++) {
            ivec2 o2 = ivec2(k & 1, k >> 1);
            ivec2 c = clamp(b + o2, ivec2(0), hs - 1);
            float zc = G(c * 2).r;
            float w = (o2.x == 1 ? f.x : 1.0 - f.x) * (o2.y == 1 ? f.y : 1.0 - f.y);
            w *= max(1.0 - abs(zc - z) / edge, 0.001);
            sum += texelFetch(ao, c, 0).r * w;
            wsum += w;
        }
        v = sum / wsum;
    }
    if (show == 2) { o = vec4(vec3(v), 1.0); return; }
    o = vec4(vec3(mix(1.0, v, strength * (1.0 - clamp(g.g, 0.0, 1.0)))), 1.0);
}
"#;

/// A half-resolution texture and its framebuffer.
type Half = (glow::Texture, glow::Framebuffer);

/// The passes' programs (single-sampled and multisampled depth), their half-resolution
/// textures and framebuffers. Made and used on the game's thread (framebuffers are not shared).
pub struct Ssao {
    progs: [[glow::Program; 2]; 3],
    vao: glow::VertexArray,
    half: Option<((i32, i32), [Half; 2])>,
}

impl Ssao {
    pub fn new(gl: &glow::Context) -> Ssao {
        let prog = |fs: &str, ms: bool| super::gl::compile(gl, VS, &format!("{}{COMMON}{fs}", if ms { "#define MS\n" } else { "" }));
        let progs = [AO_FS, BLUR_FS, APPLY_FS].map(|fs| [prog(fs, false), prog(fs, true)]);
        let vao = unsafe { gl.create_vertex_array().expect("GL vertex array") };
        Ssao { progs, vao, half: None }
    }

    pub fn delete(&mut self, gl: &glow::Context) {
        unsafe {
            for p in self.progs.iter().flatten() {
                super::gl::delete_program(gl, *p);
            }
            gl.delete_vertex_array(self.vao);
        }
        self.drop_half(gl);
    }

    fn drop_half(&mut self, gl: &glow::Context) {
        if let Some((_, h)) = self.half.take() {
            for (t, f) in h {
                unsafe {
                    gl.delete_framebuffer(f);
                    gl.delete_texture(t);
                }
            }
        }
    }

    fn half(&mut self, gl: &glow::Context, size: (i32, i32)) -> [Half; 2] {
        if let Some((s, h)) = self.half
            && s == size
        {
            return h;
        }
        self.drop_half(gl);
        let make = || unsafe {
            let t = super::gl::texture(gl, false);
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::R8 as i32, size.0, size.1, 0, glow::RED, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            let f = gl.create_framebuffer().expect("GL framebuffer");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(f));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(t), 0);
            (t, f)
        };
        let h = [make(), make()];
        self.half = Some((size, h));
        h
    }

    /// The passes over the 3D picture: `depth` the attachment (multisampled or not), `target`
    /// its size, `space` the vertices' (the game's resolution), `out` a framebuffer with only
    /// the picture's colour.
    pub fn run(&mut self, gl: &glow::Context, depth: glow::Texture, ms: bool, target: (u32, u32), space: (u32, u32), out: glow::Framebuffer) {
        let (w, h) = (target.0 as i32, target.1 as i32);
        let hs = ((w + 1) / 2, (h + 1) / 2);
        let half = self.half(gl, hs);
        let cam = *CAMERA.lock().unwrap();
        if cam[0] <= 0.0 || cam[1] <= 0.0 {
            return;
        }
        // game pixels to target pixels
        let (kx, ky) = (w as f32 / space.0 as f32, h as f32 / space.1 as f32);
        let (focal, centre) = ((cam[0] * kx, cam[1] * ky), (cam[2] * kx, h as f32 - cam[3] * ky));
        let radius = tune("NSTRIKE_AO_RADIUS", RADIUS);
        let edge = 0.5 * radius;
        let v = ms as usize;
        unsafe {
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            let kind = if ms { glow::TEXTURE_2D_MULTISAMPLE } else { glow::TEXTURE_2D };
            gl.bind_texture(kind, Some(depth));
            let common = |p: glow::Program| {
                gl.use_program(Some(p));
                let u = move |n: &'static str| super::gl::uniform(gl, p, n);
                gl.uniform_1_i32(u("gdepth").as_ref(), 0);
                gl.uniform_1_i32(u("ao").as_ref(), 1);
                gl.uniform_2_i32(u("size").as_ref(), w, h);
                gl.uniform_2_f32(u("focal").as_ref(), focal.0, focal.1);
                gl.uniform_2_f32(u("centre").as_ref(), centre.0, centre.1);
                gl.uniform_1_f32(u("edge").as_ref(), edge);
                u
            };
            // the AO at half resolution
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(half[0].1));
            gl.viewport(0, 0, hs.0, hs.1);
            let u = common(self.progs[0][v]);
            gl.uniform_1_f32(u("radius").as_ref(), radius);
            gl.uniform_1_f32(u("max_px").as_ref(), h as f32 / 4.0);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            // blurred across, then down
            gl.active_texture(glow::TEXTURE1);
            for (from, to, dir) in [(0, 1, (1, 0)), (1, 0, (0, 1))] {
                gl.bind_texture(glow::TEXTURE_2D, Some(half[from].0));
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(half[to].1));
                let u = common(self.progs[1][v]);
                gl.uniform_2_i32(u("dir").as_ref(), dir.0, dir.1);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            // the picture times the AO
            gl.bind_texture(glow::TEXTURE_2D, Some(half[0].0));
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(out));
            gl.viewport(0, 0, w, h);
            let show = debug_view();
            if show == 0 {
                gl.enable(glow::BLEND);
                gl.blend_func(glow::DST_COLOR, glow::ZERO);
            }
            let u = common(self.progs[2][v]);
            gl.uniform_1_f32(u("strength").as_ref(), tune("NSTRIKE_AO_STRENGTH", STRENGTH));
            gl.uniform_1_i32(u("show").as_ref(), show);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.disable(glow::BLEND);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(kind, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }
}
