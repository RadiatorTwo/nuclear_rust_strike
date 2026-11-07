//! The game's 3dfx Glide 2.x renderer on OpenGL. The game loads glide2x.dll itself and takes
//! its functions with GetProcAddress (`proc_address`): every name gets a thunk, the decorated
//! name (`_grDrawTriangle@12`) gives the stdcall argument size.
//!
//! The game draws its depth-sorted triangles with Glide's colour and alpha combine units, alpha
//! blending and a chroma key (black texels are not drawn), textures 256x256 ARGB1555 in TMU
//! memory, no depth buffer. We draw into a texture as tall as the window or the resolution
//! chosen in Preferences (`display.rs`; high resolution:
//! the Glide screen of `grSstWinOpen` only gives the coordinates; the game opens 1280x1024
//! for its 1280x960 mode and stretches its 4:3 picture to it) and show it at `grBufferSwap`. Vertices are in Glide screen pixels, or in the space
//! the display list walker sets (`set_space`, see `walker.rs`); clip windows are always in
//! screen pixels. Widescreen (`wide.rs`): the texture takes the window's aspect, the 3D
//! picture fills it and the HUD is drawn into 4:3 parts of it (`set_view`). With filtering on, the chroma key leaves the keyed texels
//! out of the bilinear filter, and their share becomes a soft alpha edge.
//!
//! Textures are kept premultiplied by their coverage (the key is black, as the game always
//! sets it): rgb and coverage in `array`, the texture's alpha (covered and keyed share) in
//! `alpha`. Filtered by the GPU that is the keyed bilinear filter above, and it also gives
//! mipmaps and anisotropic filtering without dark fringes (`filtering`: REG_DWORD
//! `TextureFilter`). A page is an atlas of small pictures: a primitive samples inside the
//! rectangle of its texture coordinates, at a level that still has a few texels of it.

use super::gl::{compile, texture};
use super::{arg, log, mem, thunk, thunk_not_implemented};
use crate::rt::{Cpu, EDX};
use glow::HasContext;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};

/// The module handle LoadLibraryA gives for glide2x.dll.
pub const MODULE: u32 = 0x1000_0000;

/// The picture's aspect (no widescreen yet).
const ASPECT: f32 = 4.0 / 3.0;

/// TMU memory reported: 4 MB.
const TMU_RAM: u32 = 4 << 20;

#[derive(Clone, Copy, PartialEq, Debug)]
struct State {
    /// rgb source and destination factor
    blend: (u32, u32),
    chroma: bool,
    key: u32,
    linear: bool,
    clamp: (bool, bool),
    clip: [i32; 4],
}

/// Where the following drawing goes in the frame.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum View {
    /// the whole frame
    Full,
    /// a 4:3 part: 0 at the left edge, 0.5 centred, 1 at the right edge
    Hud(f32),
    /// split screen: a 4:3 HUD whose left (or right) half is centred in the left (right)
    /// player's half of the frame, and cut to it
    Half(bool),
}

/// A texture in TMU memory: a layer of the texture array.
struct Texture {
    layer: u32,
    w: u32,
    h: u32,
    /// its size in TMU memory
    bytes: u32,
    /// when it was last downloaded (a count)
    stamp: u64,
}

/// Layers of the texture array: TMU memory holds 64 textures of 256x256 at 8 bits.
const LAYERS: u32 = 64;
/// Levels of a 256x256 texture: 256 ... 1.
const LEVELS: i32 = 9;

/// Texture filtering when the game filters: 0 the original (bilinear, no mipmaps),
/// 1 trilinear mipmaps, 2...16 also anisotropic.
pub const FILTER_CHOICES: &[u32] = &[0, 1, 2, 4, 8, 16];
static FILTER: AtomicU32 = AtomicU32::new(16);

/// The saved choice (default: anisotropic 16x); `NSTRIKE_FILTER=<n>` overrides it.
pub fn filtering() -> u32 {
    if let Some(n) = std::env::var("NSTRIKE_FILTER").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    let v = super::advapi32::value("", "TextureFilter").and_then(|(_, d)| Some(u32::from_le_bytes(d.get(..4)?.try_into().ok()?)));
    v.filter(|v| FILTER_CHOICES.contains(v)).unwrap_or(16)
}

/// Multisample anti-aliasing: samples per pixel (0 off).
pub const MSAA_CHOICES: &[u32] = &[0, 2, 4, 8];
static MSAA: AtomicU32 = AtomicU32::new(4);

/// The saved choice (REG_DWORD `Msaa`, default 4x); `NSTRIKE_MSAA=<n>` overrides it.
pub fn msaa() -> u32 {
    if let Some(n) = std::env::var("NSTRIKE_MSAA").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    let v = super::advapi32::value("", "Msaa").and_then(|(_, d)| Some(u32::from_le_bytes(d.get(..4)?.try_into().ok()?)));
    v.filter(|v| MSAA_CHOICES.contains(v)).unwrap_or(4)
}

/// Preferences' OK: saved, and used from the next frame on.
pub fn set_msaa(n: u32) {
    super::advapi32::set_value("", "Msaa", 4, &n.to_le_bytes());
    MSAA.store(n, Ordering::Relaxed);
}

/// At start, after the EXE is loaded.
pub fn init() {
    FILTER.store(filtering(), Ordering::Relaxed);
    MSAA.store(msaa(), Ordering::Relaxed);
}

/// Preferences' OK: saved, and used from the next batch on.
pub fn set_filtering(n: u32) {
    super::advapi32::set_value("", "TextureFilter", 4, &n.to_le_bytes());
    FILTER.store(n, Ordering::Relaxed);
}

