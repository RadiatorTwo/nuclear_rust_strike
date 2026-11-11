//! Sun shadows in missions: trees, buildings, vehicles and the helicopter cast shadows on what
//! is around them.
//!
//! A shadow map from the game's own triangles. The 3D picture's triangles that write depth
//! (opaque and alpha-blended solid ones) are kept as `glide` draws them (`Caster`: its batched
//! vertices, the chroma key in the constant colour's place). Every vertex's place in view
//! space is known from its screen position and its depth (`ao.rs`), so they are drawn a second
//! time, orthographically from the sun, into a depth texture (`ShadowMap::render`; keyed texels
//! are cut out: palm fronds). The AO's half-resolution pass looks each pixel up in it (3x3
//! hardware PCF) and the shadow is blurred and applied with the AO (also with the AO off).
//! (A screen-space ray march came first: it flickered where thin moving things like the rotor
//! were in the way, and its scattered hits stayed grey whatever the strength.)
//!
//! Only what the game draws casts. So that trees and vehicles just outside the picture cast
//! into it, ObjMgr's objects of the tiles around the visible ones are drawn too, with the
//! projection zoomed out (`pictures::shadow_ring`); their primitives only go into the map
//! (`glide`'s `cast`, corners taken back to the picture's projection). The terrain is not
//! (it casts little): a guard band that made the game draw a larger terrain overran its fixed
//! visibility table (0x520510: 9 x 33 x 34 cells of ~2 tiles, ~66 tiles across at any heading,
//! 16:9 already needs 65) and drew streaks; a memory of triangles drawn before lost what moves.
//!
//! The sun is the game's own (its models are lit by it): Actor3D_UpdateSun_ (0x410310) turns
//! the light's direction in the world `[0x503f28]` (3 x i16, set by Actor3D_InitModule_ to
//! (-147, -147, -147): down, world y is up, the camera at y ~6900) into camera space
//! `[0x503f2e]` (3 x i16, length ~16384; x right, y down the screen, z ahead). It points away
//! from the sun: the models' shading brightens faces whose normal points against it.
//! It is read with the camera (`ao.rs`, at the terrain's projection: the picture's own camera
//! and sun, also between logic frames) and turned into the shaders' view space (y up).
//!
//! The game leaves out a model's faces turned away from the camera (`MODEL_FACE` 0x411ca4: the
//! face's normal against the view direction, unless its first byte says two-sided): the far
//! side of a boat cast nothing. In the ring every face is made two-sided while it is drawn
//! (`model_face`): the sun sees the sides the camera does not.
//!
//! Setting `shadows` (F11), `NSTRIKE_SHADOWS=0/1`; `NSTRIKE_GBUFFER=shadows` shows the shadow
//! alone; tuning `NSTRIKE_SHADOW_STRENGTH`. Setting `shadow_quality` (F11,
//! `NSTRIKE_SHADOW_QUALITY=low|mid|high|ultra`, `Look`): the map's size, the lookup's
//! softness, where it is looked up, and (low) no ring: the objects' own primitives on screen
//! cast instead.

use glow::HasContext;
use crate::settings::ShadowQuality;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};

static ENABLED: AtomicBool = AtomicBool::new(false);
/// `ShadowQuality::ALL`'s index
static QUALITY: AtomicU8 = AtomicU8::new(1);

