//! GDI32: the little GDI the game uses (device capabilities; fonts and text on a DC it only
//! uses for diagnostics). Drawing on a DC does nothing.

use super::user32::{DESKTOP_H, DESKTOP_W};
use super::{Import, arg, mem};
use crate::rt::Cpu;

/// The one device context: the screen.
pub const SCREEN_DC: u32 = 0xdc00;

fn get_device_caps(c: &mut Cpu) -> u32 {
    match arg(c, 1) {
        8 => DESKTOP_W,  // HORZRES
        10 => DESKTOP_H, // VERTRES
        12 => 16,        // BITSPIXEL
        14 => 1,         // PLANES
        88 | 90 => 96,   // LOGPIXELSX/Y
        116 => 60,       // VREFRESH
        _ => 0,
    }
}

fn get_text_metrics(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    let m = mem();
    m.slice(p, 56).fill(0);
    // height, ascent, descent, internal and external leading, average and maximum width, weight
    for (k, v) in [16u32, 13, 3, 0, 0, 8, 16, 400].into_iter().enumerate() {
        m.wr32(p + 4 * k as u32, v);
    }
    1
}

fn handle(_: &mut Cpu) -> u32 {
    0x6600
}
fn ret0(_: &mut Cpu) -> u32 {
    0
}
fn ret1(_: &mut Cpu) -> u32 {
    1
}

pub const IMPORTS: &[Import] = &[
    ("GetDeviceCaps", 2, get_device_caps),
    ("GetTextMetricsA", 2, get_text_metrics),
    ("GetStockObject", 1, handle),
    ("CreateFontA", 14, handle),
    ("SelectObject", 2, handle),
    ("DeleteObject", 1, ret1),
    ("TextOutA", 5, ret1),
    ("SetBkColor", 2, ret0),
    ("SetBkMode", 2, ret1),
    ("SetTextColor", 2, ret0),
];