struct Glide {
    /// the Glide screen
    res: (u32, u32),
    /// the texture drawn into
    target: (u32, u32),
    /// the vertices' coordinate space
    space: (u32, u32),
    view: View,
    /// the texture shown, and its framebuffer
    fbo: glow::Framebuffer,
    color: glow::Texture,
    /// with MSAA: the multisampled framebuffer drawn into, its renderbuffer and samples;
    /// resolved into `color` at grBufferSwap
    ms: Option<(glow::Framebuffer, glow::Renderbuffer, u32)>,
    ms_max: u32,
    prog: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    /// every texture, 256x256 at most, in one array: rgb premultiplied by coverage and the
    /// coverage, with mipmaps (see the module)
    array: glow::Texture,
    /// the textures' alpha: its share in covered texels, and in keyed ones
    alpha: glow::Texture,
    /// a download changed level 0: the mipmaps are built before the next draw
    stale: bool,
    /// the anisotropy set on the arrays, and the most the driver takes
    aniso: f32,
    aniso_max: f32,
    textures: BTreeMap<u32, Texture>,
    downloads: u64,
    state: State,
    /// Vertex attributes, so that changing them draws nothing yet: the colour and alpha
    /// combine units (function, factor, local, other, invert), the constant colour (ARGB) and
    /// the TMU address of the texture
    cc: [u32; 5],
    ac: [u32; 5],
    constant: u32,
    tex: Option<u32>,
    /// uniform locations
    uniforms: Uniforms,
    /// vertices waiting to be drawn with `state` (`VERTEX` words each): x y r g b a s t,
    /// constant r g b a (floats), the combine units and texture (see `add`), then the
    /// rectangle of the primitive's texture coordinates s0 t0 s1 t1 (floats)
    batch: Vec<u32>,
    batch_mode: u32,
    gamma: f32,
}

thread_local! {
    static GLIDE: RefCell<Option<Glide>> = const { RefCell::new(None) };
}

const VS: &str = r#"
uniform vec2 space;
layout(location = 0) in vec2 pos;
layout(location = 1) in vec4 col;
layout(location = 2) in vec2 tc;
layout(location = 3) in vec4 kc;
layout(location = 4) in uvec4 mode;
layout(location = 5) in vec4 box;
out vec4 vcol;
noperspective out vec2 st;
flat out vec4 konst;
flat out uvec4 vmode;
flat out vec4 st_box;
void main() {
    vcol = col / 255.0;
    konst = kc;
    st_box = box;
    vmode = mode;
    st = tc;
    gl_Position = vec4(pos.x / space.x * 2.0 - 1.0, 1.0 - pos.y / space.y * 2.0, 0.0, 1.0);
}
"#;

const FS: &str = r#"
uniform sampler2DArray tex;     // rgb * coverage, coverage
uniform sampler2DArray tex_a;   // alpha * coverage, alpha * (1 - coverage)
uniform int mipmaps;            // filtered by the GPU with mipmaps
uniform ivec2 clamp_st;
uniform int chroma;
uniform vec3 key;
uniform int linear_filter;
uniform int edge;      // how the chroma key's soft edge is applied (see `flush`)
in vec4 vcol;
noperspective in vec2 st;
flat in vec4 konst;    // the constant colour
flat in uvec4 vmode;   // combine units and texture, packed by `add`
flat in vec4 st_box;   // the primitive's texture rectangle
out vec4 color;

ivec4 cc;              // colour combine: function, factor, local, other
int cc_inv;
ivec4 ac;              // alpha combine
int ac_inv;
int use_tex;
int layer;
ivec2 tex_size;

ivec4 bytes4(uint w) { return ivec4(w & 255u, (w >> 8) & 255u, (w >> 16) & 255u, w >> 24); }

vec4 texel(ivec2 p) {
    ivec2 s = tex_size;
    p = ivec2(clamp_st.x != 0 ? clamp(p.x, 0, s.x - 1) : ((p.x % s.x) + s.x) % s.x,
              clamp_st.y != 0 ? clamp(p.y, 0, s.y - 1) : ((p.y % s.y) + s.y) % s.y);
    vec4 a = texelFetch(tex_a, ivec3(p, layer), 0);
    return vec4(texelFetch(tex, ivec3(p, layer), 0).rgb, a.r + a.g);
}

// The GPU's trilinear (and anisotropic) filter: inside the primitive's texture rectangle,
// at most at the level where that still spans 4 texels (a page is an atlas).
vec4 sample_mip(out float cover) {
    vec2 size = vec2(textureSize(tex, 0).xy);
    vec2 dx = dFdx(st), dy = dFdy(st);
    vec2 lo = st_box.xy, hi = st_box.zw;
    float lod = log2(max(max(length(dx), length(dy)), 1e-8));
    float top = max(log2(max(min(hi.x - lo.x, hi.y - lo.y), 1.0)) - 2.0, 0.0);
    if (lod > top) {
        float k = exp2(top - lod);
        dx *= k;
        dy *= k;
        lod = top;
    }
    vec2 m = vec2(0.5 * exp2(max(lod, 0.0)));
    vec2 p = clamp(st, lo + min(m, (hi - lo) * 0.5), hi - min(m, (hi - lo) * 0.5));
    vec3 uv = vec3(p / size, float(layer));
    vec4 c = textureGrad(tex, uv, dx / size, dy / size);
    vec4 a = textureGrad(tex_a, uv, dx / size, dy / size);
    if (chroma == 0) {
        cover = 1.0;
        return vec4(c.rgb, a.r + a.g);
    }
    cover = c.a;
    return c.a > 0.0 ? vec4(c.rgb / c.a, a.r / c.a) : vec4(0.0);
}

bool keyed(vec3 c) { return chroma != 0 && all(lessThan(abs(c - key), vec3(0.5 / 255.0))); }

// the texture colour and how much of it is not keyed out
vec4 sample_tex(out float cover) {
    if (linear_filter != 0 && mipmaps != 0) return sample_mip(cover);
    if (linear_filter == 0) {
        vec4 t = texel(ivec2(floor(st)));
        cover = keyed(t.rgb) ? 0.0 : 1.0;
        return t;
    }
    vec2 p = st - 0.5;
    ivec2 i = ivec2(floor(p));
    vec2 f = p - floor(p);
    vec4 sum = vec4(0.0);
    float wsum = 0.0;
    for (int k = 0; k < 4; k++) {
        ivec2 o = ivec2(k & 1, k >> 1);
        vec4 t = texel(i + o);
        float w = (o.x == 1 ? f.x : 1.0 - f.x) * (o.y == 1 ? f.y : 1.0 - f.y);
        if (keyed(t.rgb)) w = 0.0;
        sum += t * w;
        wsum += w;
    }
    cover = wsum;
    return wsum > 0.0 ? sum / wsum : vec4(0.0);
}

