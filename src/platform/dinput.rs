//! DirectInput 5: the system keyboard and mouse (joysticks: none yet). The interfaces follow
//! future_rust_cop's (which had joysticks only).
//!
//! Keys and buttons come from `user32::poll` (`key`, `button`), mouse motion from the window
//! system's raw motion (`motion`): DirectInput's mouse is relative. Both are read as a state
//! (GetDeviceState: 256 key bytes; DIMOUSESTATE) or as buffered events (GetDeviceData:
//! DIDEVICEOBJECTDATA, after SetProperty(DIPROP_BUFFERSIZE)).
//! `NSTRIKE_DI_LOG=1` logs what the game asks.

use super::{Import, alloc, arg, free, guest_call, log, mem};
use crate::rt::Cpu;
use std::collections::VecDeque;
use std::sync::Mutex;

const DI_OK: u32 = 0;
const E_NOTIMPL: u32 = 0x8000_4001;
const E_NOINTERFACE: u32 = 0x8000_4002;
const DIERR_DEVICENOTREG: u32 = 0x8004_0154;
const DIPROP_BUFFERSIZE: u32 = 1;
const DIDEVTYPE_MOUSE: u32 = 2;
const DIDEVTYPE_KEYBOARD: u32 = 3;
const DIDC_ATTACHED: u32 = 1;

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (d1.to_le_bytes(), d2.to_le_bytes(), d3.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]]
}

const DI_D4: [u8; 8] = [0xbf, 0xc7, 0x44, 0x45, 0x53, 0x54, 0x00, 0x00];
const IID_IUNKNOWN: [u8; 16] = guid(0, 0, 0, [0xc0, 0, 0, 0, 0, 0, 0, 0x46]);
const IID_IDIRECTINPUTA: [u8; 16] = guid(0x8952_1360, 0xaa8a, 0x11cf, DI_D4);
const IID_IDIRECTINPUT2A: [u8; 16] = guid(0x5944_e662, 0xc92e, 0x11cf, DI_D4);
const IID_IDIRECTINPUTDEVICEA: [u8; 16] = guid(0x5944_e680, 0xc92e, 0x11cf, DI_D4);
const IID_IDIRECTINPUTDEVICE2A: [u8; 16] = guid(0x5944_e682, 0xc92e, 0x11cf, DI_D4);
const GUID_SYSMOUSE: [u8; 16] = guid(0x6f1d_2b60, 0xd5a0, 0x11cf, DI_D4);
const GUID_SYSKEYBOARD: [u8; 16] = guid(0x6f1d_2b61, 0xd5a0, 0x11cf, DI_D4);

fn trace() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var_os("NSTRIKE_DI_LOG").is_some_and(|v| v != "0"))
}

macro_rules! di_log {
    ($($t:tt)*) => { if trace() { log!($($t)*) } };
}

// ---- input from the window ----------------------------------------------------------------------

/// Device kinds: index into `INPUT`'s queues.
const KEYBOARD: usize = 0;
const MOUSE: usize = 1;

struct State {
    /// DIK codes: 0x80 while down
    keys: [u8; 256],
    /// mouse motion not yet read by GetDeviceState, wheel, buttons
    dx: i32,
    dy: i32,
    dz: i32,
    buttons: [u8; 4],
    /// buffered events per device: (offset, data, time)
    events: [VecDeque<(u32, u32, u32)>; 2],
    sequence: u32,
}

static INPUT: Mutex<State> = Mutex::new(State {
    keys: [0; 256],
    dx: 0,
    dy: 0,
    dz: 0,
    buttons: [0; 4],
    events: [VecDeque::new(), VecDeque::new()],
    sequence: 0,
});

/// Room in each device's buffer (more events than that are dropped, oldest first)
const BUFFER: usize = 256;

fn push(s: &mut State, dev: usize, ofs: u32, data: u32) {
    let q = &mut s.events[dev];
    if q.len() >= BUFFER {
        q.pop_front();
    }
    q.push_back((ofs, data, super::kernel32::ms_since_start()));
}

/// The DirectInput key code (DIK) of a Windows virtual key and its scan code: the scan code,
/// with 0x80 for the keys of the extended set (arrows, the block above them, keypad /).
pub fn dik(vk: u8, scan: u8) -> u8 {
    match vk {
        0x21..=0x28 | 0x2d | 0x2e | 0x6f => scan | 0x80,
        _ => scan,
    }
}

