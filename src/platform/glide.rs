//! The game's 3dfx Glide 2.x renderer on OpenGL. The game loads glide2x.dll itself and takes
//! its functions with GetProcAddress (`proc_address`): every name gets a thunk, the decorated
//! name (`_grDrawTriangle@12`) gives the stdcall argument size.
//!
//! The game draws its depth-sorted triangles with Glide's colour and alpha combine units, alpha
//! blending, the alpha test and a chroma key (black texels are not drawn), textures in TMU
//! memory up to 256x256 (pages in P_8 with a palette, converted again when it changes; movies
//! and pictures as 128x128 ARGB1555 tiles, downloaded row by row), no depth buffer. Its 2D
//! front end reads and writes buffers (grLfbReadRegion/WriteRegion: the background under the
//! cursor, one per buffer, so the front and back buffers really swap). We draw into a texture as tall as the window or the resolution
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
use crate::rt::{Cpu, EAX, EBX, ECX, EDX, ESP};
use glow::HasContext;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

/// The module handle LoadLibraryA gives for glide2x.dll.
pub const MODULE: u32 = 0x1000_0000;

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
    format: u32,
    /// a palettised texture's texels (all of its top level), converted again when the
    /// palette changes (`dirty`)
    raw: Option<Vec<u8>>,
    dirty: bool,
    /// its top level's texels as rgb (black: keyed out) for the glows' colours (`lights.rs`),
    /// and the mean colours of rectangles asked for
    rgb: Vec<u8>,
    means: HashMap<[u32; 4], [f32; 3]>,
    /// its top level as uploaded (premultiplied rgba, alpha and cover), and the rows not yet
    /// uploaded: the game downloads some textures a row at a time, ~30 000 rows a second in a
    /// mission (one upload each was ~5% of the game thread); they go up together when the
    /// texture is next drawn with (`Glide::commit`)
    staged: Vec<u8>,
    staged_alpha: Vec<u8>,
    pending: Option<(u32, u32)>,
}

impl Texture {
    /// The mean colour of a texture rectangle (in texels; keyed out ones count as black), from
    /// 6x6 texels spread over it.
    fn average(&mut self, st: [f32; 4]) -> [f32; 3] {
        let key = st.map(f32::to_bits);
        if let Some(&m) = self.means.get(&key) {
            return m;
        }
        const N: u32 = 6;
        let mut sum = [0u32; 3];
        for j in 0..N {
            for i in 0..N {
                let s = st[0] + (st[2] - st[0]) * (i as f32 + 0.5) / N as f32;
                let t = st[1] + (st[3] - st[1]) * (j as f32 + 0.5) / N as f32;
                let x = (s.floor() as i64).rem_euclid(self.w as i64) as usize;
                let y = (t.floor() as i64).rem_euclid(self.h as i64) as usize;
                let p = 3 * (y * self.w as usize + x);
                for c in 0..3 {
                    sum[c] += self.rgb[p + c] as u32;
                }
            }
        }
        let m = sum.map(|v| v as f32 / (N * N) as f32 / 255.0);
        // rectangles of a texture are few; bounded all the same
        if self.means.len() >= 4096 {
            self.means.clear();
        }
        self.means.insert(key, m);
        m
    }

    /// RGBA texels (`convert`'s) of rows from `r0` into `rgb`.
    fn keep_rgb(&mut self, r0: u32, px: &[u8]) {
        let at = 3 * (r0 * self.w) as usize;
        for (k, p) in px.chunks(4).enumerate() {
            if let Some(d) = self.rgb.get_mut(at + 3 * k..at + 3 * k + 3) {
                d.copy_from_slice(&p[..3]);
            }
        }
        self.means.clear();
    }

    /// RGBA texels of rows `r0..=r1` into the staged texels, to be uploaded at the next use.
    fn stage(&mut self, r0: u32, r1: u32, px: &[u8]) {
        let at = (r0 * self.w) as usize;
        let (rgb, alpha) = (&mut self.staged[at * 4..], &mut self.staged_alpha[at * 2..]);
        // premultiplied by coverage: a black texel is keyed out
        for (k, p) in px.chunks_exact(4).enumerate() {
            let covered = p[..3] != [0, 0, 0];
            rgb[4 * k..4 * k + 4].copy_from_slice(&if covered { [p[0], p[1], p[2], 255] } else { [0; 4] });
            alpha[2 * k..2 * k + 2].copy_from_slice(&if covered { [p[3], 0] } else { [0, p[3]] });
        }
        self.pending = Some(self.pending.map_or((r0, r1), |(a, b)| (a.min(r0), b.max(r1))));
    }
}

/// Palettised formats: P_8 and AP_88.
fn palettised(format: u32) -> bool {
    format == 5 || format == 0xe
}

/// Layers of the texture array: TMU memory holds 64 textures of 256x256 at 8 bits.
const LAYERS: u32 = 64;
/// Levels of a 256x256 texture: 256 ... 1.
const LEVELS: i32 = 9;

static FILTER: AtomicU32 = AtomicU32::new(16);
static MSAA: AtomicU32 = AtomicU32::new(4);

/// At start, after the EXE is loaded: the settings' MSAA and texture filtering.
pub fn init() {
    let s = crate::settings::get();
    FILTER.store(s.texture_filter, Ordering::Relaxed);
    MSAA.store(s.msaa, Ordering::Relaxed);
}

/// MSAA changed: saved, and used from the next frame on.
pub fn set_msaa(n: u32) {
    crate::settings::update(|s| s.msaa = n);
    MSAA.store(n, Ordering::Relaxed);
}

/// Texture filtering changed: saved, and used from the next batch on.
pub fn set_filtering(n: u32) {
    crate::settings::update(|s| s.texture_filter = n);
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
    /// the back buffer, drawn into
    color: glow::Texture,
    /// with MSAA: the multisampled renderbuffer drawn into and its samples; resolved into
    /// `color` at grBufferSwap
    ms: Option<(glow::Renderbuffer, u32)>,
    ms_max: u32,
    prog: glow::Program,
    vbo: glow::Buffer,
    /// what each thread's context has of its own (see `Local`)
    locals: HashMap<std::thread::ThreadId, Local>,
    /// the threads that drew, with `swaps` at their last call (`with`)
    drawers: Vec<(std::thread::ThreadId, u64)>,
    /// grBufferSwap calls so far
    swaps: u64,
    /// every texture, 256x256 at most, in one array: rgb premultiplied by coverage and the
    /// coverage, with mipmaps (see the module)
    array: glow::Texture,
    /// the textures' alpha: its share in covered texels, and in keyed ones
    alpha: glow::Texture,
    /// layers whose level 0 a download changed (a bit each): their mipmaps are built before
    /// the next draw that uses mipmaps. Only theirs: building the whole array's after every
    /// download (the movies and menu pictures are 128x128 tiles downloaded and drawn one by
    /// one) kept the menu at 30 fps
    stale: u64,
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
    /// the alpha test: function (GR_CMP_*) and reference value. A vertex attribute too: the
    /// game changes it between almost all primitives (it ended 97% of the batches, ~250 draws
    /// a picture, 2026-11-08)
    alpha_test: (u32, u32),
    /// uniform locations
    uniforms: Uniforms,
    /// vertices waiting to be drawn with `state` (`VERTEX` words each): x y r g b a s t,
    /// constant r g b a (floats), the combine units and texture (see `add`), then the
    /// rectangle of the primitive's texture coordinates s0 t0 s1 t1 (floats)
    batch: Vec<u32>,
    /// the primitives that follow only cast shadows (`set_cast_only`)
    cast_only: bool,
    /// the primitives that follow are an object's, drawn in the picture (`set_no_cast`): their
    /// shadow comes from the ring's drawing of all objects, not from what is on the screen
    no_cast: bool,
    /// the parts of `batch` (word offsets) that are such objects' primitives
    no_cast_parts: Vec<(usize, usize)>,
    batch_mode: u32,
    /// the batch's primitives have a depth at every corner: depth-tested (`zbuffer.rs`)
    batch_z: bool,
    gamma: f32,
    /// the texture palette (grTexDownloadTable): 0xAARRGGBB, its alpha unused
    palette: [u32; 256],
    /// Glide's front and auxiliary buffers (the back buffer is `color`): the front one holds
    /// the picture shown; grBufferSwap swaps it with the back one
    front: glow::Texture,
    aux: glow::Texture,
    /// draws a texture's rectangle into a framebuffer (LFB writes)
    copy_prog: glow::Program,
    /// the last grLfbReadRegion, so that writing what it read back to a buffer copies on
    /// the GPU at full resolution
    last_read: Option<LfbRead>,
    /// pictures `gl_CopyBitmap_` saved from the back buffer (what is under the cursor), at the
    /// target's resolution, newest last
    saved: Vec<Saved>,
    /// ambient occlusion (`ao.rs`): view-space depth and a mask, a second attachment of the
    /// framebuffer drawn into, written while the 3D picture is drawn (`gbuf`); the passes,
    /// and whether they ran in this frame
    gdepth: Option<Gdepth>,
    gbuf: bool,
    posted: bool,
    ssao: Option<super::ao::Ssao>,
    /// dynamic lights (`lights.rs`): this picture's glows, and the pass
    lights: Vec<super::lights::Light>,
    lighting: Option<super::lights::Lighting>,
    /// the last picture's lights, merged: they tint this picture's smoke (lit smoke)
    smoke_lights: Vec<super::lights::Merged>,
    /// the sun's shadow (`shadows.rs`): this picture's casters (batched vertices), the map
    casters: Vec<u32>,
    shadow_map: Option<super::shadows::ShadowMap>,
    bloom: Option<super::bloom::Bloom>,
    /// heat shimmer (`heat.rs`): this picture's sources, and the pass
    heat_sources: Vec<super::heat::Source>,
    heat: Option<super::heat::Heat>,

    /// `add`'s scratch: the vertices' depths
    zs: Vec<f32>,
}

/// The depth attachment: its texture (multisampled with MSAA: the samples), its size, the
/// framebuffers it is attached to, and framebuffers with only their colour (the AO is applied
/// through one: the depth is read meanwhile). All of the game's thread.
struct Gdepth {
    tex: glow::Texture,
    /// the depth buffer (`zbuffer.rs`), as many samples as the picture
    zrb: glow::Renderbuffer,
    samples: Option<u32>,
    size: (u32, u32),
    attached: Vec<glow::Framebuffer>,
    colour: HashMap<(bool, u32), glow::Framebuffer>,
}

