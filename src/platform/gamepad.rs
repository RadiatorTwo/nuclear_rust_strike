//! Gamepads (through gilrs), our own layer beside the keyboard, as future_rust_cop's. The game's
//! DirectInput joystick knew X/Y and buttons only and needed its Controller menu; here every
//! pad control (buttons, triggers, D-pad, both sticks' directions) can be bound to any action
//! (`gamepad.toml` in the game files folder, written with the defaults at the first start).
//!
//! The game's input (Controller_*): a thread reads the devices every 5 ms into a record (the
//! DirectInput keyboard's 256 keys among them), Controller_ProcessInput_ 0x446a6c (once a
//! frame; also the front end's and message boxes' loops) runs the attached contexts' criteria
//! over it: they set state variables (8 bytes: i16 value, i16 full scale, ...) and post
//! commands. The contexts are a list at controller +0xf5, the newest first (+0 next, the last
//! node has none); the mission's are [0x51e3f0] / [0x51e3f4], the global one (F-keys)
//! [0x51e3e0], the front end's [0x51e3e4], a message box's [0x51e404].
//!
//! - Digital actions (weapons, HUD, SMFD, ...) press the key the game's keyboard binding has
//!   for the command (its table [0x4e8608]: 11-byte entries, u16 command, u16 key), held in
//!   DirectInput's keyboard state (`dinput::pad_keys`): the game's own press, repeat and
//!   release logic does the rest.
//! - Movement is analog: each direction is a state variable 0..255 (forward 0x51e43c, reverse
//!   0x51e454, turn left 0x51e46c, right 0x51e434, jink left 0x51e45c, right 0x51e464); the
//!   chopper's motion 0x41da50 scales by value / full scale (+2). After Controller_ProcessInput_
//!   (hooked) our amount goes in where it is larger than the keyboard's.
//! - Outside flying (the newest context other than the global one is not the mission's: front
//!   end, SMFD, message boxes, the Esc menu) the pad is fixed menu keys: D-pad and left stick
//!   the arrows, A Enter, B and Start Escape, X Tab.
//!
//! gilrs lives on the main thread and is polled at every `window::pump`. `NSTRIKE_PAD_LOG=1`.

use super::{dinput, log, mem};
use crate::rt::{Cpu, EAX, call_original};
use gilrs::{Axis, Button, Gilrs};
use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

fn trace() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var_os("NSTRIKE_PAD_LOG").is_some_and(|v| v != "0"))
}

macro_rules! pad_log {
    ($($t:tt)*) => { if trace() { log!("pad: {}", format!($($t)*)) } };
}

// ---- controls -----------------------------------------------------------------------------------

/// A pad's control: buttons, triggers, the D-pad, each stick's four directions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Control {
    A,
    B,
    X,
    Y,
    LB,
    RB,
    LT,
    RT,
    Back,
    Start,
    L3,
    R3,
    DUp,
    DDown,
    DLeft,
    DRight,
    LUp,
    LDown,
    LLeft,
    LRight,
    RUp,
    RDown,
    RLeft,
    RRight,
}

use Control::*;

pub const CONTROLS: [Control; 24] =
    [A, B, X, Y, LB, RB, LT, RT, Back, Start, L3, R3, DUp, DDown, DLeft, DRight, LUp, LDown, LLeft, LRight, RUp, RDown, RLeft, RRight];
const NAMES: [&str; 24] = [
    "A", "B", "X", "Y", "LB", "RB", "LT", "RT", "Back", "Start", "L3", "R3", "DUp", "DDown", "DLeft", "DRight", "LUp", "LDown",
    "LLeft", "LRight", "RUp", "RDown", "RLeft", "RRight",
];

impl Control {
    pub fn name(self) -> &'static str {
        NAMES[self as usize]
    }

    fn parse(s: &str) -> Option<Control> {
        NAMES.iter().position(|n| n.eq_ignore_ascii_case(s)).map(|k| CONTROLS[k])
    }
}

/// How far each control is pushed, 0...1 (buttons 0 or 1).
type Amounts = [f32; 24];

/// A control counts as pressed (for digital actions) from here.
const PRESS: f32 = 0.4;
/// The sticks' dead zone (radial, of the full travel).
const DEAD: f32 = 0.18;

// ---- actions ------------------------------------------------------------------------------------

enum Kind {
    /// a movement state variable (0...255)
    Analog(u32),
    /// a command: the key the game's keyboard binding has for it, else this DIK code
    Key(u16, u8),
}

/// What a pad control can do in flight. `name` is the file's key, `label` the menu's.
pub struct Action {
    pub name: &'static str,
    pub label: &'static str,
    kind: Kind,
    default: &'static [Control],
}