/// A key went down or up (from `user32::poll`).
pub fn key(dik: u8, down: bool) {
    let mut s = INPUT.lock().unwrap();
    let v = if down { 0x80 } else { 0 };
    if s.keys[dik as usize] != v || down {
        s.keys[dik as usize] = v;
        push(&mut s, KEYBOARD, dik as u32, v as u32);
    }
}

/// Let every key go (the window lost the focus).
pub fn release_keys() {
    let mut s = INPUT.lock().unwrap();
    for k in 0..256 {
        if s.keys[k] != 0 {
            s.keys[k] = 0;
            push(&mut s, KEYBOARD, k as u32, 0);
        }
    }
}

/// A mouse button (0 left, 1 right, 2 middle, 3 X1) went down or up.
pub fn button(b: u8, down: bool) {
    let mut s = INPUT.lock().unwrap();
    let Some(k) = s.buttons.get_mut(b as usize) else { return };
    *k = if down { 0x80 } else { 0 };
    push(&mut s, MOUSE, 12 + b as u32, if down { 0x80 } else { 0 });
}

/// Raw mouse motion in counts (any thread).
pub fn motion(dx: i32, dy: i32) {
    let mut s = INPUT.lock().unwrap();
    s.dx += dx;
    s.dy += dy;
    if dx != 0 {
        push(&mut s, MOUSE, 0, dx as u32);
    }
    if dy != 0 {
        push(&mut s, MOUSE, 4, dy as u32);
    }
}

/// The mouse wheel, in lines.
pub fn wheel(lines: i32) {
    let mut s = INPUT.lock().unwrap();
    s.dz += lines * 120;
    push(&mut s, MOUSE, 8, (lines * 120) as u32);
}

// ---- IDirectInputA ------------------------------------------------------------------------------

fn guid_at(p: u32) -> [u8; 16] {
    mem().slice(p, 16).try_into().unwrap()
}

fn direct_input_create(c: &mut Cpu) -> u32 {
    let (version, out) = (arg(c, 1), arg(c, 2));
    let o = super::com_new(DIRECT_INPUT, 4);
    mem().wr32(o + 4, 1);
    mem().wr32(out, o);
    di_log!("DirectInputCreateA(version {version:#x})");
    DI_OK
}

fn di_query(c: &mut Cpu) -> u32 {
    let (this, iid, out) = (arg(c, 0), guid_at(arg(c, 1)), arg(c, 2));
    if ![IID_IUNKNOWN, IID_IDIRECTINPUTA, IID_IDIRECTINPUT2A].contains(&iid) {
        mem().wr32(out, 0);
        return E_NOINTERFACE;
    }
    add_ref(c);
    mem().wr32(out, this);
    DI_OK
}

/// AddRef and Release of both interfaces: the count is at +4.
fn add_ref(c: &mut Cpu) -> u32 {
    let o = arg(c, 0);
    let n = mem().rd32(o + 4) + 1;
    mem().wr32(o + 4, n);
    n
}

fn release(c: &mut Cpu) -> u32 {
    let o = arg(c, 0);
    let n = mem().rd32(o + 4).saturating_sub(1);
    mem().wr32(o + 4, n);
    if n == 0 {
        free(o);
    }
    n
}

// device object: vtable, references, kind (KEYBOARD/MOUSE), buffered (0/1)
const KIND: u32 = 8;
const BUFFERED: u32 = 0xc;
const DEVICE_SIZE: u32 = 0x10;

fn create_device(c: &mut Cpu) -> u32 {
    let (g, out) = (guid_at(arg(c, 1)), arg(c, 2));
    let kind = match g {
        GUID_SYSKEYBOARD => KEYBOARD,
        GUID_SYSMOUSE => MOUSE,
        _ => {
            di_log!("CreateDevice: unknown device {g:02x?}");
            mem().wr32(out, 0);
            return DIERR_DEVICENOTREG;
        }
    };
    let d = super::com_new(DEVICE, DEVICE_SIZE);
    let m = mem();
    m.wr32(d + 4, 1);
    m.wr32(d + KIND, kind as u32);
    m.wr32(d + BUFFERED, 0);
    m.wr32(out, d);
    di_log!("CreateDevice: {}", ["keyboard", "mouse"][kind]);
    DI_OK
}