/// A rectangle of the back buffer saved by `gl_CopyBitmap_`: where its pixels went in guest
/// memory and what they were, and the rectangle as a texture.
struct Saved {
    data: u32,
    stride: u32,
    size: (u32, u32),
    bytes: Vec<u8>,
    tex: glow::Texture,
}

/// Saved pictures kept at most.
const SAVED: usize = 8;

/// A grLfbReadRegion: the buffer and rectangle read, where to, and the bytes written there.
struct LfbRead {
    buffer: u32,
    rect: [u32; 4],
    dst: u32,
    stride: u32,
    bytes: Vec<u8>,
}

const COPY_VS: &str = r#"
uniform vec4 src;   // the source rectangle: u v at the bottom left, u v at the top right
out vec2 uv;
void main() {
    vec2 p = vec2(gl_VertexID & 1, gl_VertexID >> 1);
    uv = mix(src.xy, src.zw, p);
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const COPY_FS: &str = r#"
uniform sampler2D img;
in vec2 uv;
out vec4 color;
void main() { color = vec4(texture(img, uv).rgb, 1.0); }
"#;

/// A texture of `w` x `h` with a framebuffer, cleared to black.
fn new_buffer(gl: &glow::Context, w: u32, h: u32) -> (glow::Framebuffer, glow::Texture) {
    unsafe {
        let t = texture(gl, true);
        let fbo = gl.create_framebuffer().expect("GL framebuffer");
        size_buffer(gl, (fbo, t), w, h);
        (fbo, t)
    }
}

/// Give a buffer a new size; its picture is black.
fn size_buffer(gl: &glow::Context, (fbo, t): (glow::Framebuffer, glow::Texture), w: u32, h: u32) {
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w as i32, h as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(t), 0);
        gl.disable(glow::SCISSOR_TEST);
        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
    }
}

/// The game draws from more than one thread (its game thread, and another for the text
/// before a mission), each with its own GL context sharing the window's: Glide's state is one
/// for all of them.
static GLIDE: Mutex<Option<Glide>> = Mutex::new(None);

/// The TMU's memory between grSstWinClose and the next grSstWinOpen.
struct Kept {
    array: glow::Texture,
    alpha: glow::Texture,
    textures: BTreeMap<u32, Texture>,
    palette: [u32; 256],
}

static KEPT: Mutex<Option<Kept>> = Mutex::new(None);

/// GL objects contexts do not share (framebuffers, vertex arrays), made for each thread that
/// draws.
struct Local {
    vao: glow::VertexArray,
    copy_vao: glow::VertexArray,
    /// framebuffers to read and draw levels of a layer through (building mipmaps)
    mip: [glow::Framebuffer; 2],
    /// a framebuffer for each texture (true) or renderbuffer drawn into
    fbos: HashMap<(bool, u32), glow::Framebuffer>,
}

impl Local {
    fn new(gl: &glow::Context, vbo: glow::Buffer) -> Local {
        unsafe {
            let vao = gl.create_vertex_array().expect("GL vertex array");
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            vertex_layout(gl);
            let fbo = || gl.create_framebuffer().expect("GL framebuffer");
            Local { vao, copy_vao: gl.create_vertex_array().expect("GL vertex array"), mip: [fbo(), fbo()], fbos: HashMap::new() }
        }
    }

    /// Delete it (on its own thread).
    fn delete(self, gl: &glow::Context) {
        unsafe {
            gl.delete_vertex_array(self.vao);
            gl.delete_vertex_array(self.copy_vao);
            for f in self.mip.into_iter().chain(self.fbos.into_values()) {
                gl.delete_framebuffer(f);
            }
        }
    }
}

/// The batched vertices' attributes (`VERTEX` words) on the VAO and buffer bound.
pub fn vertex_layout(gl: &glow::Context) {
    unsafe {
        for (loc, n, off) in [(0, 2, 0), (1, 4, 8), (2, 2, 24), (3, 4, 32)] {
            gl.enable_vertex_attrib_array(loc);
            gl.vertex_attrib_pointer_f32(loc, n, glow::FLOAT, false, 4 * VERTEX as i32, off);
        }
        gl.enable_vertex_attrib_array(4);
        gl.vertex_attrib_pointer_i32(4, 4, glow::UNSIGNED_INT, 4 * VERTEX as i32, 48);
        gl.enable_vertex_attrib_array(5);
        gl.vertex_attrib_pointer_f32(5, 4, glow::FLOAT, false, 4 * VERTEX as i32, 64);
        gl.enable_vertex_attrib_array(6);
        gl.vertex_attrib_pointer_f32(6, 1, glow::FLOAT, false, 4 * VERTEX as i32, 80);
        gl.enable_vertex_attrib_array(7);
        gl.vertex_attrib_pointer_f32(7, 3, glow::FLOAT, false, 4 * VERTEX as i32, 84);
    }
}

const VS: &str = r#"
uniform vec2 space;
uniform float zbias;
const float ZNEAR = 100.0;
layout(location = 0) in vec2 pos;
layout(location = 1) in vec4 col;
layout(location = 2) in vec2 tc;
layout(location = 3) in vec4 kc;
layout(location = 4) in uvec4 mode;
layout(location = 5) in vec4 box;
layout(location = 6) in float z;
layout(location = 7) in vec3 tint;
out vec4 vcol;
flat out vec3 vtint;
noperspective out float vinvz;
noperspective out vec2 st;
flat out vec4 konst;
flat out uvec4 vmode;
flat out vec4 st_box;
void main() {
    vcol = col / 255.0;
    vtint = tint;
    konst = kc;
    st_box = box;
    vmode = mode;
    st = tc;
    vinvz = z > 0.0 ? 1.0 / z : 0.0;
    // the depth buffer: 1 - near / z (linear on the screen as 1 / z), pulled nearer by zbias
    float d = z > 0.0 ? clamp(1.0 - ZNEAR / (z * (1.0 - zbias)), 0.0, 1.0) : 0.0;
    gl_Position = vec4(pos.x / space.x * 2.0 - 1.0, 1.0 - pos.y / space.y * 2.0, d * 2.0 - 1.0, 1.0);
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
uniform int gmode;     // the depth attachment: 0 not written, 1 depth, 2/3 mask (blended/additive)
in vec4 vcol;
noperspective in float vinvz;
noperspective in vec2 st;
flat in vec4 konst;    // the constant colour
flat in uvec4 vmode;   // combine units and texture, packed by `add`
flat in vec4 st_box;   // the primitive's texture rectangle
flat in vec3 vtint;    // lit smoke: the light reaching it (1 elsewhere)
layout(location = 0) out vec4 color;
layout(location = 1) out vec4 gbuf;

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
    // inside the primitive's texture rectangle: at a higher resolution than the game's, the
    // edge pixels would sample the neighbouring picture of an atlas (marks over letters)
    ivec2 lo = ivec2(floor(st_box.xy));
    p = clamp(p, lo, max(ivec2(ceil(st_box.zw)) - 1, lo));
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
    layer = int((vmode.z >> 8) & 0xffu);
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
    // the alpha test: GR_CMP_* function, reference value
    int ai = int(round(a * 255.0)), ar = int(vmode.z >> 24);
    int f = int((vmode.z >> 16) & 7u);
    bool pass = f == 7 || (f == 1 && ai < ar) || (f == 2 && ai == ar) || (f == 3 && ai <= ar)
        || (f == 4 && ai > ar) || (f == 5 && ai != ar) || (f == 6 && ai >= ar);
    if (!pass) discard;
    if (edge == 0 && cover < 0.5) discard;
    if (edge == 1) a = cover;
    if (edge == 2) a *= cover;
    if (edge == 3) c *= cover;
    if (cover <= 0.0) discard;
    color = vec4(c * vtint, a);
    if (gmode == 1) gbuf = vec4(vinvz > 0.0 ? 1.0 / vinvz : 0.0, 0.0, 0.0, step(0.5, a));
    else gbuf = vec4(0.0, clamp(2.0 * (gmode == 3 ? max(c.r, max(c.g, c.b)) : a), 0.0, 1.0), 0.0, 0.0);
}
"#;

/// Words per batched vertex.
pub const VERTEX: usize = 24;

/// The shaders' uniforms, looked up once.
struct Uniforms {
    space: Option<glow::UniformLocation>,
    zbias: Option<glow::UniformLocation>,
    tex: Option<glow::UniformLocation>,
    tex_a: Option<glow::UniformLocation>,
    mipmaps: Option<glow::UniformLocation>,
    clamp_st: Option<glow::UniformLocation>,
    chroma: Option<glow::UniformLocation>,
    key: Option<glow::UniformLocation>,
    linear_filter: Option<glow::UniformLocation>,
    edge: Option<glow::UniformLocation>,
    gmode: Option<glow::UniformLocation>,
}

impl Uniforms {
    fn new(gl: &glow::Context, prog: glow::Program) -> Uniforms {
        let u = |n: &str| unsafe { gl.get_uniform_location(prog, n) };
        Uniforms {
            space: u("space"),
            zbias: u("zbias"),
            tex: u("tex"),
            tex_a: u("tex_a"),
            mipmaps: u("mipmaps"),
            clamp_st: u("clamp_st"),
            chroma: u("chroma"),
            key: u("key"),
            linear_filter: u("linear_filter"),
            edge: u("edge"),
            gmode: u("gmode"),
        }
    }
}

/// The calling thread's id (thread::current() clones an Arc: at every Glide call ~1% of the
/// game thread).
fn thread_id() -> std::thread::ThreadId {
    thread_local! {
        static ME: std::thread::ThreadId = std::thread::current().id();
    }
    ME.with(|m| *m)
}