vec3 factor3(int f, vec3 local, float local_a, float other_a, float tex_a) {
    if (f == 0) return vec3(0.0);
    if (f == 1) return local;
    if (f == 2) return vec3(other_a);
    if (f == 3) return vec3(local_a);
    if (f == 4) return vec3(tex_a);
    if (f == 8) return vec3(1.0);
    if (f == 9) return 1.0 - local;
    if (f == 10) return vec3(1.0 - other_a);
    if (f == 11) return vec3(1.0 - local_a);
    if (f == 12) return vec3(1.0 - tex_a);
    return vec3(1.0);
}

vec3 combine3(int fn_, vec3 f, vec3 local, float local_a, vec3 other) {
    if (fn_ == 0) return vec3(0.0);
    if (fn_ == 1) return local;
    if (fn_ == 2) return vec3(local_a);
    if (fn_ == 3) return f * other;
    if (fn_ == 4) return f * other + local;
    if (fn_ == 5) return f * other + local_a;
    if (fn_ == 6) return f * (other - local);
    if (fn_ == 7) return f * (other - local) + local;
    if (fn_ == 8) return f * (other - local) + local_a;
    if (fn_ == 9) return -f * local + local;
    if (fn_ == 16) return -f * local + local_a;
    return local;
}

void main() {
    cc = bytes4(vmode.x);
    ac = bytes4(vmode.y);
    cc_inv = int(vmode.z & 1u);
    ac_inv = int((vmode.z >> 1) & 1u);
    use_tex = int((vmode.z >> 2) & 1u);
    layer = int(vmode.z >> 8);
    tex_size = ivec2(vmode.w & 0xffffu, vmode.w >> 16);
    float cover = 1.0;
    vec4 t = use_tex != 0 ? sample_tex(cover) : vec4(1.0);
    // alpha first: colour factors may use it
    float a_local = ac.z == 0 ? vcol.a : konst.a;
    float a_other = ac.w == 0 ? vcol.a : ac.w == 1 ? t.a : konst.a;
    float af = factor3(ac.y, vec3(a_local), a_local, a_other, t.a).x;
    float a = clamp(combine3(ac.x, vec3(af), vec3(a_local), a_local, vec3(a_other)).x, 0.0, 1.0);
    if (ac_inv != 0) a = 1.0 - a;
    vec3 c_local = cc.z == 0 ? vcol.rgb : konst.rgb;
    vec3 c_other = cc.w == 0 ? vcol.rgb : cc.w == 1 ? t.rgb : konst.rgb;
    // the chroma key compares the colour unit's "other" input
    if (cc.w != 1 && keyed(c_other)) discard;
    if (cc.w != 1) cover = 1.0;
    vec3 cf = factor3(cc.y, c_local, a_local, a_other, t.a);
    vec3 c = clamp(combine3(cc.x, cf, c_local, a_local, c_other), 0.0, 1.0);
    if (cc_inv != 0) c = 1.0 - c;
    if (edge == 0 && cover < 0.5) discard;
    if (edge == 1) a = cover;
    if (edge == 2) a *= cover;
    if (edge == 3) c *= cover;
    if (cover <= 0.0) discard;
    color = vec4(c, a);
}
"#;

/// Words per batched vertex.
const VERTEX: usize = 20;

/// The shaders' uniforms, looked up once.
struct Uniforms {
    space: Option<glow::UniformLocation>,
    tex: Option<glow::UniformLocation>,
    tex_a: Option<glow::UniformLocation>,
    mipmaps: Option<glow::UniformLocation>,
    clamp_st: Option<glow::UniformLocation>,
    chroma: Option<glow::UniformLocation>,
    key: Option<glow::UniformLocation>,
    linear_filter: Option<glow::UniformLocation>,
    edge: Option<glow::UniformLocation>,
}

impl Uniforms {
    fn new(gl: &glow::Context, prog: glow::Program) -> Uniforms {
        let u = |n: &str| unsafe { gl.get_uniform_location(prog, n) };
        Uniforms {
            space: u("space"),
            tex: u("tex"),
            tex_a: u("tex_a"),
            mipmaps: u("mipmaps"),
            clamp_st: u("clamp_st"),
            chroma: u("chroma"),
            key: u("key"),
            linear_filter: u("linear_filter"),
            edge: u("edge"),
        }
    }
}

fn with<R>(f: impl FnOnce(&mut Glide, &glow::Context) -> R) -> Option<R> {
    GLIDE.with(|g| {
        let mut g = g.borrow_mut();
        let g = g.as_mut()?;
        super::window::with_gl(|gl| f(g, gl))
    })
}

fn f32_arg(c: &Cpu, n: u32) -> f32 {
    f32::from_bits(arg(c, n))
}

/// Size of a texture level: LOD 0 is 256, 8 is 1; aspect 0..6 is 8:1 .. 1:8.
fn lod_size(lod: u32, aspect: u32) -> (u32, u32) {
    let l = 256 >> lod.min(8);
    match aspect {
        0..=3 => (l, (l >> (3 - aspect)).max(1)),
        _ => ((l >> (aspect - 3)).max(1), l),
    }
}

/// Texels of a format as RGBA bytes (alpha 255 where the format has none).
fn convert(format: u32, data: u32, n: usize) -> Vec<u8> {
    let m = mem();
    let e5 = |v: u32| ((v << 3) | (v >> 2)) as u8;
    let e6 = |v: u32| ((v << 2) | (v >> 4)) as u8;
    let e4 = |v: u32| (v * 17) as u8;
    let mut out = Vec::with_capacity(n * 4);
    for k in 0..n as u32 {
        let px: [u8; 4] = match format {
            0xb => {
                let v = m.rd16(data + 2 * k) as u32;
                [e5(v >> 10 & 31), e5(v >> 5 & 31), e5(v & 31), if v & 0x8000 != 0 { 255 } else { 0 }]
            }
            0xa => {
                let v = m.rd16(data + 2 * k) as u32;
                [e5(v >> 11), e6(v >> 5 & 63), e5(v & 31), 255]
            }
            0xc => {
                let v = m.rd16(data + 2 * k) as u32;
                [e4(v >> 8 & 15), e4(v >> 4 & 15), e4(v & 15), e4(v >> 12)]
            }
            0xd => {
                let v = m.rd16(data + 2 * k);
                let i = v as u8;
                [i, i, i, (v >> 8) as u8]
            }
            0 => {
                let v = m.rd8(data + k) as u32;
                [(v >> 5) as u8 * 36, (v >> 2 & 7) as u8 * 36, (v & 3) as u8 * 85, 255]
            }
            2 => [255, 255, 255, m.rd8(data + k)],
            3 => {
                let i = m.rd8(data + k);
                [i, i, i, 255]
            }
            _ => [255, 0, 255, 255],
        };
        out.extend(px);
    }
    out
}