/// The actions, the defaults a modern twin-stick layout: the left stick flies (forward,
/// reverse, jink), the right stick turns, the triggers aim and fire.
pub const ACTIONS: [Action; 18] = [
    Action { name: "forward", label: "Forward", kind: Kind::Analog(0x51e43c), default: &[LUp, DUp] },
    Action { name: "reverse", label: "Reverse", kind: Kind::Analog(0x51e454), default: &[LDown, DDown] },
    Action { name: "turn_left", label: "Turn left", kind: Kind::Analog(0x51e46c), default: &[RLeft, DLeft] },
    Action { name: "turn_right", label: "Turn right", kind: Kind::Analog(0x51e434), default: &[RRight, DRight] },
    Action { name: "jink_left", label: "Jink left", kind: Kind::Analog(0x51e45c), default: &[LLeft] },
    Action { name: "jink_right", label: "Jink right", kind: Kind::Analog(0x51e464), default: &[LRight] },
    Action { name: "chaingun", label: "Chain gun", kind: Kind::Key(0x08, 0x39), default: &[RT] },
    Action { name: "hydra", label: "Hydras", kind: Kind::Key(0x07, 0x2d), default: &[RB] },
    Action { name: "hellfire", label: "Hellfires", kind: Kind::Key(0x09, 0x2c), default: &[LB] },
    Action { name: "wingtip", label: "Wingtips", kind: Kind::Key(0x0a, 0x2e), default: &[B] },
    Action { name: "set_target", label: "Set target", kind: Kind::Key(0x10, 0x2a), default: &[LT] },
    Action { name: "cargo", label: "Cargo / rescue", kind: Kind::Key(0x12, 0x20), default: &[A] },
    Action { name: "intel", label: "Map / intel", kind: Kind::Key(0x2c, 0x1c), default: &[Back] },
    Action { name: "hud", label: "HUD", kind: Kind::Key(0x17, 0x23), default: &[R3] },
    Action { name: "camera", label: "Camera", kind: Kind::Key(0x16, 0x9c), default: &[Y] },
    Action { name: "radar_zoom", label: "Radar zoom", kind: Kind::Key(0x1a, 0x36), default: &[X] },
    Action { name: "pause", label: "Pause", kind: Kind::Key(0x1b, 0x19), default: &[L3] },
    Action { name: "menu", label: "Menu", kind: Kind::Key(0x15, 0x01), default: &[Start] },
];

/// Menu keys (DIK) outside flying, fixed.
const MENU_KEYS: [(&[Control], u8); 7] = [
    (&[DUp, LUp], 0xc8),
    (&[DDown, LDown], 0xd0),
    (&[DLeft, LLeft], 0xcb),
    (&[DRight, LRight], 0xcd),
    (&[A], 0x1c),
    (&[B, Start], 0x01),
    (&[X], 0x0f),
];

// ---- bindings -----------------------------------------------------------------------------------

/// Each action's controls (`ACTIONS` order); None until loaded.
static BINDINGS: Mutex<Option<Vec<Vec<Control>>>> = Mutex::new(None);

fn file() -> std::path::PathBuf {
    crate::setup::root().join("gamepad.toml")
}

fn defaults() -> Vec<Vec<Control>> {
    ACTIONS.iter().map(|a| a.default.to_vec()).collect()
}

/// The bindings in the file's text (`action = control, control`; `#` a comment). Unknown names
/// are reported through `warn`; an action not in the text keeps its default.
pub fn parse(text: &str, mut warn: impl FnMut(String)) -> Vec<Vec<Control>> {
    let mut b = defaults();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            warn(format!("not `action = controls`: {line}"));
            continue;
        };
        let Some(k) = ACTIONS.iter().position(|a| a.name == key.trim()) else {
            warn(format!("unknown action {}", key.trim()));
            continue;
        };
        b[k] = value
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .filter_map(|c| Control::parse(c).or_else(|| (warn(format!("unknown control {c}")), None).1))
            .collect();
    }
    b
}

pub fn render(b: &[Vec<Control>]) -> String {
    let mut s = String::from(
        "# Gamepad bindings of the Nuclear Strike port: action = controls (comma-separated, none to\n\
         # unbind). Controls: A B X Y LB RB LT RT Back Start L3 R3, the D-pad DUp DDown DLeft DRight,\n\
         # the sticks' directions LUp LDown LLeft LRight RUp RDown RLeft RRight. Movement follows\n\
         # how far a stick or trigger is pushed. Outside flying the pad works menus (fixed).\n",
    );
    for (a, c) in ACTIONS.iter().zip(b) {
        let c: Vec<&str> = c.iter().map(|c| c.name()).collect();
        s += &format!("{} = {}\n", a.name, c.join(", "));
    }
    s
}