fn with<R>(f: impl FnOnce(&mut Glide, &glow::Context) -> R) -> Option<R> {
    let mut g = GLIDE.lock().unwrap_or_else(|e| e.into_inner());
    let g = g.as_mut()?;
    super::window::with_gl(|gl| {
        let r = f(g, gl);
        // while another thread draws too (within the last two pictures), the next call may be
        // its context: it sees this one's work once it is flushed. (A thread's GL objects stay
        // after it ends: the text before a mission made every call flush in it, 2026-11-11.)
        let me = thread_id();
        let swaps = g.swaps;
        let mut other = false;
        let mut known = false;
        for d in g.drawers.iter_mut() {
            if d.0 == me {
                d.1 = swaps;
                known = true;
            } else if d.1 + 2 >= swaps {
                other = true;
            }
        }
        if !known {
            g.drawers.push((me, swaps));
        }
        if other {
            unsafe { gl.flush() };
        }
        r
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
fn convert(format: u32, src: &[u8], palette: &[u32; 256]) -> Vec<u8> {
    let n = src.len() / bytes_per_texel(format) as usize;
    let rd16 = |k: u32| u16::from_le_bytes([src[2 * k as usize], src[2 * k as usize + 1]]);
    let rd8 = |k: u32| src[k as usize];
    let pal = |i: u8| {
        let c = palette[i as usize];
        [(c >> 16) as u8, (c >> 8) as u8, c as u8]
    };
    let e5 = |v: u32| ((v << 3) | (v >> 2)) as u8;
    let e6 = |v: u32| ((v << 2) | (v >> 4)) as u8;
    let e4 = |v: u32| (v * 17) as u8;
    let mut out = Vec::with_capacity(n * 4);
    for k in 0..n as u32 {
        let px: [u8; 4] = match format {
            0xb => {
                let v = rd16(k) as u32;
                [e5(v >> 10 & 31), e5(v >> 5 & 31), e5(v & 31), if v & 0x8000 != 0 { 255 } else { 0 }]
            }
            0xa => {
                let v = rd16(k) as u32;
                [e5(v >> 11), e6(v >> 5 & 63), e5(v & 31), 255]
            }
            0xc => {
                let v = rd16(k) as u32;
                [e4(v >> 8 & 15), e4(v >> 4 & 15), e4(v & 15), e4(v >> 12)]
            }
            0xd => {
                let v = rd16(k);
                let i = v as u8;
                [i, i, i, (v >> 8) as u8]
            }
            0 => {
                let v = rd8(k) as u32;
                [(v >> 5) as u8 * 36, (v >> 2 & 7) as u8 * 36, (v & 3) as u8 * 85, 255]
            }
            2 => [255, 255, 255, rd8(k)],
            4 => {
                let v = rd8(k) as u32;
                let i = e4(v & 15);
                [i, i, i, e4(v >> 4)]
            }
            8 => {
                let v = rd16(k) as u32;
                [(v >> 5 & 7) as u8 * 36, (v >> 2 & 7) as u8 * 36, (v & 3) as u8 * 85, (v >> 8) as u8]
            }
            5 => {
                let [r, g, b] = pal(rd8(k));
                [r, g, b, 255]
            }
            0xe => {
                let v = rd16(k);
                let [r, g, b] = pal(v as u8);
                [r, g, b, (v >> 8) as u8]
            }
            3 => {
                let i = rd8(k);
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
            let prog = compile(gl, VS, FS);
            let vbo = gl.create_buffer().expect("GL buffer");
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
            // the TMU's memory from before a grSstWinClose: the card keeps it, and the game
            // draws in a mission with textures it downloaded in the menu
            let kept = KEPT.lock().unwrap_or_else(|e| e.into_inner()).take();
            let (array, alpha, textures, palette) = match kept {
                Some(k) => (k.array, k.alpha, k.textures, k.palette),
                None => (layers(glow::RGBA8), layers(glow::RG8), BTreeMap::new(), [0; 256]),
            };
            let mut g = Glide {
                res: (w, h),
                target,
                space: (w, h),
                view: View::Full,
                color: texture(gl, true),
                ms: None,
                ms_max: gl.get_parameter_i32(glow::MAX_SAMPLES).max(0) as u32,
                prog,
                vbo,
                locals: HashMap::new(),
                drawers: Vec::new(),
                swaps: 0,
                array,
                alpha,
                stale: textures.values().fold(0, |m, t| m | 1 << t.layer),
                aniso: 0.0,
                aniso_max,
                textures,
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
                alpha_test: (7, 0),
                batch: Vec::new(),
                cast_only: false,
                no_cast: false,
                no_cast_parts: Vec::new(),
                batch_mode: glow::TRIANGLES,
                batch_z: false,
                gamma: 1.0,
                palette,
                front: texture(gl, true),
                aux: texture(gl, true),
                copy_prog: compile(gl, COPY_VS, COPY_FS),
                last_read: None,
                saved: Vec::new(),
                gdepth: None,
                gbuf: false,
                posted: false,
                ssao: None,
                lights: Vec::new(),
                lighting: None,
                smoke_lights: Vec::new(),
                casters: Vec::new(),
                shadow_map: None,
                bloom: None,
                heat_sources: Vec::new(),
                heat: None,

                zs: Vec::new(),
            };
            g.size_buffers(gl);
            g
        }
    }

    /// This thread's own GL objects.
    fn local(&mut self, gl: &glow::Context) -> &mut Local {
        let vbo = self.vbo;
        self.locals.entry(thread_id()).or_insert_with(|| Local::new(gl, vbo))
    }

    /// This thread's framebuffer drawing into a texture.
    fn tex_fbo(&mut self, gl: &glow::Context, t: glow::Texture) -> glow::Framebuffer {
        *self.local(gl).fbos.entry((true, t.0.get())).or_insert_with(|| unsafe {
            let f = gl.create_framebuffer().expect("GL framebuffer");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(f));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(t), 0);
            f
        })
    }

    /// This thread's framebuffer drawing into a renderbuffer.
    fn rb_fbo(&mut self, gl: &glow::Context, rb: glow::Renderbuffer) -> glow::Framebuffer {
        *self.local(gl).fbos.entry((false, rb.0.get())).or_insert_with(|| unsafe {
            let f = gl.create_framebuffer().expect("GL framebuffer");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(f));
            gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::RENDERBUFFER, Some(rb));
            f
        })
    }

    /// The three buffers at the target's size, black.
    fn size_buffers(&mut self, gl: &glow::Context) {
        let (w, h) = self.target;
        for t in [self.color, self.front, self.aux] {
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w as i32, h as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            }
            let f = self.tex_fbo(gl, t);
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(f));
                gl.disable(glow::SCISSOR_TEST);
                gl.clear_color(0.0, 0.0, 0.0, 1.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
        }
    }

    /// The view's rectangle in the target (x, y from the top, w, h) and the columns it is cut to.
    /// Game pixels to target pixels for the 3D picture (`ao::Map`).
    fn full_map(&self) -> super::ao::Map {
        let (tw, th) = (self.target.0 as f32, self.target.1 as f32);
        [0.0, 0.0, tw / self.space.0 as f32, th / self.space.1 as f32]
    }

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
        // the same aspect (a menu, the window to fullscreen): the pictures are kept, scaled.
        // The game draws a menu's background once and then only what changes: black buffers
        // left the menu black. Another aspect (wide and 4:3 in a mission) stays black: what
        // switches there draws whole (the mission menu, the cockpit panel: `wide::redraw`).
        self.drop_gbuffer(gl);
        let old = self.target;
        let same = old.0 > 0 && old.1 > 0 && (old.0 as u64 * h as u64).abs_diff(w as u64 * old.1 as u64) <= old.1.max(h) as u64;
        let kept: Vec<_> = if same { [self.color, self.front, self.aux].map(|t| self.keep(gl, t, old)).into() } else { Vec::new() };
        self.target = (w, h);
        self.size_buffers(gl);
        self.multisample(gl, true);
        for (t, (fbo, tmp)) in [self.color, self.front, self.aux].into_iter().zip(kept) {
            let to = self.tex_fbo(gl, t);
            unsafe {
                gl.disable(glow::SCISSOR_TEST);
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fbo));
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(to));
                gl.blit_framebuffer(0, 0, old.0 as i32, old.1 as i32, 0, 0, w as i32, h as i32, glow::COLOR_BUFFER_BIT, glow::LINEAR);
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.delete_framebuffer(fbo);
                gl.delete_texture(tmp);
            }
        }
        // with MSAA the game draws on in the multisampled buffer: the back buffer's picture
        // goes there too (as at a swap)
        if same && let Some((rb, _)) = self.ms {
            let ms = self.rb_fbo(gl, rb);
            self.copy(gl, self.color, [0.0, 0.0, 1.0, 1.0], ms, [0, 0, w as i32, h as i32]);
        }
    }

    /// A copy of buffer `t` (of size `size`) in a framebuffer of its own.
    fn keep(&mut self, gl: &glow::Context, t: glow::Texture, size: (u32, u32)) -> (glow::Framebuffer, glow::Texture) {
        let tmp = new_buffer(gl, size.0, size.1);
        let from = self.tex_fbo(gl, t);
        unsafe {
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(from));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(tmp.0));
            let (w, h) = (size.0 as i32, size.1 as i32);
            gl.blit_framebuffer(0, 0, w, h, 0, 0, w, h, glow::COLOR_BUFFER_BIT, glow::NEAREST);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        tmp
    }

    /// The multisampled framebuffer for the chosen samples and the target's size (`resized`:
    /// the target changed).
    fn multisample(&mut self, gl: &glow::Context, resized: bool) {
        let samples = MSAA.load(Ordering::Relaxed).min(self.ms_max);
        let samples = if samples < 2 { 0 } else { samples };
        if !resized && self.ms.map_or(0, |m| m.1) == samples {
            return;
        }
        // the target's pictures are gone: the HUD's parts drawn only once are drawn again
        super::wide::redraw();
        self.drop_gbuffer(gl);
        if let Some((rb, _)) = self.ms.take() {
            // the framebuffers on it (other threads' are left: they cannot be deleted here)
            let key = (false, rb.0.get());
            if let Some(f) = self.local(gl).fbos.remove(&key) {
                unsafe { gl.delete_framebuffer(f) };
            }
            for l in self.locals.values_mut() {
                l.fbos.remove(&key);
            }
            unsafe { gl.delete_renderbuffer(rb) };
        }
        if samples == 0 {
            return;
        }
        let rb = unsafe {
            let rb = gl.create_renderbuffer().expect("GL renderbuffer");
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(rb));
            gl.renderbuffer_storage_multisample(glow::RENDERBUFFER, samples as i32, glow::RGBA8, self.target.0 as i32, self.target.1 as i32);
            rb
        };
        self.ms = Some((rb, samples));
        let f = self.rb_fbo(gl, rb);
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(f));
            gl.disable(glow::SCISSOR_TEST);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    /// The depth attachment when the AO is on, at the target's size and samples (made anew
    /// when they changed), none when it is off.
    fn gbuffer(&mut self, gl: &glow::Context) {
        let samples = self.ms.map(|m| m.1);
        if !super::ao::depth_wanted() || self.gdepth.as_ref().is_some_and(|d| d.size != self.target || d.samples != samples) {
            self.drop_gbuffer(gl);
        }
        if !super::ao::depth_wanted() || self.gdepth.is_some() {
            return;
        }
        let (w, h) = (self.target.0 as i32, self.target.1 as i32);
        unsafe {
            let tex = gl.create_texture().expect("GL texture");
            if let Some(n) = samples {
                gl.bind_texture(glow::TEXTURE_2D_MULTISAMPLE, Some(tex));
                gl.tex_image_2d_multisample(glow::TEXTURE_2D_MULTISAMPLE, n as i32, glow::RG32F as i32, w, h, true);
                gl.bind_texture(glow::TEXTURE_2D_MULTISAMPLE, None);
            } else {
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                for p in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, p, glow::NEAREST as i32);
                }
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RG32F as i32, w, h, 0, glow::RG, glow::FLOAT, glow::PixelUnpackData::Slice(None));
            }
            let zrb = gl.create_renderbuffer().expect("GL renderbuffer");
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(zrb));
            gl.renderbuffer_storage_multisample(glow::RENDERBUFFER, samples.unwrap_or(0) as i32, glow::DEPTH_COMPONENT32F, w, h);
            gl.bind_renderbuffer(glow::RENDERBUFFER, None);
            self.gdepth = Some(Gdepth { tex, zrb, samples, size: self.target, attached: Vec::new(), colour: HashMap::new() });
        }
    }

    /// The depth attachment on the framebuffer drawn into (this thread's), cleared when new.
    /// False when it cannot be attached.
    fn attach(&mut self, gl: &glow::Context, fbo: glow::Framebuffer) -> bool {
        let Some(d) = &mut self.gdepth else { return false };
        if d.attached.contains(&fbo) {
            return true;
        }
        let kind = if d.samples.is_some() { glow::TEXTURE_2D_MULTISAMPLE } else { glow::TEXTURE_2D };
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT1, kind, Some(d.tex), 0);
            gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::DEPTH_ATTACHMENT, glow::RENDERBUFFER, Some(d.zrb));
            gl.draw_buffers(&[glow::COLOR_ATTACHMENT0, glow::COLOR_ATTACHMENT1]);
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                log!("warn: no ambient occlusion (the depth attachment is not taken)");
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT1, kind, None, 0);
                gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::DEPTH_ATTACHMENT, glow::RENDERBUFFER, None);
                gl.draw_buffers(&[glow::COLOR_ATTACHMENT0]);
                super::ao::set_enabled(false);
                return false;
            }
            gl.disable(glow::SCISSOR_TEST);
            gl.color_mask_draw_buffer(1, true, true, true, true);
            gl.clear_buffer_f32_slice(glow::COLOR, 1, &[0.0; 4]);
            gl.color_mask_draw_buffer(1, false, false, false, false);
            gl.depth_mask(true);
            gl.clear_buffer_f32_slice(glow::DEPTH, 0, &[1.0]);
        }
        d.attached.push(fbo);
        true
    }

    /// The depth attachment taken off the framebuffers (those still there) and deleted.
    fn drop_gbuffer(&mut self, gl: &glow::Context) {
        let Some(d) = self.gdepth.take() else { return };
        let live: Vec<glow::Framebuffer> = self.local(gl).fbos.values().copied().collect();
        let kind = if d.samples.is_some() { glow::TEXTURE_2D_MULTISAMPLE } else { glow::TEXTURE_2D };
        unsafe {
            for f in d.attached.into_iter().filter(|f| live.contains(f)) {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(f));
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT1, kind, None, 0);
                gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::DEPTH_ATTACHMENT, glow::RENDERBUFFER, None);
                gl.draw_buffers(&[glow::COLOR_ATTACHMENT0]);
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_renderbuffer(d.zrb);
            for f in d.colour.into_values() {
                gl.delete_framebuffer(f);
            }
            gl.delete_texture(d.tex);
        }
    }

    /// The depth attachment emptied for the next frame.
    fn clear_gbuffer(&mut self, gl: &glow::Context) {
        if self.gdepth.is_none() {
            return;
        }
        let fbo = self.draw_fbo(gl);
        if !self.attach(gl, fbo) {
            return;
        }
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.disable(glow::SCISSOR_TEST);
            gl.color_mask_draw_buffer(1, true, true, true, true);
            gl.clear_buffer_f32_slice(glow::COLOR, 1, &[0.0; 4]);
            gl.color_mask_draw_buffer(1, false, false, false, false);
            gl.depth_mask(true);
            gl.clear_buffer_f32_slice(glow::DEPTH, 0, &[1.0]);
        }
    }

    /// A framebuffer with only the colour of the one drawn into.
    fn colour_fbo(&mut self, gl: &glow::Context) -> Option<glow::Framebuffer> {
        let key = match self.ms {
            Some((rb, _)) => (false, rb.0.get()),
            None => (true, self.color.0.get()),
        };
        let (ms, color) = (self.ms, self.color);
        let d = self.gdepth.as_mut()?;
        Some(*d.colour.entry(key).or_insert_with(|| unsafe {
            let f = gl.create_framebuffer().expect("GL framebuffer");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(f));
            match ms {
                Some((rb, _)) => gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::RENDERBUFFER, Some(rb)),
                None => gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(color), 0),
            }
            f
        }))
    }

    /// The 3D picture is drawn (the HUD comes next): the ambient occlusion over it, then the
    /// dynamic lights, the heat shimmer, the bloom.
    fn post_3d(&mut self, gl: &glow::Context) {
        self.flush(gl);
        self.gbuf = false;
        self.posted = true;
        let lights = std::mem::take(&mut self.lights);
        self.smoke_lights.clear();
        let heat_sources = std::mem::take(&mut self.heat_sources);
        let Some(out) = self.colour_fbo(gl) else { return };
        let d = self.gdepth.as_ref().unwrap();
        let (tex, ms) = (d.tex, d.samples.is_some());
        let (target, map) = (self.target, self.full_map());
        // the AO and the sun's shadow (`NSTRIKE_GBUFFER=lights`: neither, `=shadows`: no AO)
        let view = super::ao::debug_view();
        let ao = super::ao::enabled() && view != 3 && view != 4;
        let casters = std::mem::take(&mut self.casters);
        let sun = match (super::shadows::enabled() && view != 3, super::ao::sun(), super::ao::camera(target, map)) {
            (true, Some(sun), Some(cam)) => {
                let textures = (self.array, self.alpha);
                self.shadow_map.get_or_insert_with(|| super::shadows::ShadowMap::new(gl)).render(gl, &casters, sun, cam, map, target.1 as f32, textures)
            }
            _ => None,
        };
        self.casters = casters;
        self.casters.clear();
        if ao || sun.is_some() {
            self.ssao.get_or_insert_with(|| super::ao::Ssao::new(gl)).run(gl, tex, ms, target, map, out, ao, sun.as_ref());
        }
        if let (true, Some(cam)) = (super::lights::enabled(), super::ao::camera(target, map)) {
            let merged = super::lights::merge(&lights);
            self.lighting.get_or_insert_with(|| super::lights::Lighting::new(gl)).run(gl, tex, ms, target, cam, out, &merged);
            self.smoke_lights = merged;
        }
        if let (true, Some((focal, _))) = (super::heat::enabled(), super::ao::camera(target, map)) {
            let clip = [0, 0, target.0 as i32, target.1 as i32];
            self.heat.get_or_insert_with(|| super::heat::Heat::new(gl)).run(gl, tex, ms, target, out, clip, focal, &heat_sources);
        }
        if super::bloom::enabled() {
            let clip = [0, 0, target.0 as i32, target.1 as i32];
            self.bloom.get_or_insert_with(|| super::bloom::Bloom::new(gl)).run(gl, tex, ms, target, out, clip);
        }
    }

    /// The framebuffer drawn into.
    fn draw_fbo(&mut self, gl: &glow::Context) -> glow::Framebuffer {
        match self.ms {
            Some((rb, _)) => self.rb_fbo(gl, rb),
            None => self.tex_fbo(gl, self.color),
        }
    }

    /// The multisampled picture into the back buffer's texture.
    fn resolve(&mut self, gl: &glow::Context) {
        if let Some((rb, _)) = self.ms {
            let (w, h) = (self.target.0 as i32, self.target.1 as i32);
            let (from, to) = (self.rb_fbo(gl, rb), self.tex_fbo(gl, self.color));
            unsafe {
                gl.disable(glow::SCISSOR_TEST);
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(from));
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(to));
                gl.blit_framebuffer(0, 0, w, h, 0, 0, w, h, glow::COLOR_BUFFER_BIT, glow::NEAREST);
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            }
        }
    }

    /// A Glide buffer (GR_BUFFER_FRONTBUFFER 0, BACKBUFFER 1, AUXBUFFER 2) to read: the back
    /// one resolved.
    fn read_buffer(&mut self, gl: &glow::Context, b: u32) -> (glow::Framebuffer, glow::Texture) {
        let t = match b {
            0 => self.front,
            2 => self.aux,
            _ => {
                self.flush(gl);
                self.resolve(gl);
                self.color
            }
        };
        (self.tex_fbo(gl, t), t)
    }

    /// The framebuffer to write a Glide buffer through.
    fn write_buffer(&mut self, gl: &glow::Context, b: u32) -> glow::Framebuffer {
        match b {
            0 => self.tex_fbo(gl, self.front),
            2 => self.tex_fbo(gl, self.aux),
            _ => self.draw_fbo(gl),
        }
    }

    /// A rectangle in Glide screen pixels (x, y from the top, w, h) in the target's GL pixels
    /// (from the bottom).
    fn target_rect(&self, [x, y, w, h]: [u32; 4]) -> [i32; 4] {
        let (sx, sy) = (self.target.0 as f32 / self.res.0 as f32, self.target.1 as f32 / self.res.1 as f32);
        let x0 = (x as f32 * sx).round() as i32;
        let x1 = ((x + w) as f32 * sx).round() as i32;
        let y0 = self.target.1 as i32 - ((y + h) as f32 * sy).round() as i32;
        let y1 = self.target.1 as i32 - (y as f32 * sy).round() as i32;
        [x0, y0, x1 - x0, y1 - y0]
    }

    /// Draw the rectangle `src` of a texture (u v bottom left, u v top right) into `rect`
    /// (GL pixels) of a framebuffer.
    fn copy(&mut self, gl: &glow::Context, tex: glow::Texture, src: [f32; 4], fbo: glow::Framebuffer, rect: [i32; 4]) {
        self.flush(gl);
        let vao = self.local(gl).copy_vao;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.viewport(rect[0], rect[1], rect[2], rect[3]);
            gl.use_program(Some(self.copy_prog));
            gl.bind_vertex_array(Some(vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            // asked of the driver once: NVIDIA's lookup made system calls, 11% of the game
            // thread at a copy a frame
            gl.uniform_1_i32(super::gl::uniform(gl, self.copy_prog, "img").as_ref(), 0);
            gl.uniform_4_f32(super::gl::uniform(gl, self.copy_prog, "src").as_ref(), src[0], src[1], src[2], src[3]);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }

    /// grLfbReadRegion: pixels of a buffer's rectangle as RGB565 rows at `dst`.
    fn lfb_read(&mut self, gl: &glow::Context, buffer: u32, rect: [u32; 4], stride: u32, dst: u32) {
        let (fbo, _) = self.read_buffer(gl, buffer);
        let [x, y, w, h] = self.target_rect(rect);
        let (rw, rh) = (rect[2] as i32, rect[3] as i32);
        let mut px = vec![0u8; (rw * rh * 4) as usize];
        unsafe {
            // scaled to Glide pixels in a framebuffer of the rectangle's size
            let tmp = new_buffer(gl, rw as u32, rh as u32);
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fbo));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(tmp.0));
            gl.blit_framebuffer(x, y, x + w, y + h, 0, 0, rw, rh, glow::COLOR_BUFFER_BIT, glow::LINEAR);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(tmp.0));
            gl.read_pixels(0, 0, rw, rh, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px)));
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_framebuffer(tmp.0);
            gl.delete_texture(tmp.1);
        }
        let m = mem();
        let mut bytes = Vec::with_capacity((rw * rh * 2) as usize);
        for r in 0..rh {
            // GL rows go up
            let row = &px[((rh - 1 - r) * rw * 4) as usize..][..(rw * 4) as usize];
            for (k, p) in row.chunks(4).enumerate() {
                let v = (p[0] as u16 >> 3) << 11 | (p[1] as u16 >> 2) << 5 | p[2] as u16 >> 3;
                m.wr16(dst + r as u32 * stride + 2 * k as u32, v);
                bytes.extend(v.to_le_bytes());
            }
        }
        self.last_read = Some(LfbRead { buffer, rect, dst, stride, bytes });
    }

    /// grLfbWriteRegion: pixels (GrLfbSrcFmt_t) into a buffer's rectangle. Pixels just read
    /// with grLfbReadRegion and not changed since are copied between the buffers instead, at
    /// full resolution.
    #[allow(clippy::too_many_arguments)]
    fn lfb_write(&mut self, gl: &glow::Context, buffer: u32, at: (u32, u32), format: u32, size: (u32, u32), stride: u32, data: u32) {
        let m = mem();
        let rect = self.target_rect([at.0, at.1, size.0, size.1]);
        let fbo = self.write_buffer(gl, buffer);
        let (rw, rh) = size;
        let row_bytes = (rw * 2) as usize;
        let unchanged = self.last_read.as_ref().is_some_and(|r| {
            format == 0
                && (r.rect[2], r.rect[3], r.dst, r.stride) == (rw, rh, data, stride)
                && (0..rh).all(|y| *m.slice(data + y * stride, row_bytes) == r.bytes[y as usize * row_bytes..][..row_bytes])
        });
        if unchanged {
            let (src_buffer, [x, y, w, h]) = self.last_read.as_ref().map(|r| (r.buffer, r.rect)).unwrap();
            if src_buffer == buffer && (x, y) == at {
                return;
            }
            let (_, tex) = self.read_buffer(gl, src_buffer);
            let (gw, gh) = (self.res.0 as f32, self.res.1 as f32);
            let src = [x as f32 / gw, 1.0 - (y + h) as f32 / gh, (x + w) as f32 / gw, 1.0 - y as f32 / gh];
            self.copy(gl, tex, src, fbo, rect);
            return;
        }
        if !matches!(format, 0..=2 | 4 | 5) {
            log!("warn: grLfbWriteRegion format {format} not supported");
        }
        let e5 = |v: u32| ((v << 3) | (v >> 2)) as u8;
        let e6 = |v: u32| ((v << 2) | (v >> 4)) as u8;
        let mut px = Vec::with_capacity((rw * rh * 4) as usize);
        for y in 0..rh {
            let row = data + y * stride;
            for k in 0..rw {
                let p = match format {
                    0 => {
                        let v = m.rd16(row + 2 * k) as u32;
                        [e5(v >> 11), e6(v >> 5 & 63), e5(v & 31), 255]
                    }
                    1 | 2 => {
                        let v = m.rd16(row + 2 * k) as u32;
                        [e5(v >> 10 & 31), e5(v >> 5 & 31), e5(v & 31), 255]
                    }
                    4 | 5 => {
                        let v = m.rd32(row + 4 * k);
                        [(v >> 16) as u8, (v >> 8) as u8, v as u8, 255]
                    }
                    _ => [255, 0, 255, 255],
                };
                px.extend(p);
            }
        }
        unsafe {
            let t = texture(gl, true);
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, rw as i32, rh as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&px)));
            // the rows go down from the top
            self.copy(gl, t, [0.0, 1.0, 1.0, 0.0], fbo, rect);
            gl.delete_texture(t);
        }
    }

    /// Keep the back buffer's rectangle `rect` (Glide pixels) as a texture, for the pixels
    /// just read to `data` (see `copy_bitmap`).
    fn save(&mut self, gl: &glow::Context, rect: [u32; 4], data: u32, stride: u32) {
        let (fbo, _) = self.read_buffer(gl, 1);
        let [x, y, w, h] = self.target_rect_out(rect);
        let tex = unsafe {
            let tmp = new_buffer(gl, w.max(1) as u32, h.max(1) as u32);
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fbo));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(tmp.0));
            gl.blit_framebuffer(x, y, x + w, y + h, 0, 0, w, h, glow::COLOR_BUFFER_BIT, glow::NEAREST);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_framebuffer(tmp.0);
            tmp.1
        };
        let m = mem();
        let row = (rect[2] * 2) as usize;
        let bytes = (0..rect[3]).flat_map(|r| m.slice(data + r * stride, row).to_vec()).collect();
        if let Some(k) = self.saved.iter().position(|sv| sv.data == data) {
            unsafe { gl.delete_texture(self.saved.remove(k).tex) };
        }
        if self.saved.len() >= SAVED {
            unsafe { gl.delete_texture(self.saved.remove(0).tex) };
        }
        self.saved.push(Saved { data, stride, size: (rect[2], rect[3]), bytes, tex });
    }

    /// Draw a saved picture back into the back buffer at (x, y) if `data` still holds what was
    /// saved there (the whole picture: `size`); false if there is none.
    fn restore(&mut self, gl: &glow::Context, at: (u32, u32), size: (u32, u32), data: u32, stride: u32) -> bool {
        let m = mem();
        let Some(sv) = self.saved.iter().find(|sv| (sv.data, sv.stride, sv.size) == (data, stride, size)) else { return false };
        let row = (size.0 * 2) as usize;
        if !(0..size.1).all(|r| *m.slice(data + r * stride, row) == sv.bytes[r as usize * row..][..row]) {
            return false;
        }
        let tex = sv.tex;
        let rect = self.target_rect_out([at.0, at.1, size.0, size.1]);
        let fbo = self.draw_fbo(gl);
        self.copy(gl, tex, [0.0, 0.0, 1.0, 1.0], fbo, rect);
        true
    }

    /// `target_rect` rounded outwards: every pixel a primitive over `rect` may touch. At a
    /// scale that is no whole number (1080 / 480 = 2.25) an edge falls on a pixel's middle,
    /// which the GPU draws and a rounded rectangle left out: the cursor, put back over such a
    /// rectangle, left lines behind (2026-11-08).
    fn target_rect_out(&self, [x, y, w, h]: [u32; 4]) -> [i32; 4] {
        let (sx, sy) = (self.target.0 as f32 / self.res.0 as f32, self.target.1 as f32 / self.res.1 as f32);
        let x0 = ((x as f32 * sx).floor() as i32).max(0);
        let x1 = (((x + w) as f32 * sx).ceil() as i32).min(self.target.0 as i32);
        let y0 = (self.target.1 as i32 - ((y + h) as f32 * sy).ceil() as i32).max(0);
        let y1 = (self.target.1 as i32 - (y as f32 * sy).floor() as i32).min(self.target.1 as i32);
        [x0, y0, x1 - x0, y1 - y0]
    }

    /// Draw what is batched with the current state.
    fn flush(&mut self, gl: &glow::Context) {
        if self.batch.is_empty() {
            return;
        }
        let s = self.state;
        let filter = FILTER.load(Ordering::Relaxed);
        if filter != 0 && s.key & 0xff_ffff == 0 && self.stale != 0 {
            self.build_mipmaps(gl);
        }
        let fbo = self.draw_fbo(gl);
        if self.gbuf {
            self.gbuffer(gl);
        }
        let depth = self.gbuf && self.attach(gl, fbo);
        let vao = self.local(gl).vao;
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            self.scissor(gl, s.clip);
            gl.use_program(Some(self.prog));
            gl.bind_vertex_array(Some(vao));
            let u = &self.uniforms;
            gl.uniform_2_f32(u.space.as_ref(), self.space.0 as f32, self.space.1 as f32);
            let mipmaps = filter != 0 && s.key & 0xff_ffff == 0;
            gl.uniform_1_i32(u.mipmaps.as_ref(), mipmaps as i32);
            gl.uniform_1_i32(u.tex_a.as_ref(), 1);
            gl.uniform_1_i32(u.tex.as_ref(), 0);
            let aniso = (filter.max(1) as f32).min(self.aniso_max);
            for (k, t) in [(glow::TEXTURE1, self.alpha), (glow::TEXTURE0, self.array)] {
                gl.active_texture(k);
                gl.bind_texture(glow::TEXTURE_2D_ARRAY, Some(t));
                if aniso != self.aniso && self.aniso_max > 1.0 {
                    gl.tex_parameter_f32(glow::TEXTURE_2D_ARRAY, glow::TEXTURE_MAX_ANISOTROPY_EXT, aniso);
                }
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
            // the depth attachment: depth from opaque 3D primitives and alpha-blended ones (the
            // game's solid geometry may be blended with alpha 1), where their alpha is at least
            // half (the attachment blended with its own alpha 0 or 1); a mask from those that
            // add colour; nothing from those that only darken (shadows) or the HUD
            let gmode = match depth {
                true if opaque || src == glow::SRC_ALPHA && dst == glow::ONE_MINUS_SRC_ALPHA => 1,
                true if src == glow::ZERO || src == glow::DST_COLOR => 0,
                true if dst == glow::ONE => 3,
                true => 2,
                false => 0,
            };
            gl.uniform_1_i32(u.gmode.as_ref(), gmode);
            // the depth buffer: primitives with depth tested; solid ones write it, the others
            // (blended, darkening) are drawn a little nearer (lying on the ground: no flicker)
            let ztest = depth && self.batch_z && super::zbuffer::enabled();
            if ztest {
                gl.enable(glow::DEPTH_TEST);
                gl.depth_func(glow::LEQUAL);
                gl.depth_mask(gmode == 1);
            }
            gl.uniform_1_f32(u.zbias.as_ref(), if ztest && gmode != 1 { super::zbuffer::BIAS } else { 0.0 });
            // the sun's shadow casters: triangles with depth at every corner, the chroma key in
            // the constant colour's place (`shadows.rs`)
            if gmode == 1 && self.batch_mode == glow::TRIANGLES && super::shadows::enabled() {
                let key = s.key;
                let kc = [(key >> 16 & 0xff) as f32 / 255.0, (key >> 8 & 0xff) as f32 / 255.0, (key & 0xff) as f32 / 255.0, s.chroma as u32 as f32].map(f32::to_bits);
                let mut parts = self.no_cast_parts.iter().peekable();
                for (i, t) in self.batch.chunks_exact(3 * VERTEX).enumerate() {
                    let at = i * 3 * VERTEX;
                    while parts.next_if(|p| p.1 <= at).is_some() {}
                    if parts.peek().is_some_and(|p| p.0 <= at) {
                        continue;
                    }
                    if t.chunks_exact(VERTEX).all(|v| f32::from_bits(v[20]) > 0.0) {
                        for v in t.chunks_exact(VERTEX) {
                            self.casters.extend_from_slice(&v[..8]);
                            self.casters.extend(kc);
                            self.casters.extend_from_slice(&v[12..]);
                        }
                    }
                }
            }
            match gmode {
                1 => gl.color_mask_draw_buffer(1, true, true, false, false),
                2 | 3 => {
                    gl.color_mask_draw_buffer(1, false, true, false, false);
                    gl.disable_draw_buffer(glow::BLEND, 1);
                }
                _ => {}
            }
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            let bytes = std::slice::from_raw_parts(self.batch.as_ptr() as *const u8, self.batch.len() * 4);
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
            gl.draw_arrays(self.batch_mode, 0, (self.batch.len() / VERTEX) as i32);
            DRAWS.fetch_add(1, Ordering::Relaxed);
            gl.disable(glow::DEPTH_TEST);
            gl.depth_mask(true);
            if depth {
                gl.color_mask_draw_buffer(1, false, false, false, false);
            }
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
        }
        self.batch.clear();
        self.no_cast_parts.clear();
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
        // the combine units a byte each, then invert bits, texturing, the layer and the alpha
        // test, the texture's size
        let bytes = |u: [u32; 5]| u[0] | u[1] << 8 | u[2] << 16 | u[3] << 24;
        if let Some(a) = self.tex {
            self.refresh(gl, a);
        }
        let tex = self.tex.and_then(|a| self.textures.get(&a));
        let mode = [
            bytes(self.cc),
            bytes(self.ac),
            (self.cc[4] & 1) | (self.ac[4] & 1) << 1 | (tex.is_some() as u32) << 2 | tex.map_or(0, |t| t.layer) << 8 | self.alpha_test.0 << 16 | self.alpha_test.1 << 24,
            tex.map_or(0, |t| t.w | t.h << 16),
        ];
        // Glide's s and t span 0..256 over the texture's longer side, at any size: in texels
        let scale = tex.map_or(1.0, |t| t.w.max(t.h) as f32 / 256.0);
        // x y z r g b ooz a oow, then tmu 0: sow tow oow
        let st = |v: u32| {
            let oow = f32::from_bits(m.rd32(v + 0x2c));
            let oow = if oow != 0.0 { oow } else { 1.0 };
            (f32::from_bits(m.rd32(v + 0x24)) / oow * scale, f32::from_bits(m.rd32(v + 0x28)) / oow * scale)
        };
        let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for &v in verts {
            let (s, t) = st(v);
            b = [b[0].min(s), b[1].min(t), b[2].max(s), b[3].max(t)];
        }
        let st_box = b;
        let b = b.map(f32::to_bits);
        let xy = |v: u32| (f32::from_bits(m.rd32(v)), f32::from_bits(m.rd32(v + 4)));
        // a line: a quad as wide as the game's line at its resolution
        let quad = if let [v0, v1] = *verts { Some(self.line_offsets(xy(v0), xy(v1))) } else { None };
        // the view-space depth of each vertex (3D picture, AO on), by its place
        let mut z = std::mem::take(&mut self.zs);
        if self.gbuf && self.gdepth.is_some() {
            super::ao::depths(verts.iter().map(|&v| xy(v)), &mut z);
        } else {
            z.clear();
            z.resize(verts.len(), 0.0);
        }
        // a glow of the 3D picture (additive): a dynamic light (not lines), or smoke tinted by them
        let mut tint = [1.0f32; 3];
        if self.gbuf && !self.cast_only && self.gdepth.is_some() && (super::lights::enabled() || super::heat::enabled()) && verts.len() >= 3 && self.state.blend.1 == 4 && matches!(self.state.blend.0, 1 | 4)
            && let Some(cam) = super::ao::camera(self.target, self.full_map())
        {
            let colour = self.glow_colour(verts, st_box);
            static LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *LOG.get_or_init(|| std::env::var_os("NSTRIKE_LIGHT_LOG").is_some()) {
                super::lights::log_source(colour);
            }
            // a sprite has no projected points: the nearest point's depth
            if z.iter().all(|&d| d <= 0.0) {
                let n = verts.len() as f32;
                let (cx, cy) = verts.iter().fold((0.0, 0.0), |a, &v| (a.0 + xy(v).0 / n, a.1 + xy(v).1 / n));
                if let Some(d) = super::ao::depth_near(cx, cy, 32.0) {
                    z.iter_mut().for_each(|z| *z = d);
                }
            }
            let [ox, oy, kx, ky] = self.full_map();
            let th = self.target.1 as f32;
            let corners: Vec<_> = verts.iter().map(|&v| (ox + xy(v).0 * kx, th - (oy + xy(v).1 * ky))).collect();
            if super::lights::smoke_enabled() && super::lights::is_smoke() {
                tint = super::lights::smoke_tint(&corners, &z, cam, &self.smoke_lights);
            }
            // only the game's fire and explosions (`lights::gives_light`)
            if super::lights::gives_light() {
                if super::lights::enabled()
                    && let Some(l) = super::lights::light(&corners, &z, colour, cam)
                {
                    self.lights.push(l);
                }
                if super::heat::enabled()
                    && let Some(s) = super::heat::source(&corners, &z, true, cam.0.1)
                {
                    self.heat_sources.push(s);
                }
            }
        }
        // the depth buffer: corners without depth among ones with it take their mean; a batch
        // holds primitives all with depth (tested) or all without (drawn in the game's order)
        let has_z = super::zbuffer::enabled() && super::zbuffer::fill(&mut z);
        if has_z != self.batch_z {
            self.flush(gl);
            self.batch_z = has_z;
        }
        let start = self.batch.len();
        let mut put = |v: u32, (x, y): (f32, f32), z: f32| {
            let (s, t) = st(v);
            self.batch.extend([x.to_bits(), y.to_bits(), m.rd32(v + 0xc), m.rd32(v + 0x10), m.rd32(v + 0x14), m.rd32(v + 0x1c), s.to_bits(), t.to_bits()]);
            self.batch.extend(konst);
            self.batch.extend(mode);
            self.batch.extend(b);
            self.batch.push(z.to_bits());
            self.batch.extend(tint.map(f32::to_bits));
        };
        if let (Some([n0, n1, n2, n3]), &[v0, v1]) = (quad, verts) {
            put(v0, n0, z[0]);
            put(v0, n1, z[0]);
            put(v1, n2, z[1]);
            put(v1, n2, z[1]);
            put(v0, n1, z[0]);
            put(v1, n3, z[1]);
        } else {
            for (&v, &z) in verts.iter().zip(&z) {
                put(v, xy(v), z);
            }
        }
        self.zs = z;
        if self.cast_only {
            self.cast(start, verts.len());
        } else if self.no_cast {
            self.no_cast_parts.push((start, self.batch.len()));
        }
    }

    /// The shadows' object ring (`pictures::shadow_ring`): the primitive just batched from
    /// `start` is not drawn; a solid one with depth at every corner is a shadow caster, its
    /// corners taken from the zoomed-out projection back to the picture's.
    fn cast(&mut self, start: usize, corners: usize) {
        let new: Vec<u32> = self.batch.drain(start..).collect();
        let (sb, db) = self.state.blend;
        let solid = (sb, db) == (4, 0) || (sb, db) == (1, 5);
        if !(self.gbuf && self.gdepth.is_some() && solid && corners >= 3 && super::shadows::enabled()) {
            return;
        }
        let Some((cx, cy)) = super::ao::centre() else { return };
        let g = [0, 1].map(|i| f32::from_bits(RING_ZOOM[i].load(Ordering::Relaxed)));
        let key = self.state.key;
        let kc = [(key >> 16 & 0xff) as f32 / 255.0, (key >> 8 & 0xff) as f32 / 255.0, (key & 0xff) as f32 / 255.0, self.state.chroma as u32 as f32].map(f32::to_bits);
        for t in new.chunks_exact(3 * VERTEX) {
            if !t.chunks_exact(VERTEX).all(|v| f32::from_bits(v[20]) > 0.0) {
                continue;
            }
            for v in t.chunks_exact(VERTEX) {
                let (x, y) = (f32::from_bits(v[0]), f32::from_bits(v[1]));
                self.casters.extend([(cx + (x - cx) * g[0]).to_bits(), (cy + (y - cy) * g[1]).to_bits()]);
                self.casters.extend_from_slice(&v[2..8]);
                self.casters.extend(kc);
                self.casters.extend_from_slice(&v[12..]);
            }
        }
    }

    /// A glow's colour: the local colour (the vertices' mean or the constant, by grColorCombine)
    /// times the mean of its texture rectangle `st` (when the combine takes the texture), times
    /// its alpha.
    fn glow_colour(&mut self, verts: &[u32], st: [f32; 4]) -> [f32; 3] {
        let m = mem();
        let n = verts.len().max(1) as f32;
        let avg = |o: u32| verts.iter().map(|&v| f32::from_bits(m.rd32(v + o))).sum::<f32>() / n / 255.0;
        let k = self.constant;
        let konst = |sh: u32| (k >> sh & 0xff) as f32 / 255.0;
        let local = if self.cc[2] == 0 { [avg(0xc), avg(0x10), avg(0x14)] } else { [konst(16), konst(8), konst(0)] };
        // grAlphaCombine (its factor taken as 1): local iterated or constant, "other" iterated,
        // the texture (taken as 1: keyed-out texels already count black) or constant
        let local_a = if self.ac[2] == 0 { avg(0x1c) } else { konst(24) };
        let other_a = match self.ac[3] {
            0 => avg(0x1c),
            1 => 1.0,
            _ => konst(24),
        };
        let alpha = match (self.state.blend.0, self.ac[0]) {
            (4, _) => 1.0,
            (_, 0) => 0.0,
            (_, 1 | 2) => local_a,
            (_, 3) => other_a,
            (_, 4 | 5) => other_a + local_a,
            (_, 6) => other_a - local_a,
            _ => other_a,
        }
        .clamp(0.0, 1.0);
        // grColorCombine: function 1 is the local colour alone; "other" 1 the texture
        let c = if self.cc[0] != 1 && self.cc[3] == 1 {
            let t = self.tex.and_then(|a| self.textures.get_mut(&a)).map_or([1.0; 3], |t| t.average(st));
            if self.cc[1] == 8 { t } else { [t[0] * local[0], t[1] * local[1], t[2] * local[2]] }
        } else {
            local
        };
        c.map(|c| (c * alpha).clamp(0.0, 1.0))
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
    fn download(&mut self, addr: u32, lod: u32, aspect: u32, format: u32, data: u32, rows: Option<(u32, u32)>) {
        let (w, h) = lod_size(lod, aspect);
        let bytes = w * h * bytes_per_texel(format);
        if trace() {
            log!("glide: download {addr:#x} {w}x{h} format {format:#x} from {data:#x} rows {rows:?}");
        }
        self.downloads += 1;
        let stamp = self.downloads;
        match self.textures.get_mut(&addr) {
            Some(t) if (t.w, t.h, t.bytes) == (w, h, bytes) => t.stamp = stamp,
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
                let n = (w * h) as usize;
                self.textures.insert(addr, Texture { layer, w, h, bytes, stamp, format, raw: None, dirty: false, rgb: vec![0; n * 3], means: HashMap::new(), staged: vec![0; n * 4], staged_alpha: vec![0; n * 2], pending: None });
            }
        }
        let (r0, r1) = rows.unwrap_or((0, h - 1));
        let r1 = r1.min(h - 1);
        if r0 > r1 {
            return;
        }
        let bpt = bytes_per_texel(format);
        let src = mem().slice(data, ((r1 - r0 + 1) * w * bpt) as usize).to_vec();
        let t = self.textures.get_mut(&addr).unwrap();
        if palettised(format) {
            let at = (r0 * w * bpt) as usize;
            // the same texels again (the game downloads a page whose rows it marked, each
            // picture): nothing to convert
            if t.format == format && t.raw.as_ref().is_some_and(|raw| raw[at..at + src.len()] == src[..]) {
                SAME_ROWS.fetch_add(r1 - r0 + 1, Ordering::Relaxed);
                return;
            }
            let raw = t.raw.get_or_insert_with(|| vec![0; (w * h * bpt) as usize]);
            raw[at..at + src.len()].copy_from_slice(&src);
        } else {
            t.raw = None;
        }
        t.format = format;
        let px = convert(format, &src, &self.palette);
        t.keep_rgb(r0, &px);
        t.stage(r0, r1, &px);
    }

    /// Before drawing with the texture at `addr`: a palettised one converted again with the
    /// current palette, the rows downloaded since its last use uploaded (what was batched
    /// before drawn first, with the old texels).
    fn refresh(&mut self, gl: &glow::Context, addr: u32) {
        let Some(t) = self.textures.get_mut(&addr) else { return };
        if t.dirty {
            t.dirty = false;
            REFRESHES.fetch_add(1, Ordering::Relaxed);
            if let Some(raw) = &t.raw {
                let px = convert(t.format, raw, &self.palette);
                t.keep_rgb(0, &px);
                t.stage(0, t.h - 1, &px);
            }
        }
        if t.pending.is_some() {
            self.flush(gl);
            self.commit(gl, addr);
        }
    }

    /// Upload a texture's pending rows into its layer.
    fn commit(&mut self, gl: &glow::Context, addr: u32) {
        let Some(t) = self.textures.get_mut(&addr) else { return };
        let Some((r0, r1)) = t.pending.take() else { return };
        let (w, layer) = (t.w as usize, t.layer);
        UPLOADS.fetch_add(1, Ordering::Relaxed);
        UPLOAD_TEXELS.fetch_add(((r1 - r0 + 1) as usize * w) as u32, Ordering::Relaxed);
        let rows = r0 as usize * w..(r1 as usize + 1) * w;
        unsafe {
            for (tex, format, px) in [(self.array, glow::RGBA, &t.staged[rows.start * 4..rows.end * 4]), (self.alpha, glow::RG, &t.staged_alpha[rows.start * 2..rows.end * 2])] {
                gl.bind_texture(glow::TEXTURE_2D_ARRAY, Some(tex));
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
        }
        self.stale |= 1 << layer;
    }

    /// The mipmaps of the stale layers, each level filtered from the one above.
    fn build_mipmaps(&mut self, gl: &glow::Context) {
        let [read, draw] = self.local(gl).mip;
        unsafe {
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(read));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(draw));
            for layer in (0..LAYERS).filter(|l| self.stale >> l & 1 != 0) {
                for t in [self.array, self.alpha] {
                    for level in 1..LEVELS {
                        let (a, b) = (256 >> (level - 1), 256 >> level);
                        gl.framebuffer_texture_layer(glow::READ_FRAMEBUFFER, glow::COLOR_ATTACHMENT0, Some(t), level - 1, layer as i32);
                        gl.framebuffer_texture_layer(glow::DRAW_FRAMEBUFFER, glow::COLOR_ATTACHMENT0, Some(t), level, layer as i32);
                        gl.blit_framebuffer(0, 0, a, a, 0, 0, b, b, glow::COLOR_BUFFER_BIT, glow::LINEAR);
                    }
                }
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        self.stale = 0;
    }
}

