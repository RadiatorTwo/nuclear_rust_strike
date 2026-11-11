//! A depth buffer for the 3D picture. The game draws without one, back to front by its
//! ordering table (as on the PlayStation): single triangles behind other geometry came out in
//! front. The depth of each corner is known from the hooked projections (`ao.rs`, the depth
//! attachment's); with it `glide` tests the 3D primitives whose corners all have one:
//! solid ones test and write, blended and darkening ones only test, a little nearer (`BIAS`:
//! shadows and spray on the ground do not flicker). Primitives without depth (HUD, menus,
//! sprites without projected points) are drawn in the game's order as before; a batch holds
//! only one kind (`glide`'s `batch_z`). The buffer (32-bit float, 1 - near / z) sits beside the
//! depth attachment, with its samples, and is cleared with it every frame.
//!
//! Setting `depth_buffer` (F11), `NSTRIKE_ZBUFFER=0/1`.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// What blended primitives are pulled nearer by, as a share of their depth.
pub const BIAS: f32 = 0.004;

/// At start: the setting.
pub fn init() {
    ENABLED.store(crate::settings::get().depth_buffer, Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// The F11 menu: saved, used from the next picture on.
pub fn set_enabled(on: bool) {
    crate::settings::update(|s| s.depth_buffer = on);
    ENABLED.store(on, Relaxed);
}

/// A primitive's corner depths (0: none): when at least half have one, the others take their
/// mean and true is returned (tested); else all are 0 (drawn in the game's order).
pub fn fill(z: &mut [f32]) -> bool {
    let (n, sum) = z.iter().filter(|&&d| d > 0.0).fold((0usize, 0.0f32), |a, &d| (a.0 + 1, a.1 + d));
    if n == 0 || n * 2 < z.len() {
        z.iter_mut().for_each(|d| *d = 0.0);
        return false;
    }
    let mean = sum / n as f32;
    z.iter_mut().filter(|d| **d <= 0.0).for_each(|d| *d = mean);
    true
}