fn with_bindings<R>(f: impl FnOnce(&mut Vec<Vec<Control>>) -> R) -> R {
    let mut g = BINDINGS.lock().unwrap();
    let b = g.get_or_insert_with(|| match std::fs::read_to_string(file()) {
        Ok(text) => parse(&text, |w| log!("warn: gamepad.toml: {w}")),
        Err(_) => {
            let b = defaults();
            let _ = std::fs::write(file(), render(&b));
            b
        }
    });
    f(b)
}

/// The controls bound to action `k`.
pub fn binding(k: usize) -> Vec<Control> {
    with_bindings(|b| b[k].clone())
}

fn save(b: &[Vec<Control>]) {
    if let Err(e) = std::fs::write(file(), render(b)) {
        log!("warn: gamepad.toml not written: {e}");
    }
}

/// Bind `c` to action `k` (taken from any other action: a control does one thing), or unbind
/// it there if it was; saved.
pub fn toggle(k: usize, c: Control) {
    with_bindings(|b| {
        if let Some(i) = b[k].iter().position(|&x| x == c) {
            b[k].remove(i);
        } else {
            for v in b.iter_mut() {
                v.retain(|&x| x != c);
            }
            b[k].push(c);
        }
        save(b);
    });
}

/// Unbind action `k`; saved.
pub fn clear(k: usize) {
    with_bindings(|b| {
        b[k].clear();
        save(b);
    });
}

/// The default bindings again; saved.
pub fn reset() {
    with_bindings(|b| {
        *b = defaults();
        save(b);
    });
}

// ---- reading the pad ----------------------------------------------------------------------------

struct State {
    amounts: Amounts,
    /// each pad's triggers' rest (pad, left): the lowest value seen. Some pads (a
    /// "HID-konformer Gamecontroller" in future_rust_cop) rest at ~0.45.
    rest: Vec<((usize, bool), f32)>,
}

static STATE: Mutex<State> = Mutex::new(State { amounts: [0.0; 24], rest: Vec::new() });

thread_local! {
    /// gilrs on the main thread; None when it could not start (then no pads)
    /// (never dropped: its thread must not end at exit)
    static GILRS: RefCell<Option<Option<&'static mut Gilrs>>> = const { RefCell::new(None) };
}

fn new_gilrs() -> Option<&'static mut Gilrs> {
    let g = Gilrs::new().map_err(|e| log!("warn: no gamepads: {e}")).ok()?;
    Some(Box::leak(Box::new(g)))
}

/// Start gilrs on the main thread before the game (pads are found some time after), read the
/// bindings (the file is written with the defaults if there is none).
pub fn start() {
    with_bindings(|_| ());
    GILRS.with(|g| {
        g.borrow_mut().get_or_insert_with(new_gilrs);
    });
}

/// A trigger's travel beyond its rest, 0...1.
fn trigger(rest: &mut Vec<((usize, bool), f32)>, key: (usize, bool), v: f32) -> f32 {
    let r = match rest.iter_mut().find(|e| e.0 == key) {
        Some(e) => {
            e.1 = e.1.min(v);
            e.1
        }
        None => {
            rest.push((key, v));
            v
        }
    };
    if r >= 1.0 { 0.0 } else { ((v - r) / (1.0 - r)).clamp(0.0, 1.0) }
}

/// A stick (x right, y up) as its four directions' amounts (up, down, left, right), with a
/// radial dead zone.
fn stick(x: f32, y: f32) -> [f32; 4] {
    let m = x.hypot(y);
    if m <= DEAD {
        return [0.0; 4];
    }
    let k = ((m - DEAD) / (1.0 - DEAD)).min(1.0) / m;
    let (x, y) = (x * k, y * k);
    [y.max(0.0), (-y).max(0.0), (-x).max(0.0), x.max(0.0)]
}

fn read(g: &gilrs::Gamepad, rest: &mut Vec<((usize, bool), f32)>) -> Amounts {
    let id: usize = g.id().into();
    let mut a = [0.0; 24];
    let buttons = [
        (A, Button::South),
        (B, Button::East),
        (X, Button::West),
        (Y, Button::North),
        (LB, Button::LeftTrigger),
        (RB, Button::RightTrigger),
        (Back, Button::Select),
        (Start, Button::Start),
        (L3, Button::LeftThumb),
        (R3, Button::RightThumb),
        (DUp, Button::DPadUp),
        (DDown, Button::DPadDown),
        (DLeft, Button::DPadLeft),
        (DRight, Button::DPadRight),
    ];
    for (c, b) in buttons {
        a[c as usize] = g.is_pressed(b) as u8 as f32;
    }
    for (c, b, left) in [(LT, Button::LeftTrigger2, true), (RT, Button::RightTrigger2, false)] {
        a[c as usize] = g.button_data(b).map_or(0.0, |d| trigger(rest, (id, left), d.value()));
    }
    let l = stick(g.value(Axis::LeftStickX), g.value(Axis::LeftStickY));
    let r = stick(g.value(Axis::RightStickX), g.value(Axis::RightStickY));
    for (k, c) in [LUp, LDown, LLeft, LRight].into_iter().enumerate() {
        a[c as usize] = l[k];
    }
    for (k, c) in [RUp, RDown, RLeft, RRight].into_iter().enumerate() {
        a[c as usize] = r[k];
    }
    a
}

