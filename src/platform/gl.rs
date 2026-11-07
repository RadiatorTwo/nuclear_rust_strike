//! OpenGL: the context of the game window and what it shows. A shown picture is either a 2D
//! frame (DirectDraw: menus, the software renderer) uploaded as a texture, or a texture
//! something else rendered (the Glide scene); a video and the dialog overlay go on top.

use glow::HasContext;
use glutin::context::{ContextApi, ContextAttributesBuilder, GlProfile, NotCurrentContext, NotCurrentGlContext, PossiblyCurrentContext, Version};
use glutin::config::GlConfig;
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SwapInterval, WindowSurface};
use glutin_winit::GlWindow;
use raw_window_handle::HasWindowHandle;
use std::num::NonZeroU32;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes};

/// GLSL for desktop OpenGL 3.3 core.
pub const GLSL: &str = "#version 330 core\n";

pub struct Gl {
    pub gl: glow::Context,
    /// boxed: a shared context on another thread draws with it too (`share`)
    surface: Box<Surface<WindowSurface>>,
    context: PossiblyCurrentContext,
    config: glutin::config::Config,
    raw: Option<raw_window_handle::RawWindowHandle>,
    blit: glow::Program,
    vao: glow::VertexArray,
    frame: glow::Texture,
    overlay: glow::Texture,
    has_overlay: bool,
    video: glow::Texture,
    /// where the video is shown: x, y, width, height as parts of the 4:3 picture
    video_size: Option<(u32, u32)>,
    /// what is shown: a texture, its size, whether its pixels are 0RGB/ARGB words (2D frames:
    /// the bytes are B G R A), and the gamma to show it with
    pub shown: (glow::Texture, u32, u32, bool, f32),
}

thread_local! {
    static UNIFORMS: std::cell::RefCell<std::collections::HashMap<(glow::Program, &'static str), Option<glow::UniformLocation>>> = Default::default();
}

/// A uniform's location in program `p`, asked of the driver once (asking for each in every
/// picture cost the effects' passes ~0.2 ms).
pub fn uniform(gl: &glow::Context, p: glow::Program, name: &'static str) -> Option<glow::UniformLocation> {
    UNIFORMS.with_borrow_mut(|u| u.entry((p, name)).or_insert_with(|| unsafe { gl.get_uniform_location(p, name) }).clone())
}

/// Deletes program `p` and forgets its uniforms (a new program may get its number).
pub fn delete_program(gl: &glow::Context, p: glow::Program) {
    UNIFORMS.with_borrow_mut(|u| u.retain(|k, _| k.0 != p));
    unsafe { gl.delete_program(p) };
}

pub fn compile(gl: &glow::Context, vs: &str, fs: &str) -> glow::Program {
    unsafe {
        let p = gl.create_program().expect("GL program");
        for (kind, src) in [(glow::VERTEX_SHADER, vs), (glow::FRAGMENT_SHADER, fs)] {
            let s = gl.create_shader(kind).expect("GL shader");
            gl.shader_source(s, &format!("{GLSL}{src}"));
            gl.compile_shader(s);
            if !gl.get_shader_compile_status(s) {
                panic!("shader: {}", gl.get_shader_info_log(s));
            }
            gl.attach_shader(p, s);
        }
        gl.link_program(p);
        if !gl.get_program_link_status(p) {
            panic!("shader program: {}", gl.get_program_info_log(p));
        }
        p
    }
}

/// A texture with nearest or linear filtering, clamped.
pub fn texture(gl: &glow::Context, linear: bool) -> glow::Texture {
    unsafe {
        let t = gl.create_texture().expect("GL texture");
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        let f = if linear { glow::LINEAR } else { glow::NEAREST } as i32;
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, f);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, f);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
        t
    }
}

/// Upload 32-bit words (as their little-endian bytes, RGBA order) to a texture.
pub fn upload(gl: &glow::Context, t: glow::Texture, px: &[u32], w: u32, h: u32) {
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        let bytes = std::slice::from_raw_parts(px.as_ptr() as *const u8, px.len() * 4);
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            w as i32,
            h as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(bytes)),
        );
    }
}