fn bytes_per_texel(format: u32) -> u32 {
    if format >= 8 { 2 } else { 1 }
}

impl Glide {
    fn open(gl: &glow::Context, w: u32, h: u32) -> Glide {
        let target = (w, w * 3 / 4);
        unsafe {
            let color = texture(gl, true);
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, target.0 as i32, target.1 as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            let fbo = gl.create_framebuffer().expect("GL framebuffer");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(color), 0);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            let prog = compile(gl, VS, FS);
            let vao = gl.create_vertex_array().expect("GL vertex array");
            let vbo = gl.create_buffer().expect("GL buffer");
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            for (loc, n, off) in [(0, 2, 0), (1, 4, 8), (2, 2, 24), (3, 4, 32)] {
                gl.enable_vertex_attrib_array(loc);
                gl.vertex_attrib_pointer_f32(loc, n, glow::FLOAT, false, 4 * VERTEX as i32, off);
            }
            gl.enable_vertex_attrib_array(4);
            gl.vertex_attrib_pointer_i32(4, 4, glow::UNSIGNED_INT, 4 * VERTEX as i32, 48);
            gl.enable_vertex_attrib_array(5);
            gl.vertex_attrib_pointer_f32(5, 4, glow::FLOAT, false, 4 * VERTEX as i32, 64);
            let aniso_max = if gl.supported_extensions().iter().any(|e| e.ends_with("texture_filter_anisotropic")) {
                gl.get_parameter_f32(glow::MAX_TEXTURE_MAX_ANISOTROPY_EXT)
            } else {
                1.0
            };
            let layers = |format: u32| {
                let t = gl.create_texture().expect("GL texture");
                gl.bind_texture(glow::TEXTURE_2D_ARRAY, Some(t));
                gl.tex_parameter_i32(glow::TEXTURE_2D_ARRAY, glow::TEXTURE_MIN_FILTER, glow::LINEAR_MIPMAP_LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D_ARRAY, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                for p in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D_ARRAY, p, glow::CLAMP_TO_EDGE as i32);
                }
                gl.tex_storage_3d(glow::TEXTURE_2D_ARRAY, LEVELS, format, 256, 256, LAYERS as i32);
                t
            };
            let array = layers(glow::RGBA8);
            let alpha = layers(glow::RG8);
            Glide {
                res: (w, h),
                target,
                space: (w, h),
                view: View::Full,
                fbo,
                color,
                ms: None,
                ms_max: gl.get_parameter_i32(glow::MAX_SAMPLES).max(0) as u32,
                prog,
                vao,
                vbo,
                array,
                alpha,
                stale: false,
                aniso: 0.0,
                aniso_max,
                textures: BTreeMap::new(),
                downloads: 0,
                cc: [1, 0, 0, 0, 0],
                ac: [1, 0, 0, 0, 0],
                constant: 0xffff_ffff,
                tex: None,
                uniforms: Uniforms::new(gl, prog),
                state: State {
                    blend: (4, 0),
                    chroma: false,
                    key: 0,
                    linear: false,
                    clamp: (true, true),
                    clip: [0, 0, w as i32, h as i32],
                },
                batch: Vec::new(),
                batch_mode: glow::TRIANGLES,
                gamma: 1.0,
            }
        }
    }

    /// The view's rectangle in the target (x, y from the top, w, h) and the columns it is cut to.
    fn view_rect(&self) -> ([f32; 4], (f32, f32)) {
        let (tw, th) = (self.target.0 as f32, self.target.1 as f32);
        let w43 = (th * 4.0 / 3.0).min(tw);
        match self.view {
            View::Full => ([0.0, 0.0, tw, th], (0.0, tw)),
            View::Hud(at) => ([(tw - w43) * at, 0.0, w43, th], (0.0, tw)),
            View::Half(false) => ([tw / 4.0 - w43 / 4.0, 0.0, w43, th], (0.0, tw / 2.0)),
            View::Half(true) => ([tw * 0.75 - w43 * 0.75, 0.0, w43, th], (tw / 2.0, tw)),
        }
    }

    /// GL's viewport for the view, and a clip rectangle (x0 y0 x1 y1 in screen pixels) as
    /// its scissor box.
    fn scissor(&self, gl: &glow::Context, c: [i32; 4]) {
        let ([vx, vy, vw, vh], (cut0, cut1)) = self.view_rect();
        let th = self.target.1 as f32;
        let (sx, sy) = (vw / self.res.0 as f32, vh / self.res.1 as f32);
        let x0 = (vx + c[0] as f32 * sx).max(cut0).round() as i32;
        let x1 = (vx + c[2] as f32 * sx).min(cut1).round() as i32;
        let (y0, y1) = ((vy + c[1] as f32 * sy).round() as i32, (vy + c[3] as f32 * sy).round() as i32);
        unsafe {
            gl.viewport(vx.round() as i32, (th - vy - vh).round() as i32, vw.round() as i32, vh.round() as i32);
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(x0, th as i32 - y1, (x1 - x0).max(0), (y1 - y0).max(0));
        }
    }

    /// Size the target (between frames): as tall as the render size `h` (the window's or the
    /// chosen resolution: high resolution), as wide as the aspect `a` makes it.
    fn resize(&mut self, gl: &glow::Context, a: f32, h: u32) {
        let h = h.clamp(240, 4320);
        let w = (h as f32 * a).round() as u32;
        if (w, h) == self.target {
            // the samples may have changed in Preferences
            self.multisample(gl, false);
            return;
        }
        self.target = (w, h);
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.color));
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w as i32, h as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.disable(glow::SCISSOR_TEST);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        self.multisample(gl, true);
    }

    /// The multisampled framebuffer for the chosen samples and the target's size (`resized`:
    /// the target changed).
    fn multisample(&mut self, gl: &glow::Context, resized: bool) {
        let samples = MSAA.load(Ordering::Relaxed).min(self.ms_max);
        let samples = if samples < 2 { 0 } else { samples };
        if !resized && self.ms.map_or(0, |m| m.2) == samples {
            return;
        }
        unsafe {
            if let Some((fbo, rb, _)) = self.ms.take() {
                gl.delete_framebuffer(fbo);
                gl.delete_renderbuffer(rb);
            }
            if samples == 0 {
                return;
            }
            let rb = gl.create_renderbuffer().expect("GL renderbuffer");
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(rb));
            gl.renderbuffer_storage_multisample(glow::RENDERBUFFER, samples as i32, glow::RGBA8, self.target.0 as i32, self.target.1 as i32);
            let fbo = gl.create_framebuffer().expect("GL framebuffer");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::RENDERBUFFER, Some(rb));
            gl.disable(glow::SCISSOR_TEST);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            self.ms = Some((fbo, rb, samples));
        }
    }

    /// The framebuffer drawn into.
    fn draw_fbo(&self) -> glow::Framebuffer {
        self.ms.map_or(self.fbo, |m| m.0)
    }

    /// The multisampled picture into the texture shown.
    fn resolve(&self, gl: &glow::Context) {
        if let Some((fbo, _, _)) = self.ms {
            let (w, h) = (self.target.0 as i32, self.target.1 as i32);
            unsafe {
                gl.disable(glow::SCISSOR_TEST);
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fbo));
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.fbo));
                gl.blit_framebuffer(0, 0, w, h, 0, 0, w, h, glow::COLOR_BUFFER_BIT, glow::NEAREST);
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            }
        }
    }

    /// Draw what is batched with the current state.
    fn flush(&mut self, gl: &glow::Context) {
        if self.batch.is_empty() {
            return;
        }
        let s = self.state;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.draw_fbo()));
            self.scissor(gl, s.clip);
            gl.use_program(Some(self.prog));
            gl.bind_vertex_array(Some(self.vao));
            let u = &self.uniforms;
            gl.uniform_2_f32(u.space.as_ref(), self.space.0 as f32, self.space.1 as f32);
            let filter = FILTER.load(Ordering::Relaxed);
            let mipmaps = filter != 0 && s.key & 0xff_ffff == 0;
            gl.uniform_1_i32(u.mipmaps.as_ref(), mipmaps as i32);
            gl.uniform_1_i32(u.tex_a.as_ref(), 1);
            gl.uniform_1_i32(u.tex.as_ref(), 0);
            let aniso = (filter.max(1) as f32).min(self.aniso_max);
            for (k, t) in [(glow::TEXTURE1, self.alpha), (glow::TEXTURE0, self.array)] {
                gl.active_texture(k);
                gl.bind_texture(glow::TEXTURE_2D_ARRAY, Some(t));
                if mipmaps && self.stale {
                    gl.generate_mipmap(glow::TEXTURE_2D_ARRAY);
                }
                if aniso != self.aniso && self.aniso_max > 1.0 {
                    gl.tex_parameter_f32(glow::TEXTURE_2D_ARRAY, glow::TEXTURE_MAX_ANISOTROPY_EXT, aniso);
                }
            }
            if mipmaps {
                self.stale = false;
            }
            self.aniso = aniso;
            gl.uniform_2_i32(u.clamp_st.as_ref(), s.clamp.0 as i32, s.clamp.1 as i32);
            gl.uniform_1_i32(u.chroma.as_ref(), s.chroma as i32);
            let key = s.key;
            gl.uniform_3_f32(u.key.as_ref(), (key >> 16 & 0xff) as f32 / 255.0, (key >> 8 & 0xff) as f32 / 255.0, (key & 0xff) as f32 / 255.0);
            gl.uniform_1_i32(u.linear_filter.as_ref(), s.linear as i32);
            // blending: Glide factors 0 zero, 1 src alpha, 2 src/dst colour, 3 dst alpha, 4 one,
            // 5 1 - src alpha, 6 1 - src/dst colour, 7 1 - dst alpha
            let factor = |f: u32, dst: bool| match f {
                0 => glow::ZERO,
                1 => glow::SRC_ALPHA,
                2 => if dst { glow::SRC_COLOR } else { glow::DST_COLOR },
                3 => glow::DST_ALPHA,
                5 => glow::ONE_MINUS_SRC_ALPHA,
                6 => if dst { glow::ONE_MINUS_SRC_COLOR } else { glow::ONE_MINUS_DST_COLOR },
                7 => glow::ONE_MINUS_DST_ALPHA,
                _ => glow::ONE,
            };
            let (src, dst) = (factor(s.blend.0, false), factor(s.blend.1, true));
            let opaque = src == glow::ONE && dst == glow::ZERO;
            let soft = s.chroma && s.linear;
            // the soft chroma key edge: 0 cut at half, 1 alpha = cover (blending turned on for
            // it), 2 alpha *= cover, 3 rgb *= cover (additive). Untextured vertices have a
            // cover of 1, which leaves them as they are.
            let edge = if !soft {
                0
            } else if opaque {
                1
            } else if src == glow::SRC_ALPHA || dst == glow::ONE_MINUS_SRC_ALPHA {
                2
            } else if src == glow::ONE && dst == glow::ONE {
                3
            } else {
                0
            };
            gl.uniform_1_i32(u.edge.as_ref(), edge);
            if opaque && edge != 1 {
                gl.disable(glow::BLEND);
            } else {
                gl.enable(glow::BLEND);
                if edge == 1 {
                    gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                } else {
                    gl.blend_func(src, dst);
                }
            }
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            let bytes = std::slice::from_raw_parts(self.batch.as_ptr() as *const u8, self.batch.len() * 4);
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
            gl.draw_arrays(self.batch_mode, 0, (self.batch.len() / VERTEX) as i32);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
        }
        self.batch.clear();
    }

    /// Change the state, drawing what was batched with the old one first.
    fn set(&mut self, gl: &glow::Context, f: impl FnOnce(&mut State)) {
        let mut s = self.state;
        f(&mut s);
        if s != self.state {
            self.flush(gl);
            self.state = s;
        }
    }

    /// Add vertices (GrVertex at guest addresses) as primitives of `mode`.
    fn add(&mut self, gl: &glow::Context, mode: u32, verts: &[u32]) {
        if mode != self.batch_mode {
            self.flush(gl);
            self.batch_mode = mode;
        }
        let m = mem();
        let k = self.constant;
        let ch = |sh: u32| ((k >> sh & 0xff) as f32 / 255.0).to_bits();
        let konst = [ch(16), ch(8), ch(0), ch(24)];
        // the combine units a byte each, then invert bits, texturing and the layer, its size
        let bytes = |u: [u32; 5]| u[0] | u[1] << 8 | u[2] << 16 | u[3] << 24;
        let tex = self.tex.and_then(|a| self.textures.get(&a));
        let mode = [
            bytes(self.cc),
            bytes(self.ac),
            (self.cc[4] & 1) | (self.ac[4] & 1) << 1 | (tex.is_some() as u32) << 2 | tex.map_or(0, |t| t.layer) << 8,
            tex.map_or(0, |t| t.w | t.h << 16),
        ];
        // x y z r g b ooz a oow, then tmu 0: sow tow oow
        let st = |v: u32| {
            let oow = f32::from_bits(m.rd32(v + 0x2c));
            let oow = if oow != 0.0 { oow } else { 1.0 };
            (f32::from_bits(m.rd32(v + 0x24)) / oow, f32::from_bits(m.rd32(v + 0x28)) / oow)
        };
        let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for &v in verts {
            let (s, t) = st(v);
            b = [b[0].min(s), b[1].min(t), b[2].max(s), b[3].max(t)];
        }
        let b = b.map(f32::to_bits);
        let xy = |v: u32| (f32::from_bits(m.rd32(v)), f32::from_bits(m.rd32(v + 4)));
        // a line: a quad as wide as the game's line at its resolution
        let quad = if let [v0, v1] = *verts { Some(self.line_offsets(xy(v0), xy(v1))) } else { None };
        let mut put = |v: u32, (x, y): (f32, f32)| {
            let (s, t) = st(v);
            self.batch.extend([x.to_bits(), y.to_bits(), m.rd32(v + 0xc), m.rd32(v + 0x10), m.rd32(v + 0x14), m.rd32(v + 0x1c), s.to_bits(), t.to_bits()]);
            self.batch.extend(konst);
            self.batch.extend(mode);
            self.batch.extend(b);
        };
        if let (Some([n0, n1, n2, n3]), &[v0, v1]) = (quad, verts) {
            put(v0, n0);
            put(v0, n1);
            put(v1, n2);
            put(v1, n2);
            put(v0, n1);
            put(v1, n3);
        } else {
            for &v in verts {
                put(v, xy(v));
            }
        }
    }

    /// The corners of a line from `a` to `b` (in the vertices' space) drawn as a quad: a pixel
    /// wide at 720 lines (the laser sight: 1 pixel stayed hair-thin at high resolution), at
    /// least one; past the ends by half that, as GL draws lines.
    fn line_offsets(&self, a: (f32, f32), b: (f32, f32)) -> [(f32, f32); 4] {
        let ([_, _, vw, vh], _) = self.view_rect();
        // target pixels per unit of the vertices' space
        let (kx, ky) = (vw / self.space.0 as f32, vh / self.space.1 as f32);
        let width = (self.target.1 as f32 / 720.0).round().max(1.0);
        let (dx, dy) = ((b.0 - a.0) * kx, (b.1 - a.1) * ky);
        let len = (dx * dx + dy * dy).sqrt();
        let (ux, uy) = if len > 0.0 { (dx / len, dy / len) } else { (1.0, 0.0) };
        let h = width / 2.0;
        // along (u) and across (n = u turned) in pixels, back to the space's units
        let p = |o: (f32, f32), along: f32, across: f32| (o.0 + (ux * along - uy * across) / kx, o.1 + (uy * along + ux * across) / ky);
        [p(a, -h, -h), p(a, -h, h), p(b, h, -h), p(b, h, h)]
    }

    /// Texels into the texture at TMU address `addr` (rows `rows` of its top level, or all).
    fn download(&mut self, gl: &glow::Context, addr: u32, lod: u32, aspect: u32, format: u32, data: u32, rows: Option<(u32, u32)>) {
        self.flush(gl);
        let (w, h) = lod_size(lod, aspect);
        let bytes = w * h * bytes_per_texel(format);
        self.downloads += 1;
        let stamp = self.downloads;
        let layer = match self.textures.get_mut(&addr) {
            Some(t) if (t.w, t.h, t.bytes) == (w, h, bytes) => {
                t.stamp = stamp;
                t.layer
            }
            _ => {
                // what it overwrites in TMU memory is gone; then a free layer, or the one
                // downloaded longest ago
                self.textures.retain(|&a, t| a + t.bytes <= addr || addr + bytes <= a);
                let layer = (0..LAYERS).find(|l| self.textures.values().all(|t| t.layer != *l)).unwrap_or_else(|| {
                    let (&a, t) = self.textures.iter().min_by_key(|t| t.1.stamp).unwrap();
                    let l = t.layer;
                    self.textures.remove(&a);
                    l
                });
                self.textures.insert(addr, Texture { layer, w, h, bytes, stamp });
                layer
            }
        };
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D_ARRAY, Some(self.array));
            let (r0, r1) = rows.unwrap_or((0, h - 1));
            let r1 = r1.min(h - 1);
            if r0 > r1 {
                return;
            }
            let n = ((r1 - r0 + 1) * w) as usize;
            let px = convert(format, data, n);
            // premultiplied by coverage: a black texel is keyed out
            let mut rgb = Vec::with_capacity(n * 4);
            let mut alpha = Vec::with_capacity(n * 2);
            for p in px.chunks(4) {
                let covered = p[..3] != [0, 0, 0];
                rgb.extend(if covered { [p[0], p[1], p[2], 255] } else { [0; 4] });
                alpha.extend(if covered { [p[3], 0] } else { [0, p[3]] });
            }
            for (t, format, px) in [(self.array, glow::RGBA, &rgb), (self.alpha, glow::RG, &alpha)] {
                gl.bind_texture(glow::TEXTURE_2D_ARRAY, Some(t));
                gl.tex_sub_image_3d(
                    glow::TEXTURE_2D_ARRAY,
                    0,
                    0,
                    r0 as i32,
                    layer as i32,
                    w as i32,
                    (r1 - r0 + 1) as i32,
                    1,
                    format,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(px)),
                );
            }
            self.stale = true;
        }
    }
}