/// Whether the pad flies the chopper (`process_input` finds the mission's context in front).
static FLYING: AtomicBool = AtomicBool::new(false);

fn amount(a: &Amounts, controls: &[Control]) -> f32 {
    controls.iter().fold(0.0, |m, &c| m.max(a[c as usize]))
}

/// The DIK key the game's keyboard binding has for `command` (else `default`).
fn key_of(command: u16, default: u8) -> u8 {
    let m = mem();
    let table = m.rd32(0x4e8608);
    let table = if table == 0 { 0x4e8628 } else { table };
    let n = m.rd8(table + 1) as u32;
    (0..n)
        .map(|k| table + 2 + 11 * k)
        .find(|&e| m.rd16(e) == command)
        .map(|e| m.rd16(e + 2))
        .filter(|&k| k < 0x100)
        .map_or(default, |k| k as u8)
}

/// Read the pads and hold the keys they press (main thread, at every pump). Returns the
/// controls pressed since the last poll; with `menu` (our menu open) no key is held.
pub fn poll(menu: bool) -> Vec<Control> {
    let mut s = STATE.lock().unwrap();
    let mut pressed = Vec::new();
    let mut a = [0.0; 24];
    GILRS.with(|g| {
        let mut g = g.borrow_mut();
        let Some(g) = g.get_or_insert_with(new_gilrs) else { return };
        while let Some(e) = g.next_event() {
            match e.event {
                gilrs::EventType::Connected => pad_log!("connected: {}", g.gamepad(e.id).name()),
                gilrs::EventType::Disconnected => pad_log!("disconnected: {}", g.gamepad(e.id).name()),
                _ => {}
            }
        }
        if let Some((_, p)) = g.gamepads().next() {
            a = read(&p, &mut s.rest);
        }
    });
    for c in CONTROLS {
        let (was, now) = (s.amounts[c as usize] >= PRESS, a[c as usize] >= PRESS);
        if now && !was {
            pressed.push(c);
            pad_log!("{} pressed", c.name());
        }
    }
    s.amounts = a;
    drop(s);
    let mut keys = [0u8; 256];
    if menu {
        // nothing for the game
    } else if FLYING.load(Ordering::Relaxed) {
        with_bindings(|b| {
            for (act, controls) in ACTIONS.iter().zip(b.iter()) {
                if let Kind::Key(command, default) = act.kind {
                    if amount(&a, controls) >= PRESS {
                        keys[key_of(command, default) as usize] = 0x80;
                    }
                }
            }
        });
    } else {
        for (controls, key) in MENU_KEYS {
            if amount(&a, controls) >= PRESS {
                keys[key as usize] = 0x80;
            }
        }
    }
    dinput::pad_keys(&keys);
    pressed
}

// ---- the game's controller ----------------------------------------------------------------------

/// Controller_ProcessInput_ (eax controller, edx: post commands).
pub const PROCESS_INPUT: u32 = 0x446a6c;
/// the game's controller, the contexts' pointers
const CONTROLLER: u32 = 0x51e408;
const GLOBAL: u32 = 0x51e3e0;
const MISSION: [u32; 2] = [0x51e3f0, 0x51e3f4];

pub fn process_input(c: &mut Cpu) {
    let ctrl = c.r[EAX];
    call_original(c, PROCESS_INPUT);
    let m = mem();
    if ctrl == 0 || ctrl != m.rd32(CONTROLLER) {
        return;
    }
    // the newest context but the global one
    let global = m.rd32(GLOBAL);
    let mut n = m.rd32(ctrl + 0xf5);
    let mut front = 0;
    while n != 0 && m.rd32(n) != 0 {
        if n != global {
            front = n;
            break;
        }
        n = m.rd32(n);
    }
    let flying = front != 0 && MISSION.iter().any(|&v| m.rd32(v) == front);
    if FLYING.swap(flying, Ordering::Relaxed) != flying {
        pad_log!("{}", if flying { "flying" } else { "menus" });
    }
    if !flying {
        return;
    }
    let a = STATE.lock().unwrap().amounts;
    with_bindings(|b| {
        for (act, controls) in ACTIONS.iter().zip(b.iter()) {
            if let Kind::Analog(var) = act.kind {
                let v = (amount(&a, controls) * 255.0).round() as i16;
                if v > m.rd16(var) as i16 {
                    m.wr16(var, v as u16);
                    m.wr16(var + 2, 255);
                }
            }
        }
    });
}