const BLIT_VS: &str = r#"
uniform vec4 rect;
out vec2 uv;
void main() {
    vec2 c = vec2(gl_VertexID & 1, gl_VertexID >> 1);
    uv = vec2(c.x, 1.0 - c.y);
    gl_Position = vec4(mix(rect.xy, rect.zw, c), 0.0, 1.0);
}
"#;

const BLIT_FS: &str = r#"
uniform sampler2D tex;
uniform int words;
uniform int flip;
uniform float gamma;
in vec2 uv;
out vec4 color;
void main() {
    vec4 c = texture(tex, flip != 0 ? vec2(uv.x, 1.0 - uv.y) : uv);
    // 2D frames hold 0RGB/ARGB words: their bytes are B G R A
    if (words != 0) c = c.bgra;
    color = vec4(pow(c.rgb, vec3(1.0 / gamma)), c.a);
}
"#;

impl Gl {
    /// Create the window with an OpenGL 3.3 context.
    pub fn new(el: &ActiveEventLoop, attrs: WindowAttributes) -> (Window, Gl) {
        let template = glutin::config::ConfigTemplateBuilder::new().with_alpha_size(8);
        let (win, config) = glutin_winit::DisplayBuilder::new()
            .with_window_attributes(Some(attrs))
            .build(el, template, |configs| configs.reduce(|a, b| if b.num_samples() < a.num_samples() { b } else { a }).unwrap())
            .expect("cannot create an OpenGL window");
        let win = win.expect("no window");
        let display = config.display();
        let handle = win.window_handle().ok().map(|h| h.as_raw());
        let attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .with_profile(GlProfile::Core)
            .build(handle);
        let context = unsafe { display.create_context(&config, &attrs) }.expect("cannot create an OpenGL 3.3 context");
        let sattrs = win.build_surface_attributes(Default::default()).expect("surface attributes");
        let surface = unsafe { display.create_window_surface(&config, &sattrs) }.expect("cannot create the GL surface");
        let surface = Box::new(surface);
        let context = context.make_current(&surface).expect("cannot make the GL context current");
        // the game paces itself
        let _ = surface.set_swap_interval(&context, SwapInterval::DontWait);
        let gl = unsafe { glow::Context::from_loader_function_cstr(|s| display.get_proc_address(s)) };
        let blit = compile(&gl, BLIT_VS, BLIT_FS);
        let vao = unsafe { gl.create_vertex_array() }.expect("GL vertex array");
        let frame = texture(&gl, false);
        upload(&gl, frame, &vec![0; 640 * 480], 640, 480);
        let overlay = texture(&gl, true);
        let video = texture(&gl, true);
        let g = Gl { gl, surface, context, config, raw: handle, blit, vao, frame, overlay, has_overlay: false, video, video_size: None, shown: (frame, 640, 480, true, 1.0) };
        (win, g)
    }

    /// A context sharing this one's objects (textures), for another thread: the game draws its
    /// Glide scenes on its own thread, the window shows them on this one.
    pub fn share(&self) -> SharedParts {
        let attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .with_profile(GlProfile::Core)
            .with_sharing(&self.context)
            .build(self.raw);
        let display = self.config.display();
        let context = unsafe { display.create_context(&self.config, &attrs) }.expect("cannot create a shared OpenGL context");
        SharedParts { context, surface: &*self.surface, display }
    }

    /// Show a 2D frame of 0RGB pixels.
    pub fn frame(&mut self, px: &[u32], w: u32, h: u32) {
        upload(&self.gl, self.frame, px, w, h);
        self.shown = (self.frame, w, h, true, 1.0);
    }

    pub fn overlay(&mut self, o: Option<(&[u32], u32, u32)>) {
        self.has_overlay = o.is_some();
        if let Some((px, w, h)) = o {
            upload(&self.gl, self.overlay, px, w, h);
            // the game's thread may draw it next, with its context
            unsafe { self.gl.flush() };
        }
    }

    /// Show a video frame (0RGB, w x h), or no video.
    pub fn video(&mut self, v: Option<(&[u32], u32, u32)>) {
        self.video_size = v.map(|v| (v.1, v.2));
        if let Some((px, w, h)) = v {
            upload(&self.gl, self.video, px, w, h);
        }
    }