// ---- the Glide functions ---------------------------------------------------------------------

fn ret1(_: &mut Cpu) -> u32 {
    1
}
fn ret0(_: &mut Cpu) -> u32 {
    0
}

fn sst_query_boards(c: &mut Cpu) -> u32 {
    mem().wr32(arg(c, 0), 1);
    1
}

fn sst_query_hardware(c: &mut Cpu) -> u32 {
    let p = arg(c, 0);
    let m = mem();
    m.slice(p, 0x40).fill(0);
    // one Voodoo with 6 MB frame buffer memory (the game offers up to 1280x960 for it)
    for (off, v) in [(0, 1), (4, 0), (8, 6), (0xc, 2), (0x10, 1), (0x14, 0), (0x18, 1), (0x1c, TMU_RAM >> 20)] {
        m.wr32(p + off, v);
    }
    1
}

fn sst_win_open(c: &mut Cpu) -> u32 {
    let res = arg(c, 1);
    let (w, h) = match res {
        0 => (320, 200),
        1 => (320, 240),
        2 => (400, 256),
        3 => (512, 384),
        4 => (640, 200),
        5 => (640, 350),
        6 => (640, 400),
        7 => (640, 480),
        8 => (800, 600),
        9 => (960, 720),
        0xa => (856, 480),
        0xb => (512, 256),
        0xc => (1024, 768),
        0xd => (1280, 1024),
        0xe => (1600, 1200),
        _ => (640, 480),
    };
    log!("glide: grSstWinOpen {w}x{h}");
    let mut g = super::window::with_gl(|gl| Glide::open(gl, w, h));
    if let Some(g) = &mut g {
        let (_, wh) = super::window::window_size();
        super::window::with_gl(|gl| g.resize(gl, ASPECT, wh));
    }
    let ok = g.is_some();
    GLIDE.with(|s| *s.borrow_mut() = g);
    ok as u32
}