/// At start: the settings.
pub fn init() {
    let s = crate::settings::get();
    ENABLED.store(s.shadows, Relaxed);
    QUALITY.store(s.shadow_quality as u8, Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

fn quality() -> ShadowQuality {
    ShadowQuality::ALL[QUALITY.load(Relaxed) as usize % ShadowQuality::ALL.len()]
}

/// The F11 menu: saved, used from the next picture on.
pub fn set_quality(q: ShadowQuality) {
    crate::settings::update(|s| s.shadow_quality = q);
    QUALITY.store(q as u8, Relaxed);
}

/// The objects are drawn once more for the shadows, zoomed out (`pictures::shadow_ring`), and
/// their primitives on screen do not cast; low quality: those on screen cast, no ring.
pub fn ring() -> bool {
    enabled() && quality() != ShadowQuality::Low
}

/// How a quality looks: the map's size in texels; the lookup's taps, (2r + 1)² hardware PCF
/// taps `step` texels apart (how soft the edge is: r * step texels each way); whether it is
/// looked up for every pixel in the AO's last pass, else at half resolution and blurred with
/// the AO (very soft: the blur is ~18 pixels wide, the map's detail is lost).
pub struct Look {
    pub size: i32,
    pub taps: i32,
    pub step: f32,
    pub full: bool,
}

pub fn look() -> Look {
    match quality() {
        ShadowQuality::Low => Look { size: 1024, taps: 1, step: 1.0, full: false },
        // as soft as High's edge at 4096 two times over
        ShadowQuality::Mid => Look { size: 2048, taps: 2, step: 1.5, full: true },
        ShadowQuality::High => Look { size: 4096, taps: 2, step: 1.5, full: true },
        ShadowQuality::Ultra => Look { size: 4096, taps: 2, step: 1.0, full: true },
    }
}

fn size() -> i32 {
    look().size
}

/// The F11 menu: saved, used from the next picture on.
pub fn set_enabled(on: bool) {
    crate::settings::update(|s| s.shadows = on);
    ENABLED.store(on, Relaxed);
}

/// The sun in camera space (Actor3D_UpdateSun_'s result).
const SUN_CAMERA: u32 = 0x503f2e;

/// Draws a model's face (called by Actor3D_Render_ per face): eax the face record, its first
/// byte non-zero for two-sided (else left out when turned away from the camera).
pub const MODEL_FACE: u32 = 0x411ca4;

/// In the shadows' ring: the face drawn as two-sided (its byte put back afterwards).
pub fn model_face(c: &mut crate::rt::Cpu) {
    if !super::wide::is_casting() {
        return crate::rt::call_original(c, MODEL_FACE);
    }
    let m = super::mem();
    let face = c.r[crate::rt::EAX];
    let was = m.rd8(face);
    m.wr8(face, 1);
    crate::rt::call_original(c, MODEL_FACE);
    m.wr8(face, was);
}

/// Actor3D_RenderShadow_(model, matrix, position, darkness, ...; 3 dwords on the stack): the
/// game's own shadow of a model, flattened onto the ground, called by 0x431914 per object
/// with one: in mission 1 only the helicopter (and in the shadows' ring, as a caster).
pub const RENDER_SHADOW: u32 = 0x412cf8;

/// The game's model shadows are left out while ours are on (they lay twice under the
/// helicopter).
pub fn render_shadow(c: &mut crate::rt::Cpu) {
    if !enabled() {
        return crate::rt::call_original(c, RENDER_SHADOW);
    }
    // ret 0Ch (its caller does not use the result)
    c.r[crate::rt::EAX] = 0;
    c.r[crate::rt::ESP] += 4 + 12;
}

/// How dark a shadow is (0...1).
pub const STRENGTH: f32 = 0.5;

/// Toward the sun in the shaders' view space (x right, y up, z ahead), unit; None when the
/// game has none.
pub fn sun() -> Option<[f32; 3]> {
    let m = super::mem();
    let c = |k: u32| m.rd16(SUN_CAMERA + 2 * k) as i16 as f32;
    // the light's direction in camera space (y down), turned around and y up
    let v = [-c(0), c(1), -c(2)];
    let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    (n > 0.0).then(|| v.map(|c| c / n))
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn unit(a: [f32; 3]) -> [f32; 3] {
    let n = dot(a, a).sqrt().max(1e-9);
    a.map(|c| c / n)
}

/// The shadow map of a picture as the lookup takes it: the texture, the sun's axes in view
/// space (right, up, away from the sun), the box drawn (x0, y0, x1, y1) and the depth range.
pub struct View {
    pub tex: glow::Texture,
    pub axes: [[f32; 3]; 3],
    pub rect: [f32; 4],
    pub depth: [f32; 2],
}

const CASTER_VS: &str = r#"
uniform vec4 map;        // game to target pixels (`ao::Map`)
uniform float h;         // the target's height
uniform vec2 focal;
uniform vec2 centre;
uniform vec3 ax, ay, az; // the sun's axes in view space
uniform vec4 rect;
uniform vec2 drange;
layout(location = 0) in vec2 pos;
layout(location = 2) in vec2 tc;
layout(location = 3) in vec4 kc;
layout(location = 4) in uvec4 mode;
layout(location = 5) in vec4 box;
layout(location = 6) in float z;
noperspective out vec2 st;
flat out vec4 key;
flat out uvec4 vmode;
flat out vec4 st_box;
void main() {
    st = tc;
    key = kc;
    vmode = mode;
    st_box = box;
    vec2 q = vec2(map.x + pos.x * map.z, h - (map.y + pos.y * map.w));
    vec3 p = vec3((q - centre) * z / focal, z);
    vec3 l = vec3(dot(p, ax), dot(p, ay), dot(p, az));
    vec2 xy = (l.xy - rect.xy) / (rect.zw - rect.xy) * 2.0 - 1.0;
    float d = (l.z - drange.x) / (drange.y - drange.x) * 2.0 - 1.0;
    gl_Position = vec4(xy, d, 1.0);
}
"#;

const CASTER_FS: &str = r#"
uniform sampler2DArray tex;
uniform sampler2DArray tex_a;
noperspective in vec2 st;
flat in vec4 key;        // the chroma key (rgb) and whether it is on (a)
flat in uvec4 vmode;
flat in vec4 st_box;
void main() {
    if (((vmode.z >> 2) & 1u) == 0u) return;
    int layer = int((vmode.z >> 8) & 0xffu);
    ivec2 s = ivec2(vmode.w & 0xffffu, vmode.w >> 16);
    ivec2 lo = ivec2(floor(st_box.xy));
    ivec2 p = clamp(ivec2(floor(st)), lo, max(ivec2(ceil(st_box.zw)) - 1, lo));
    p = ((p % s) + s) % s;
    vec4 c = texelFetch(tex, ivec3(p, layer), 0);
    // keyed out (coverage 0), or a chroma key matched
    if (c.a < 0.5) discard;
    if (key.a > 0.5 && all(lessThan(abs(c.rgb - key.rgb), vec3(0.5 / 255.0)))) discard;
    // the alpha combine takes the texture's alpha: cut where it is low (alpha-tested leaves)
    uint other = (vmode.y >> 24) & 255u;
    vec4 a = texelFetch(tex_a, ivec3(p, layer), 0);
    if (other == 1u && a.r + a.g < 0.5) discard;
}
"#;

/// GLSL after `ao::COMMON`: how much of p (normal n) is in the sun's shadow, 0...1.
pub const GLSL: &str = r#"
uniform sampler2DShadow smap;
uniform vec3 sx, sy, sz;   // the sun's axes
uniform vec4 srect;
uniform vec2 sdepth;
uniform int staps;         // the lookup's taps: (2 staps + 1)², `sstep` texels apart
uniform float sstep;
float shadow(vec3 p, vec3 n) {
    // faces turned away are the game's own shading
    if (dot(n, -sz) <= 0.02) return 0.0;
    vec2 texel = (srect.zw - srect.xy) / vec2(textureSize(smap, 0));
    // off the surface by a texel or two (against acne), more where the sun grazes it
    vec3 o = p + n * (2.0 * max(texel.x, texel.y));
    vec3 l = vec3(dot(o, sx), dot(o, sy), dot(o, sz));
    vec2 uv = (l.xy - srect.xy) / (srect.zw - srect.xy);
    if (any(lessThan(uv, vec2(0.0))) || any(greaterThan(uv, vec2(1.0)))) return 0.0;
    float d = (l.z - sdepth.x) / (sdepth.y - sdepth.x) - 0.002;
    vec2 t = sstep / vec2(textureSize(smap, 0));
    float lit = 0.0;
    for (int j = -staps; j <= staps; j++)
        for (int i = -staps; i <= staps; i++)
            lit += texture(smap, vec3(uv + vec2(i, j) * t, d));
    float k = float(2 * staps + 1);
    return 1.0 - lit / (k * k);
}
"#;

/// The shadow map: its program, vertex array and buffer, depth texture and framebuffer. Made
/// and used on the game's thread.
pub struct ShadowMap {
    prog: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    tex: glow::Texture,
    fbo: glow::Framebuffer,
    /// the texture's size (`size()` when made: made anew when the quality changes it)
    size: i32,
}

impl ShadowMap {
    pub fn new(gl: &glow::Context) -> ShadowMap {
        let prog = super::gl::compile(gl, CASTER_VS, CASTER_FS);
        unsafe {
            let vao = gl.create_vertex_array().expect("GL vertex array");
            let vbo = gl.create_buffer().expect("GL buffer");
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            super::glide::vertex_layout(gl);
            gl.bind_vertex_array(None);
            let tex = gl.create_texture().expect("GL texture");
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            let size = size();
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::DEPTH_COMPONENT24 as i32, size, size, 0, glow::DEPTH_COMPONENT, glow::UNSIGNED_INT, glow::PixelUnpackData::Slice(None));
            for (p, v) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_COMPARE_MODE, glow::COMPARE_REF_TO_TEXTURE),
                (glow::TEXTURE_COMPARE_FUNC, glow::LEQUAL),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
            let fbo = gl.create_framebuffer().expect("GL framebuffer");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::DEPTH_ATTACHMENT, glow::TEXTURE_2D, Some(tex), 0);
            gl.draw_buffers(&[]);
            gl.read_buffer(glow::NONE);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            ShadowMap { prog, vao, vbo, tex, fbo, size }
        }
    }

    pub fn delete(&mut self, gl: &glow::Context) {
        unsafe {
            super::gl::delete_program(gl, self.prog);
            gl.delete_vertex_array(self.vao);
            gl.delete_buffer(self.vbo);
            gl.delete_framebuffer(self.fbo);
            gl.delete_texture(self.tex);
        }
    }

    /// Draw the casters (`glide`'s batched vertices, triangles) from the sun `sun` (view
    /// space, toward it); `cam` the camera in target pixels, `map` game to target pixels,
    /// `h` the target's height, `textures` the texture arrays (colour, alpha).
    #[allow(clippy::too_many_arguments)]
    pub fn render(&mut self, gl: &glow::Context, casters: &[u32], sun: [f32; 3], cam: ((f32, f32), (f32, f32)), map: super::ao::Map, h: f32, textures: (glow::Texture, glow::Texture)) -> Option<View> {
        use super::glide::VERTEX;
        if casters.is_empty() {
            return None;
        }
        if self.size != size() {
            self.size = size();
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(self.tex));
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::DEPTH_COMPONENT24 as i32, self.size, self.size, 0, glow::DEPTH_COMPONENT, glow::UNSIGNED_INT, glow::PixelUnpackData::Slice(None));
                gl.bind_texture(glow::TEXTURE_2D, None);
            }
        }
        let ((fx, fy), (cx, cy)) = cam;
        // the sun's axes: away from it, and two across
        let az = sun.map(|c| -c);
        let up = if az[1].abs() > 0.9 { [1.0, 0.0, 0.0] } else { [0.0, 1.0, 0.0] };
        let ax = unit(cross(up, az));
        let ay = cross(az, ax);
        // the casters' bounds in the sun's space
        let mut lo = [f32::MAX; 3];
        let mut hi = [f32::MIN; 3];
        for v in casters.chunks_exact(VERTEX) {
            let (x, y, z) = (f32::from_bits(v[0]), f32::from_bits(v[1]), f32::from_bits(v[20]));
            let p = [(map[0] + x * map[2] - cx) * z / fx, (h - (map[1] + y * map[3]) - cy) * z / fy, z];
            let l = [dot(p, ax), dot(p, ay), dot(p, az)];
            for i in 0..3 {
                lo[i] = lo[i].min(l[i]);
                hi[i] = hi[i].max(l[i]);
            }
        }
        let pad = |a: f32, b: f32| (b - a).max(1.0) * 0.01;
        let rect = [lo[0] - pad(lo[0], hi[0]), lo[1] - pad(lo[1], hi[1]), hi[0] + pad(lo[0], hi[0]), hi[1] + pad(lo[1], hi[1])];
        let depth = [lo[2] - pad(lo[2], hi[2]), hi[2] + pad(lo[2], hi[2])];
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.viewport(0, 0, self.size, self.size);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.disable(glow::CULL_FACE);
            gl.enable(glow::DEPTH_TEST);
            gl.depth_func(glow::LESS);
            gl.depth_mask(true);
            gl.clear_depth_f32(1.0);
            gl.clear(glow::DEPTH_BUFFER_BIT);
            gl.use_program(Some(self.prog));
            let p = self.prog;
            let u = |n: &'static str| super::gl::uniform(gl, p, n);
            gl.uniform_4_f32(u("map").as_ref(), map[0], map[1], map[2], map[3]);
            gl.uniform_1_f32(u("h").as_ref(), h);
            gl.uniform_2_f32(u("focal").as_ref(), fx, fy);
            gl.uniform_2_f32(u("centre").as_ref(), cx, cy);
            gl.uniform_3_f32(u("ax").as_ref(), ax[0], ax[1], ax[2]);
            gl.uniform_3_f32(u("ay").as_ref(), ay[0], ay[1], ay[2]);
            gl.uniform_3_f32(u("az").as_ref(), az[0], az[1], az[2]);
            gl.uniform_4_f32(u("rect").as_ref(), rect[0], rect[1], rect[2], rect[3]);
            gl.uniform_2_f32(u("drange").as_ref(), depth[0], depth[1]);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D_ARRAY, Some(textures.0));
            gl.uniform_1_i32(u("tex").as_ref(), 0);
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D_ARRAY, Some(textures.1));
            gl.uniform_1_i32(u("tex_a").as_ref(), 1);
            gl.bind_vertex_array(Some(self.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            let bytes = std::slice::from_raw_parts(casters.as_ptr() as *const u8, casters.len() * 4);
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
            gl.draw_arrays(glow::TRIANGLES, 0, (casters.len() / VERTEX) as i32);
            gl.bind_vertex_array(None);
            gl.disable(glow::DEPTH_TEST);
            gl.bind_texture(glow::TEXTURE_2D_ARRAY, None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D_ARRAY, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        Some(View { tex: self.tex, axes: [ax, ay, az], rect, depth })
    }
}