// ---- the Glide functions ---------------------------------------------------------------------

/// NSTRIKE_GLIDE_LOG=1: texture downloads and other rarely called functions.
fn trace() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var_os("NSTRIKE_GLIDE_LOG").is_some_and(|v| v != "0"))
}

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
        super::wide::reset();
        let (ww, wh) = super::window::window_size();
        let a = super::wide::set_aspect(ww as f32 / wh.max(1) as f32);
        super::window::with_gl(|gl| g.resize(gl, a, wh));
    }
    let ok = g.is_some();
    *GLIDE.lock().unwrap_or_else(|e| e.into_inner()) = g;
    ok as u32
}

fn sst_win_close(_: &mut Cpu) -> u32 {
    with(|g, gl| unsafe {
        g.drop_gbuffer(gl);
        if let Some(mut s) = g.ssao.take() {
            s.delete(gl);
        }
        if let Some(mut l) = g.lighting.take() {
            l.delete(gl);
        }
        if let Some(mut m) = g.shadow_map.take() {
            m.delete(gl);
        }
        if let Some(mut b) = g.bloom.take() {
            b.delete(gl);
        }
        if let Some(mut h) = g.heat.take() {
            h.delete(gl);
        }
        // this thread's own objects (other threads' cannot be deleted here)
        if let Some(l) = g.locals.remove(&thread_id()) {
            l.delete(gl);
        }
        if let Some((rb, _)) = g.ms.take() {
            gl.delete_renderbuffer(rb);
        }
        for t in [g.color, g.front, g.aux] {
            gl.delete_texture(t);
        }
        // the TMU's memory stays (see `open`)
        *KEPT.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Kept { array: g.array, alpha: g.alpha, textures: std::mem::take(&mut g.textures), palette: g.palette });
        for sv in g.saved.drain(..) {
            gl.delete_texture(sv.tex);
        }
        super::gl::delete_program(gl, g.copy_prog);
        gl.delete_program(g.prog);
        gl.delete_buffer(g.vbo);
    });
    *GLIDE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    super::wide::reset();
    1
}