fn sst_win_close(_: &mut Cpu) -> u32 {
    with(|g, gl| unsafe {
        if let Some((fbo, rb, _)) = g.ms.take() {
            gl.delete_framebuffer(fbo);
            gl.delete_renderbuffer(rb);
        }
        gl.delete_framebuffer(g.fbo);
        gl.delete_texture(g.color);
        gl.delete_texture(g.array);
        gl.delete_texture(g.alpha);
    });
    GLIDE.with(|s| *s.borrow_mut() = None);
    1
}


fn buffer_clear(c: &mut Cpu) -> u32 {
    let color = arg(c, 0);
    with(|g, gl| {
        g.flush(gl);
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(g.draw_fbo()));
            g.scissor(gl, g.state.clip);
            let ch = |sh: u32| (color >> sh & 0xff) as f32 / 255.0;
            gl.clear_color(ch(16), ch(8), ch(0), 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.disable(glow::SCISSOR_TEST);
        }
    });
    0
}

fn buffer_swap(_: &mut Cpu) -> u32 {
    let shown = with(|g, gl| {
        g.flush(gl);
        g.resolve(gl);
        (g.color, g.target, g.gamma)
    });
    if let Some((tex, (w, h), gamma)) = shown {
        super::window::show_texture(tex, w, h, gamma);
        // the next frame follows the window's height
        let (_, wh) = super::window::window_size();
        with(|g, gl| g.resize(gl, ASPECT, wh));
    }
    0
}