    pub fn resize(&self, w: u32, h: u32) {
        if let (Some(w), Some(h)) = (NonZeroU32::new(w), NonZeroU32::new(h)) {
            self.surface.resize(&self.context, w, h);
        }
    }

    /// What the window shows, in a window of w x h.
    pub fn look(&self, w: u32, h: u32) -> Look {
        Look {
            blit: self.blit,
            shown: self.shown,
            overlay: self.has_overlay.then_some(self.overlay),
            video: self.video_size.map(|(vw, vh)| (self.video, vw, vh)),
            size: (w, h),
        }
    }

    /// Draw the shown picture and the overlay into the window (w x h), letterboxed; with `shot`,
    /// read back the picture's area as 0RGB pixels.
    pub fn draw(&mut self, w: u32, h: u32, shot: bool) -> Option<(Vec<u32>, u32, u32)> {
        let _p = PAINT.lock().unwrap_or_else(|e| e.into_inner());
        let out = paint(&self.gl, self.vao, &self.look(w, h), shot);
        let _ = self.surface.swap_buffers(&self.context);
        out
    }
}

/// What the window shows: the picture (texture, size, 0RGB words or not, gamma), the overlay
/// and a video (textures shared by all contexts), the window's size.
#[derive(Clone, Copy)]
pub struct Look {
    blit: glow::Program,
    pub shown: (glow::Texture, u32, u32, bool, f32),
    overlay: Option<glow::Texture>,
    video: Option<(glow::Texture, u32, u32)>,
    pub size: (u32, u32),
}

/// One thread at a time draws into the window and swaps (the window's thread, or the game's
/// with its shared context).
static PAINT: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Draw `l` into the window's back buffer; with `shot`, read back the picture's area.
fn paint(gl: &glow::Context, vao: glow::VertexArray, l: &Look, shot: bool) -> Option<(Vec<u32>, u32, u32)> {
    let (w, h) = l.size;
    {
        let (tex, fw, fh, words, gamma) = l.shown;
        let (ox, oy, s) = super::window::fit(fw, fh, w, h);
        let (x0, y0) = (ox / w as f64 * 2.0 - 1.0, 1.0 - oy / h as f64 * 2.0);
        let (x1, y1) = (x0 + fw as f64 * s / w as f64 * 2.0, y0 - fh as f64 * s / h as f64 * 2.0);
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.viewport(0, 0, w as i32, h as i32);
            gl.disable(glow::SCISSOR_TEST);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            // the window stays opaque: 2D frames' 0RGB words have alpha 0, and a compositor that
            // blends windows by their alpha (Wayland) showed what is behind through the menu
            gl.color_mask(true, true, true, false);
            gl.use_program(Some(l.blit));
            gl.bind_vertex_array(Some(vao));
            gl.active_texture(glow::TEXTURE0);
            let u = |n: &'static str| uniform(gl, l.blit, n);
            gl.uniform_4_f32(u("rect").as_ref(), x0 as f32, y1 as f32, x1 as f32, y0 as f32);
            gl.uniform_1_i32(u("tex").as_ref(), 0);
            gl.disable(glow::BLEND);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.uniform_1_i32(u("words").as_ref(), words as i32);
            // rendered scenes have their first row at the bottom, uploaded frames at the top
            gl.uniform_1_i32(u("flip").as_ref(), !words as i32);
            gl.uniform_1_f32(u("gamma").as_ref(), gamma);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            // dialogs stay 4:3 over a wide picture
            let (sx, sw) = super::window::overlay_span(fw, fh);
            let (ox0, ox1) = (x0 + (x1 - x0) * sx as f64 / fw as f64, x0 + (x1 - x0) * (sx + sw) as f64 / fw as f64);
            // videos fill the window as far as their aspect allows, centred (the game placed
            // them in its 640x480 screen, briefings small and high up)
            if let Some((video, vw, vh)) = l.video {
                gl.clear(glow::COLOR_BUFFER_BIT);
                let (vx, vy, s) = super::window::fit(vw, vh, w, h);
                let (ax, ay) = (vx / w as f64 * 2.0 - 1.0, 1.0 - vy / h as f64 * 2.0);
                let (bx, by) = (ax + vw as f64 * s / w as f64 * 2.0, ay - vh as f64 * s / h as f64 * 2.0);
                gl.uniform_4_f32(u("rect").as_ref(), ax as f32, by as f32, bx as f32, ay as f32);
                gl.bind_texture(glow::TEXTURE_2D, Some(video));
                gl.uniform_1_i32(u("words").as_ref(), 1);
                gl.uniform_1_i32(u("flip").as_ref(), 0);
                gl.uniform_1_f32(u("gamma").as_ref(), 1.0);
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }
            if let Some(overlay) = l.overlay {
                gl.uniform_4_f32(u("rect").as_ref(), ox0 as f32, y1 as f32, ox1 as f32, y0 as f32);
                gl.enable(glow::BLEND);
                gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                gl.bind_texture(glow::TEXTURE_2D, Some(overlay));
                gl.uniform_1_i32(u("words").as_ref(), 1);
                gl.uniform_1_i32(u("flip").as_ref(), 0);
                gl.uniform_1_f32(u("gamma").as_ref(), 1.0);
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                gl.disable(glow::BLEND);
            }
            gl.color_mask(true, true, true, true);
        }
        let out = shot.then(|| {
            // the picture's area of the window, top row first
            let (rx, ry) = (ox.round() as i32, oy.round() as i32);
            let (rw, rh) = ((fw as f64 * s).round() as i32, (fh as f64 * s).round() as i32);
            let mut buf = vec![0u8; (rw * rh * 4) as usize];
            unsafe {
                gl.read_pixels(rx, h as i32 - ry - rh, rw, rh, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut buf)));
            }
            let px = (0..rh)
                .rev()
                .flat_map(|y| (0..rw).map(move |x| (x, y)))
                .map(|(x, y)| {
                    let i = ((y * rw + x) * 4) as usize;
                    (buf[i] as u32) << 16 | (buf[i + 1] as u32) << 8 | buf[i + 2] as u32
                })
                .collect();
            (px, rw as u32, rh as u32)
        });
        out
    }
}