/// EnumDevices(type, callback, ref, flags): the keyboard and the mouse (DIDEVICEINSTANCEA of
/// DirectX 5); no joysticks yet.
fn enum_devices(c: &mut Cpu) -> u32 {
    let (kind, cb, user) = (arg(c, 1) & 0xff, arg(c, 2), arg(c, 3));
    di_log!("EnumDevices(type {kind})");
    let ddi = alloc(0x244);
    let m = mem();
    for (g, t, name) in [(GUID_SYSKEYBOARD, DIDEVTYPE_KEYBOARD, "Keyboard"), (GUID_SYSMOUSE, DIDEVTYPE_MOUSE, "Mouse")] {
        if kind != 0 && kind != t {
            continue;
        }
        m.slice(ddi, 0x244).fill(0);
        m.wr32(ddi, 0x244);
        m.slice(ddi + 4, 16).copy_from_slice(&g);
        m.slice(ddi + 0x14, 16).copy_from_slice(&g);
        m.wr32(ddi + 0x24, t);
        super::put_cstr(ddi + 0x28, name, 260);
        super::put_cstr(ddi + 0x12c, name, 260);
        if guest_call(c, cb, &[ddi, user]) == 0 {
            break;
        }
    }
    free(ddi);
    DI_OK
}

fn ok(_: &mut Cpu) -> u32 {
    DI_OK
}

fn not_impl(_: &mut Cpu) -> u32 {
    E_NOTIMPL
}

static DIRECT_INPUT: &[Import] = &[
    ("IDirectInputA::QueryInterface", 3, di_query),
    ("IDirectInputA::AddRef", 1, add_ref),
    ("IDirectInputA::Release", 1, release),
    ("IDirectInputA::CreateDevice", 4, create_device),
    ("IDirectInputA::EnumDevices", 5, enum_devices),
    ("IDirectInputA::GetDeviceStatus", 2, ok),
    ("IDirectInputA::RunControlPanel", 3, ok),
    ("IDirectInputA::Initialize", 3, ok),
    ("IDirectInput2A::FindDevice", 4, not_impl),
];

// ---- IDirectInputDevice2A -----------------------------------------------------------------------

fn kind_of(d: u32) -> usize {
    mem().rd32(d + KIND) as usize
}

fn dev_query(c: &mut Cpu) -> u32 {
    let (this, iid, out) = (arg(c, 0), guid_at(arg(c, 1)), arg(c, 2));
    if ![IID_IUNKNOWN, IID_IDIRECTINPUTDEVICEA, IID_IDIRECTINPUTDEVICE2A].contains(&iid) {
        mem().wr32(out, 0);
        return E_NOINTERFACE;
    }
    add_ref(c);
    mem().wr32(out, this);
    DI_OK
}

/// GetCapabilities(DIDEVCAPS): attached; the keyboard's 256 keys, the mouse's 3 axes and 4
/// buttons.
fn get_capabilities(c: &mut Cpu) -> u32 {
    let (d, p) = (arg(c, 0), arg(c, 1));
    let m = mem();
    let size = m.rd32(p).max(24);
    m.slice(p + 4, size as usize - 4).fill(0);
    let caps = if kind_of(d) == KEYBOARD { [DIDC_ATTACHED, DIDEVTYPE_KEYBOARD, 0, 256, 0] } else { [DIDC_ATTACHED, DIDEVTYPE_MOUSE, 3, 4, 0] };
    for (k, v) in caps.into_iter().enumerate() {
        m.wr32(p + 4 + 4 * k as u32, v);
    }
    DI_OK
}

/// SetProperty: DIPROP_BUFFERSIZE turns buffered data on; others are accepted.
fn set_property(c: &mut Cpu) -> u32 {
    let (d, prop, h) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let v = mem().rd32(h + 0x10);
    di_log!("SetProperty({prop}, {v}) of the {}", ["keyboard", "mouse"][kind_of(d)]);
    if prop == DIPROP_BUFFERSIZE {
        mem().wr32(d + BUFFERED, (v != 0) as u32);
    }
    DI_OK
}

fn get_property(c: &mut Cpu) -> u32 {
    let (prop, h) = (arg(c, 1), arg(c, 2));
    match prop {
        DIPROP_BUFFERSIZE => mem().wr32(h + 0x10, BUFFER as u32),
        _ => return E_NOTIMPL,
    }
    DI_OK
}

fn acquire(c: &mut Cpu) -> u32 {
    let d = arg(c, 0);
    di_log!("Acquire: {}", ["keyboard", "mouse"][kind_of(d)]);
    let mut s = INPUT.lock().unwrap();
    // what happened before belongs to no one
    s.events[kind_of(d)].clear();
    if kind_of(d) == MOUSE {
        (s.dx, s.dy, s.dz) = (0, 0, 0);
    }
    DI_OK
}