/// The coordinate space of the vertices that follow (None: screen pixels).
pub fn set_space(space: Option<(u32, u32)>) {
    with(|g, gl| {
        let s = space.unwrap_or(g.res);
        if s != g.space {
            g.flush(gl);
            g.space = s;
        }
    });
}

/// Where the vertices that follow go in the frame.
pub fn set_view(view: View) {
    with(|g, gl| {
        if view != g.view {
            g.flush(gl);
            g.view = view;
        }
    });
}

fn draw_triangle(c: &mut Cpu) -> u32 {
    let v = [arg(c, 0), arg(c, 1), arg(c, 2)];
    with(|g, gl| g.add(gl, glow::TRIANGLES, &v));
    0
}

fn draw_line(c: &mut Cpu) -> u32 {
    let v = [arg(c, 0), arg(c, 1)];
    with(|g, gl| g.add(gl, glow::TRIANGLES, &v));
    0
}

fn draw_point(c: &mut Cpu) -> u32 {
    let v = [arg(c, 0)];
    with(|g, gl| g.add(gl, glow::POINTS, &v));
    0
}

fn clip_window(c: &mut Cpu) -> u32 {
    let r = [arg(c, 0) as i32, arg(c, 1) as i32, arg(c, 2) as i32, arg(c, 3) as i32];
    with(|g, gl| g.set(gl, |s| s.clip = r));
    0
}

fn combine(c: &Cpu) -> [u32; 5] {
    [arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4)]
}

fn color_combine(c: &mut Cpu) -> u32 {
    let v = combine(c);
    with(|g, _| g.cc = v);
    0
}

fn alpha_combine(c: &mut Cpu) -> u32 {
    let v = combine(c);
    with(|g, _| g.ac = v);
    0
}

fn tex_combine(c: &mut Cpu) -> u32 {
    // grTexCombine(tmu, rgb function, factor, alpha function, factor, rgb invert, alpha invert):
    // the game passes the texture through (LOCAL, NONE)
    let (rgb, alpha) = (arg(c, 1), arg(c, 3));
    if rgb != 1 || alpha != 1 {
        log!("warn: grTexCombine({rgb}, {alpha}) taken as LOCAL");
    }
    0
}

fn constant_color(c: &mut Cpu) -> u32 {
    let v = arg(c, 0);
    with(|g, _| g.constant = v);
    0
}

fn alpha_blend(c: &mut Cpu) -> u32 {
    let b = (arg(c, 0), arg(c, 1));
    with(|g, gl| g.set(gl, |s| s.blend = b));
    0
}