fn buffer_clear(c: &mut Cpu) -> u32 {
    let color = arg(c, 0);
    with(|g, gl| {
        g.flush(gl);
        let fbo = g.draw_fbo(gl);
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            g.scissor(gl, g.state.clip);
            let ch = |sh: u32| (color >> sh & 0xff) as f32 / 255.0;
            gl.clear_color(ch(16), ch(8), ch(0), 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.disable(glow::SCISSOR_TEST);
            // another thread may draw next (see `with`)
            gl.flush();
        }
    });
    0
}

/// grBufferSwap(swap_interval): the card's swap waited for its retrace; the pace is
/// `frames::pace` (without a wait the menu ran at hundreds of frames).
fn buffer_swap(c: &mut Cpu) -> u32 {
    super::frames::pace(arg(c, 0));
    let shown = with(|g, gl| {
        // no HUD primitive followed the 3D picture (HUD off): the passes now
        if g.gbuf && !g.posted && g.gdepth.is_some() {
            g.post_3d(gl);
        }
        g.swaps += 1;
        g.flush(gl);
        g.resolve(gl);
        g.clear_gbuffer(gl);
        g.casters.clear();
        g.gbuf = false;
        g.posted = false;
        // the buffers trade places, as on the card: the game keeps what is under its cursor
        // for each of them
        std::mem::swap(&mut g.color, &mut g.front);
        if let Some((rb, _)) = g.ms {
            let (w, h) = (g.target.0 as i32, g.target.1 as i32);
            let ms = g.rb_fbo(gl, rb);
            g.copy(gl, g.color, [0.0, 0.0, 1.0, 1.0], ms, [0, 0, w, h]);
        }
        // the frame's commands to the GPU: another thread may draw the next one (the text
        // before a mission); unflushed, this thread's last movie frame landed in a buffer
        // after that thread had cleared it, and flickered under the text
        unsafe { gl.flush() };
        (g.front, g.target, g.gamma)
    });
    super::ao::clear();
    if let Some((tex, (w, h), gamma)) = shown {
        super::window::show_texture(tex, w, h, gamma);
        // the next frame follows the window: its height, and its aspect in a mission
        follow_window();
    }
    0
}