/// GetDeviceState: the keyboard's 256 key bytes, or DIMOUSESTATE (motion since the last read,
/// 4 buttons).
fn get_device_state(c: &mut Cpu) -> u32 {
    let (d, size, out) = (arg(c, 0), arg(c, 1) as usize, arg(c, 2));
    let m = mem();
    let mut s = INPUT.lock().unwrap();
    if kind_of(d) == KEYBOARD {
        let n = size.min(256);
        m.slice(out, n).copy_from_slice(&s.keys[..n]);
    } else {
        let mut st = [0u8; 16];
        st[0..4].copy_from_slice(&s.dx.to_le_bytes());
        st[4..8].copy_from_slice(&s.dy.to_le_bytes());
        st[8..12].copy_from_slice(&s.dz.to_le_bytes());
        st[12..16].copy_from_slice(&s.buttons);
        (s.dx, s.dy, s.dz) = (0, 0, 0);
        let n = size.min(16);
        m.slice(out, n).copy_from_slice(&st[..n]);
    }
    DI_OK
}

/// GetDeviceData(object size, DIDEVICEOBJECTDATA[], in/out count, flags): buffered events
/// (offset, data, time, sequence); no array: only removes them (DIGDD_PEEK = 1 keeps them).
fn get_device_data(c: &mut Cpu) -> u32 {
    let (d, size, out, count_p, flags) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
    let m = mem();
    let want = m.rd32(count_p) as usize;
    let mut s = INPUT.lock().unwrap();
    let kind = kind_of(d);
    let n = want.min(s.events[kind].len());
    for k in 0..n {
        let (ofs, data, time) = s.events[kind][k];
        s.sequence += 1;
        if out != 0 {
            let p = out + k as u32 * size.max(16);
            for (j, v) in [ofs, data, time, s.sequence].into_iter().enumerate() {
                m.wr32(p + 4 * j as u32, v);
            }
        }
    }
    if flags & 1 == 0 {
        s.events[kind].drain(..n);
    }
    m.wr32(count_p, n as u32);
    DI_OK
}

static DEVICE: &[Import] = &[
    ("IDirectInputDevice2A::QueryInterface", 3, dev_query),
    ("IDirectInputDevice2A::AddRef", 1, add_ref),
    ("IDirectInputDevice2A::Release", 1, release),
    ("IDirectInputDevice2A::GetCapabilities", 2, get_capabilities),
    ("IDirectInputDevice2A::EnumObjects", 4, ok),
    ("IDirectInputDevice2A::GetProperty", 3, get_property),
    ("IDirectInputDevice2A::SetProperty", 3, set_property),
    ("IDirectInputDevice2A::Acquire", 1, acquire),
    ("IDirectInputDevice2A::Unacquire", 1, ok),
    ("IDirectInputDevice2A::GetDeviceState", 3, get_device_state),
    ("IDirectInputDevice2A::GetDeviceData", 5, get_device_data),
    ("IDirectInputDevice2A::SetDataFormat", 2, ok),
    ("IDirectInputDevice2A::SetEventNotification", 2, ok),
    ("IDirectInputDevice2A::SetCooperativeLevel", 3, ok),
    ("IDirectInputDevice2A::GetObjectInfo", 4, not_impl),
    ("IDirectInputDevice2A::GetDeviceInfo", 2, not_impl),
    ("IDirectInputDevice2A::RunControlPanel", 3, ok),
    ("IDirectInputDevice2A::Initialize", 4, ok),
    ("IDirectInputDevice2A::CreateEffect", 5, not_impl),
    ("IDirectInputDevice2A::EnumEffects", 4, ok),
    ("IDirectInputDevice2A::GetEffectInfo", 3, not_impl),
    ("IDirectInputDevice2A::GetForceFeedbackState", 2, not_impl),
    ("IDirectInputDevice2A::SendForceFeedbackCommand", 2, not_impl),
    ("IDirectInputDevice2A::EnumCreatedEffectObjects", 4, ok),
    ("IDirectInputDevice2A::Escape", 2, not_impl),
    ("IDirectInputDevice2A::Poll", 1, ok),
    ("IDirectInputDevice2A::SendDeviceData", 5, not_impl),
];

pub const IMPORTS: &[Import] = &[("DirectInputCreateA", 4, direct_input_create)];
