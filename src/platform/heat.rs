//! Heat shimmer, as future_rust_cop's `heat.rs`: the picture wavers above fire and
//! explosions.
//!
//! Sources: the additive primitives of the game's fire and explosion pyros (the dynamic
//! lights' sources, `lights::gives_light`) with a depth (`ao.rs`), collected by `glide`'s
//! `add`: centre and size in target pixels, z. Near ones are merged, the largest `MAX` kept.
//!
//! Applied after the lights, before the bloom (`glide`'s `post_3d`): the picture is copied
//! (resolved with MSAA), then every pixel near a source reads the copy a few pixels off
//! (scrolling waves rising above the source), less toward the source's edge. Not where the
//! pixel's own geometry is in front of the source (foreground stays sharp), and not from a
//! place much nearer than the pixel. Pixels without an offset are left alone (MSAA kept).
//!
//! Setting `heat_shimmer` (F11), `NSTRIKE_HEAT=0/1`; tuning `NSTRIKE_HEAT_AMPLITUDE` (the
//! largest offset as a share of the height).

use super::ao;
use super::gl::compile;
use glow::HasContext;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// At start: the setting.
pub fn init() {
    ENABLED.store(crate::settings::get().heat_shimmer, Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// The F11 menu: saved, used from the next picture on.
pub fn set_enabled(on: bool) {
    crate::settings::update(|s| s.heat_shimmer = on);
    ENABLED.store(on, Relaxed);
}

/// The most sources a picture.
const MAX: usize = 32;
/// The largest offset, as a share of the target's height.
const AMPLITUDE: f32 = 1.0 / 130.0;

/// A source: centre (target pixels from the bottom left), z, radius in pixels, fire or not.
#[derive(Clone, Copy, Debug)]
pub struct Source {
    pub at: [f32; 2],
    pub z: f32,
    pub radius: f32,
    pub fire: bool,
}

/// The largest source in the world (object units across, half of it as the radius; a tree is
/// ~700 high): large explosions shimmer only around their middle.
const MAX_SIZE: f32 = 1200.0;

/// A source from an effect's corners (target pixels from the bottom left) and z (0 unknown);
/// `focal` the camera's pixels per unit at z = 1 (y).
pub fn source(corners: &[(f32, f32)], z: &[f32], fire: bool, focal: f32) -> Option<Source> {
    let (mut n, mut zs) = (0.0, 0.0);
    for &z in z.iter().filter(|&&z| z > 0.0) {
        n += 1.0;
        zs += z;
    }
    if n == 0.0 || corners.is_empty() {
        return None;
    }
    let (x0, x1) = corners.iter().fold((f32::MAX, f32::MIN), |a, p| (a.0.min(p.0), a.1.max(p.0)));
    let (y0, y1) = corners.iter().fold((f32::MAX, f32::MIN), |a, p| (a.0.min(p.1), a.1.max(p.1)));
    let z = zs / n;
    let radius = (0.4 * (x1 - x0).hypot(y1 - y0)).min(0.5 * MAX_SIZE * focal / z);
    (radius >= 2.0).then_some(Source { at: [(x0 + x1) * 0.5, (y0 + y1) * 0.5], z, radius, fire })
}

/// Sources nearly at the same place as one (the larger), the largest `MAX`.
fn merge(sources: &[Source]) -> Vec<Source> {
    let mut v = sources.to_vec();
    v.sort_by(|a, b| b.radius.total_cmp(&a.radius));
    let mut out: Vec<Source> = Vec::new();
    for s in v {
        // sorted by size: the one kept is the larger
        let near = out.iter().any(|o| o.fire == s.fire && (o.at[0] - s.at[0]).hypot(o.at[1] - s.at[1]) < 0.3 * o.radius);
        if !near && out.len() < MAX {
            out.push(s);
        }
    }
    out
}

const HEAT_FS: &str = r#"
const int MAX = 32;
uniform int count;
uniform vec4 src[MAX];    // x, y (pixels), z, radius (pixels)
uniform int fire[MAX];
uniform sampler2D picture; // the view's picture, copied
uniform float t;          // seconds
uniform float amp;        // the largest offset in pixels
out vec4 o;
void main() {
    vec2 f = gl_FragCoord.xy;
    ivec2 q = ivec2(f);
    float zq = G(q).r;
    vec2 off = vec2(0.0);
    for (int i = 0; i < count; i++) {
        vec2 d = f - src[i].xy;
        float r = src[i].w;
        bool hot = fire[i] != 0;
        // heat rises: above a fire, a taller shimmer
        if (hot) {
            d.y -= 0.7 * r;
            d.y *= 0.6;
            r *= 1.3;
        }
        float w = 1.0 - length(d) / r;
        if (w <= 0.0) continue;
        // in front of the source: sharp
        float rw = src[i].w * src[i].z / focal.y;
        if (zq > 0.0 && zq < src[i].z - rw) continue;
        // the waves move with the source (on the screen they ran past a moving one: faster)
        vec2 p = (f - src[i].xy) / max(src[i].w * 0.35, 6.0);
        float rise = hot ? t * 4.0 : 0.0;
        vec2 n = vec2(sin(p.y * 2.3 - rise * 2.0 + t * 11.0) + sin(p.x * 1.1 + t * 7.0),
                      cos(p.x * 1.9 - t * 9.8) + sin(p.y * 1.4 - rise * 3.0 + t * 8.3)) * 0.25;
        off += n * w * w * amp;
    }
    if (dot(off, off) < 0.09) discard;
    vec2 s = f + off;
    // not a nearer thing pulled into the shimmer
    float zs = G(ivec2(s)).r;
    if (zs > 0.0 && (zq <= 0.0 || zs < zq * 0.9)) discard;
    o = vec4(texture(picture, s / vec2(size)).rgb, 1.0);
}
"#;

/// The pass's programs (single-sampled and multisampled depth) and the picture's copy.
pub struct Heat {
    progs: [glow::Program; 2],
    vao: glow::VertexArray,
    /// the copy: its size, texture, framebuffer
    copy: Option<((u32, u32), glow::Texture, glow::Framebuffer)>,
    start: std::time::Instant,
}

impl Heat {
    pub fn new(gl: &glow::Context) -> Heat {
        let prog = |ms: bool| compile(gl, ao::VS, &format!("{}{}{HEAT_FS}", if ms { "#define MS\n" } else { "" }, ao::COMMON));
        let vao = unsafe { gl.create_vertex_array().expect("GL vertex array") };
        Heat { progs: [prog(false), prog(true)], vao, copy: None, start: std::time::Instant::now() }
    }

    fn drop_copy(&mut self, gl: &glow::Context) {
        if let Some((_, t, f)) = self.copy.take() {
            unsafe {
                gl.delete_framebuffer(f);
                gl.delete_texture(t);
            }
        }
    }

    pub fn delete(&mut self, gl: &glow::Context) {
        self.drop_copy(gl);
        unsafe {
            for p in self.progs {
                super::gl::delete_program(gl, p);
            }
            gl.delete_vertex_array(self.vao);
        }
    }

    fn copy(&mut self, gl: &glow::Context, target: (u32, u32)) -> (glow::Texture, glow::Framebuffer) {
        if let Some((size, t, f)) = self.copy {
            if size == target {
                return (t, f);
            }
        }
        self.drop_copy(gl);
        unsafe {
            let t = super::gl::texture(gl, false);
            for p in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, p, glow::LINEAR as i32);
            }
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, target.0 as i32, target.1 as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            let f = gl.create_framebuffer().expect("GL framebuffer");
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(f));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(t), 0);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.copy = Some((target, t, f));
            (t, f)
        }
    }

    /// The shimmer over the 3D picture: `depth` the attachment, `target` its size, `out` a
    /// framebuffer with only the picture's colour, `clip` the scissor box.
    #[allow(clippy::too_many_arguments)]
    pub fn run(&mut self, gl: &glow::Context, depth: glow::Texture, ms: bool, target: (u32, u32), out: glow::Framebuffer, clip: [i32; 4], focal: (f32, f32), sources: &[Source]) {
        if sources.is_empty() || !matches!(ao::debug_view(), 0 | 5) {
            return;
        }
        let merged = merge(sources);
        let src: Vec<f32> = merged.iter().flat_map(|s| [s.at[0], s.at[1], s.z, s.radius]).collect();
        let fire: Vec<i32> = merged.iter().map(|s| s.fire as i32).collect();
        let (w, h) = (target.0 as i32, target.1 as i32);
        let (copy, copy_fbo) = self.copy(gl, target);
        unsafe {
            // the view's part of the picture, resolved
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(out));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(copy_fbo));
            let [x, y, cw, ch] = clip;
            gl.blit_framebuffer(x, y, x + cw, y + ch, x, y, x + cw, y + ch, glow::COLOR_BUFFER_BIT, glow::NEAREST);
            let p = self.progs[ms as usize];
            gl.use_program(Some(p));
            let u = |n: &'static str| super::gl::uniform(gl, p, n);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(if ms { glow::TEXTURE_2D_MULTISAMPLE } else { glow::TEXTURE_2D }, Some(depth));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(copy));
            gl.uniform_1_i32(u("gdepth").as_ref(), 0);
            gl.uniform_1_i32(u("picture").as_ref(), 1);
            gl.uniform_2_i32(u("size").as_ref(), w, h);
            gl.uniform_2_f32(u("focal").as_ref(), focal.0, focal.1);
            gl.uniform_1_i32(u("count").as_ref(), merged.len() as i32);
            gl.uniform_4_f32_slice(u("src").as_ref(), &src);
            gl.uniform_1_i32_slice(u("fire").as_ref(), &fire);
            gl.uniform_1_f32(u("t").as_ref(), self.start.elapsed().as_secs_f32() % 1000.0);
            gl.uniform_1_f32(u("amp").as_ref(), h as f32 * ao::tune("NSTRIKE_HEAT_AMPLITUDE", AMPLITUDE));
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(out));
            gl.viewport(0, 0, w, h);
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(x, y, cw, ch);
            gl.disable(glow::BLEND);
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }
}