/// Size the target for the window: its height, and its aspect in a mission (between frames:
/// the buffers are made anew, black).
pub fn follow_window() {
    let (ww, wh) = super::window::window_size();
    let a = super::wide::set_aspect(ww as f32 / wh.max(1) as f32);
    with(|g, gl| g.resize(gl, a, wh));
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

/// The primitives that follow belong to the 3D picture (true) or the HUD (false, the first
/// of a frame runs the ambient occlusion over the 3D picture).
pub fn set_3d(on: bool) {
    with(|g, gl| {
        if g.posted || on == g.gbuf {
            return;
        }
        if on {
            g.flush(gl);
            g.gbuf = true;
        } else if g.gdepth.is_some() {
            g.post_3d(gl);
        } else {
            g.flush(gl);
            g.gbuf = false;
        }
    });
}

/// The shadows' object ring's zoom (`pictures::shadow_ring`), x and y as f32 bits.
static RING_ZOOM: [AtomicU32; 2] = [AtomicU32::new(0x3f80_0000), AtomicU32::new(0x3f80_0000)];

pub fn set_ring_zoom(g: (f32, f32)) {
    RING_ZOOM[0].store(g.0.to_bits(), Ordering::Relaxed);
    RING_ZOOM[1].store(g.1.to_bits(), Ordering::Relaxed);
}

/// Whether the primitives that follow only cast shadows (the ring's, drawn zoomed out).
pub fn set_cast_only(on: bool) {
    with(|g, _| g.cast_only = on);
}

/// Whether the primitives that follow are an object's drawn in the picture (not shadow
/// casters: the ring draws all objects for the shadows, `pictures::shadow_ring`).
pub fn set_no_cast(on: bool) {
    with(|g, _| g.no_cast = on);
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
    with(|g, _| g.download(addr, large, aspect, format, data, None));
    0
}

fn tex_download_mipmap_level(c: &mut Cpu) -> u32 {
    let (addr, this, large, aspect, format, data) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 7));
    if this == large {
        with(|g, _| g.download(addr, this, aspect, format, data, None));
    }
    0
}