/// A shared context (`Gl::share`) on its way to the thread that uses it.
pub struct SharedParts {
    context: NotCurrentContext,
    /// the window's surface: the Gl that made it lives as long as the window
    surface: *const Surface<WindowSurface>,
    display: glutin::display::Display,
}

// the context is made current on the receiving thread only; the surface is the window's, which
// two contexts may draw to on two threads
unsafe impl Send for SharedParts {}

/// A shared context current on its thread.
pub struct Shared {
    pub gl: glow::Context,
    context: PossiblyCurrentContext,
    surface: *const Surface<WindowSurface>,
    /// for drawing into the window (vertex arrays are not shared)
    vao: glow::VertexArray,
}

impl SharedParts {
    pub fn make_current(self) -> Shared {
        let context = self.context.make_current(unsafe { &*self.surface }).expect("cannot make the shared GL context current");
        let display = self.display;
        let gl = unsafe { glow::Context::from_loader_function_cstr(|s| display.get_proc_address(s)) };
        let vao = unsafe { gl.create_vertex_array() }.expect("GL vertex array");
        Shared { gl, context, surface: self.surface, vao }
    }
}

impl Shared {
    /// Draw `l` into the window and show it at once, from this thread (the window's thread
    /// shows at its next turn: pictures came 1 to 7 ms apart instead of 4.2).
    pub fn present(&self, l: &Look, shot: bool) -> Option<(Vec<u32>, u32, u32)> {
        let _p = PAINT.lock().unwrap_or_else(|e| e.into_inner());
        // Glide sets its viewport only when its view changes
        let mut vp = [0i32; 4];
        unsafe { self.gl.get_parameter_i32_slice(glow::VIEWPORT, &mut vp) };
        let out = paint(&self.gl, self.vao, l, shot);
        // SAFETY: the window's surface lives as long as the window (see SharedParts)
        let _ = unsafe { &*self.surface }.swap_buffers(&self.context);
        unsafe { self.gl.viewport(vp[0], vp[1], vp[2], vp[3]) };
        out
    }
}