fn chromakey_mode(c: &mut Cpu) -> u32 {
    let on = arg(c, 0) != 0;
    with(|g, gl| g.set(gl, |s| s.chroma = on));
    0
}

fn chromakey_value(c: &mut Cpu) -> u32 {
    let k = arg(c, 0);
    with(|g, gl| g.set(gl, |s| s.key = k));
    0
}

fn tex_filter_mode(c: &mut Cpu) -> u32 {
    let linear = arg(c, 2) != 0;
    with(|g, gl| g.set(gl, |s| s.linear = linear));
    0
}

fn tex_clamp_mode(c: &mut Cpu) -> u32 {
    let cl = (arg(c, 1) != 0, arg(c, 2) != 0);
    with(|g, gl| g.set(gl, |s| s.clamp = cl));
    0
}

fn gamma_value(c: &mut Cpu) -> u32 {
    let v = f32_arg(c, 0);
    with(|g, _| g.gamma = if v > 0.0 { v } else { 1.0 });
    0
}

fn tex_min_address(_: &mut Cpu) -> u32 {
    0
}

fn tex_max_address(_: &mut Cpu) -> u32 {
    // the last start address a 256x256 16-bit texture fits at
    TMU_RAM - 0x2_0000
}

/// GrTexInfo: smallLod, largeLod, aspect, format, data
fn tex_info(p: u32) -> (u32, u32, u32, u32, u32) {
    let m = mem();
    (m.rd32(p), m.rd32(p + 4), m.rd32(p + 8), m.rd32(p + 12), m.rd32(p + 16))
}

fn tex_mem_required(info: (u32, u32, u32, u32, u32)) -> u32 {
    let (small, large, aspect, format, _) = info;
    (large..=small.max(large)).map(|l| {
        let (w, h) = lod_size(l, aspect);
        w * h * bytes_per_texel(format)
    }).sum::<u32>().next_multiple_of(8)
}

fn tex_texture_mem_required(c: &mut Cpu) -> u32 {
    tex_mem_required(tex_info(arg(c, 1)))
}

fn tex_calc_mem_required(c: &mut Cpu) -> u32 {
    tex_mem_required((arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), 0))
}

fn tex_download_mipmap(c: &mut Cpu) -> u32 {
    let (addr, info) = (arg(c, 1), tex_info(arg(c, 3)));
    let (_, large, aspect, format, data) = info;
    with(|g, gl| g.download(gl, addr, large, aspect, format, data, None));
    0
}

fn tex_download_mipmap_level(c: &mut Cpu) -> u32 {
    let (addr, this, large, aspect, format, data) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 7));
    if this == large {
        with(|g, gl| g.download(gl, addr, this, aspect, format, data, None));
    }
    0
}

fn tex_download_mipmap_level_partial(c: &mut Cpu) -> u32 {
    let (addr, this, large, aspect, format, data, r0, r1) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 7), arg(c, 8), arg(c, 9));
    if this == large {
        // `data` points at the first row downloaded
        with(|g, gl| g.download(gl, addr, this, aspect, format, data, Some((r0, r1))));
    }
    0
}

fn tex_source(c: &mut Cpu) -> u32 {
    let addr = arg(c, 1);
    with(|g, _| g.tex = Some(addr));
    0
}

fn sst_screen_width(c: &mut Cpu) -> u32 {
    let w = with(|g, _| g.res.0).unwrap_or(640);
    c.r[EDX] = 0;
    w
}

fn sst_screen_height(_: &mut Cpu) -> u32 {
    with(|g, _| g.res.1).unwrap_or(480)
}

/// The Glide functions we implement; anything else the game asks for gets `not_implemented`.
const FUNCTIONS: &[(&str, fn(&mut Cpu) -> u32)] = &[
    ("grGlideInit", ret0),
    ("grGlideShutdown", ret0),
    ("grSstQueryBoards", sst_query_boards),
    ("grSstQueryHardware", sst_query_hardware),
    ("grSstSelect", ret0),
    ("grSstWinOpen", sst_win_open),
    ("grSstWinClose", sst_win_close),
    ("grSstIdle", ret0),
    ("grSstControl", ret1),
    ("grSstScreenWidth", sst_screen_width),
    ("grSstScreenHeight", sst_screen_height),
    ("grBufferClear", buffer_clear),
    ("grBufferSwap", buffer_swap),
    ("grDrawTriangle", draw_triangle),
    ("guDrawTriangleWithClip", draw_triangle),
    ("grDrawLine", draw_line),
    ("grDrawPoint", draw_point),
    ("grClipWindow", clip_window),
    ("grColorCombine", color_combine),
    ("grAlphaCombine", alpha_combine),
    ("grTexCombine", tex_combine),
    ("grConstantColorValue", constant_color),
    ("grAlphaBlendFunction", alpha_blend),
    ("grChromakeyMode", chromakey_mode),
    ("grChromakeyValue", chromakey_value),
    ("grTexFilterMode", tex_filter_mode),
    ("grTexClampMode", tex_clamp_mode),
    ("grTexMipMapMode", ret0),
    ("grDepthBufferMode", ret0),
    ("grDitherMode", ret0),
    ("grGammaCorrectionValue", gamma_value),
    ("grTexMinAddress", tex_min_address),
    ("grTexMaxAddress", tex_max_address),
    ("grTexTextureMemRequired", tex_texture_mem_required),
    ("grTexCalcMemRequired", tex_calc_mem_required),
    ("grTexDownloadMipMap", tex_download_mipmap),
    ("grTexDownloadMipMapLevel", tex_download_mipmap_level),
    ("grTexDownloadMipMapLevelPartial", tex_download_mipmap_level_partial),
    ("grTexSource", tex_source),
];

/// GetProcAddress on glide2x.dll: a thunk for the name (`_grName@argbytes`).
pub fn proc_address(name: &str) -> u32 {
    let plain = name.trim_start_matches('_');
    let (base, bytes) = plain.split_once('@').map_or((plain, 0), |(b, n)| (b, n.parse().unwrap_or(0)));
    let name = format!("glide2x!{base}");
    match FUNCTIONS.iter().find(|e| e.0 == base) {
        Some(&(_, f)) => thunk(&name, (bytes / 4) as i32, f),
        None => thunk_not_implemented(&name, (bytes / 4) as i32),
    }
}