fn tex_download_mipmap_level_partial(c: &mut Cpu) -> u32 {
    let (addr, this, large, aspect, format, data, r0, r1) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 7), arg(c, 8), arg(c, 9));
    if this == large {
        // `data` points at the first row downloaded
        with(|g, _| g.download(addr, this, aspect, format, data, Some((r0, r1))));
    }
    0
}

/// Counts for `NSTRIKE_FPS`: uploads, their texels, palettised textures converted again,
/// palette changes.
pub static SAME_ROWS: AtomicU32 = AtomicU32::new(0);
pub static DRAWS: AtomicU32 = AtomicU32::new(0);
pub static UPLOADS: AtomicU32 = AtomicU32::new(0);
pub static UPLOAD_TEXELS: AtomicU32 = AtomicU32::new(0);
pub static REFRESHES: AtomicU32 = AtomicU32::new(0);
pub static PALETTES: AtomicU32 = AtomicU32::new(0);

/// Entries `first..=last` of a table (GrTexTable_t: NCC0 0, NCC1 1, PALETTE 2) from `data`,
/// which holds the whole table.
fn download_table(kind: u32, data: u32, first: u32, last: u32) {
    if kind != 2 {
        log!("warn: grTexDownloadTable: NCC table {kind} ignored");
        return;
    }
    let m = mem();
    with(|g, _| {
        let mut changed = false;
        for i in first..=last.min(255) {
            let v = m.rd32(data + 4 * i);
            changed |= g.palette[i as usize] != v;
            g.palette[i as usize] = v;
        }
        if changed {
            PALETTES.fetch_add(1, Ordering::Relaxed);
            for t in g.textures.values_mut().filter(|t| t.raw.is_some()) {
                t.dirty = true;
            }
        }
    });
}

fn tex_download_table(c: &mut Cpu) -> u32 {
    download_table(arg(c, 1), arg(c, 2), 0, 255);
    0
}

fn tex_download_table_partial(c: &mut Cpu) -> u32 {
    download_table(arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
    0
}

fn alpha_test_function(c: &mut Cpu) -> u32 {
    let f = arg(c, 0) & 7;
    with(|g, _| g.alpha_test.0 = f);
    0
}

fn alpha_test_reference(c: &mut Cpu) -> u32 {
    let v = arg(c, 0) & 0xff;
    with(|g, _| g.alpha_test.1 = v);
    0
}

/// The game's `gl_CopyBitmap_(eax bitmap, edx pixels, ebx stride, ecx x, [esp+4] y,
/// [esp+8] flags)`: flags bit 0 saves the back buffer's rectangle under the bitmap's size into
/// the pixels (grLfbReadRegion, at 640x480 in RGB565), else the pixels are drawn there as
/// 128x128 textures. What the cursor puts back (the background it saved) does not come out as
/// it was on modern systems; here it was blurred and a little darker. So a save keeps the
/// rectangle on the GPU instead, and drawing back pixels that are still what they were at the
/// save draws that.
pub const COPY_BITMAP: u32 = 0x47a360;

pub fn copy_bitmap(c: &mut Cpu) {
    let m = mem();
    let (bitmap, data, stride, x) = (c.r[EAX], c.r[EDX], c.r[EBX], c.r[ECX] & 0xffff);
    let (y, flags) = (m.rd32(c.r[ESP] + 4) & 0xffff, m.rd32(c.r[ESP] + 8));
    // the bitmap: x and y of the part drawn, its width and height (16 bits each)
    let (bx, by, w, h) = (m.rd16(bitmap), m.rd16(bitmap + 2), m.rd16(bitmap + 4) as u32, m.rd16(bitmap + 6) as u32);
    if trace() {
        log!("glide: gl_CopyBitmap {x},{y} {w}x{h} part at {bx},{by} pixels {data:#x} stride {stride} flags {flags:#x}");
    }
    if flags & 1 != 0 {
        // kept on the GPU only: reading the pixels back (grLfbReadRegion) waits for the GPU to
        // finish the frame, every frame (the menu ran at 30 fps). The pixels in guest memory
        // stay as they are: only gl_CopyBitmap_ uses them, to draw them back (`restore`).
        with(|g, gl| g.save(gl, [x, y, w, h], data, stride));
        c.r[EAX] = (c.r[EAX] & !0xff) | 1;
        c.r[ESP] += 12;
        return;
    }
    // drawn back as it was (flag 8: with the chroma key, the cursor itself)
    if (bx, by) == (0, 0) && flags & 8 == 0 && with(|g, gl| g.restore(gl, (x, y), (w, h), data, stride)) == Some(true) {
        c.r[EAX] = (c.r[EAX] & !0xff) | 1;
        // ret 8
        c.r[ESP] += 12;
        return;
    }
    crate::rt::call_original(c, COPY_BITMAP);
}

/// grLfbReadRegion(src_buffer, x, y, w, h, dst_stride, dst_data)
fn lfb_read_region(c: &mut Cpu) -> u32 {
    let (b, rect, stride, dst) = (arg(c, 0), [arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4)], arg(c, 5), arg(c, 6));
    with(|g, gl| g.lfb_read(gl, b, rect, stride, dst)).is_some() as u32
}

/// grLfbWriteRegion(dst_buffer, x, y, src_format, w, h, src_stride, src_data)
fn lfb_write_region(c: &mut Cpu) -> u32 {
    let (b, at, format, size, stride, data) = (arg(c, 0), (arg(c, 1), arg(c, 2)), arg(c, 3), (arg(c, 4), arg(c, 5)), arg(c, 6), arg(c, 7));
    with(|g, gl| g.lfb_write(gl, b, at, format, size, stride, data)).is_some() as u32
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
    // antialiased edges: drawn as they are (MSAA smooths them)
    ("grAADrawTriangle", draw_triangle),
    ("guAADrawTriangleWithClip", draw_triangle),
    ("grAADrawLine", draw_line),
    ("grAADrawPoint", draw_point),
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
    ("grTexDownloadTable", tex_download_table),
    ("grTexDownloadTablePartial", tex_download_table_partial),
    ("grAlphaTestFunction", alpha_test_function),
    ("grAlphaTestReferenceValue", alpha_test_reference),
    ("grBufferNumPending", ret0),
    ("grLfbReadRegion", lfb_read_region),
    ("grLfbWriteRegion", lfb_write_region),
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
