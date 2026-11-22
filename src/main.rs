//! Nuclear Strike recompiled: compiles the player's NUKEPC.EXE and runs it.
//!
//!     nstrike [path to NUKEPC.EXE]
//!
//! Without a path: the game in `game_files` (installed there from a CD image first: `setup`).
//!
//! A release build is a Windows program without a console window: its messages go to the
//! console it was started from, else to nstrike.log next to nstrike.exe.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use nstrike::discover::{Code, Program, TEXT_END};
use nstrike::image::Image;
use nstrike::jit::compile_cached;
use nstrike::setup;
use nstrike::smc::Smc;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Without a console window of our own: messages to the console we were started from (or
/// wherever they were redirected), else to nstrike.log next to nstrike.exe.
#[cfg(windows)]
fn output() {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::*;
    unsafe {
        if !GetStdHandle(STD_ERROR_HANDLE).is_null() {
            return;
        }
        let out = if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            std::fs::OpenOptions::new().write(true).open("CONOUT$").ok()
        } else {
            let log = std::env::current_exe().ok().and_then(|p| Some(p.parent()?.join("nstrike.log")));
            log.and_then(|p| std::fs::File::create(p).ok())
        };
        if let Some(f) = out {
            let h = f.into_raw_handle();
            SetStdHandle(STD_OUTPUT_HANDLE, h as _);
            SetStdHandle(STD_ERROR_HANDLE, h as _);
        }
    }
}

#[cfg(not(windows))]
fn output() {}

/// What the player must know before the game can start (no game found, wrong version): in a
/// message box too, since a release build has no console window.
fn fail(text: &str) -> ! {
    eprintln!("{text}");
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide("Nuclear Strike").as_ptr(), MB_OK | MB_ICONERROR);
    }
    std::process::exit(2)
}

fn main() {
    output();
    nstrike::platform::reserve_memory();
    // guest calls nest on the host stack like the original's: run on a thread with a large one
    let code = std::thread::Builder::new().stack_size(256 << 20).spawn(run).unwrap().join().unwrap();
    nstrike::platform::kernel32::terminate(code);
}

fn run() -> u32 {
    let found = match std::env::args().nth(1) {
        Some(p) => setup::check(Path::new(&p)).map(|_| PathBuf::from(p)),
        None => setup::game_exe(&setup::root()),
    };
    let exe = found.unwrap_or_else(|text| fail(&text));
    let mut img = Image::load(&exe).unwrap_or_else(|e| panic!("{}: {e}", exe.display()));
    for &(addr, old, new) in nstrike::platform::PATCHES.iter().copied().flatten() {
        img.patch(addr, old, new).unwrap_or_else(|e| panic!("{e}"));
    }
    nstrike::platform::gamepad::start(); // gamepads are found while we compile
    let t = Instant::now();
    let ranges = nstrike::cv::functions(&nstrike::cv::publics(&img.data), TEXT_END);
    let mut code = Code::new(&img);
    let prog = Program::build(&mut code, &ranges);
    let smc = Smc::find(&prog, &mut code);
    let found = t.elapsed().as_millis();
    let cache = exe.with_file_name(setup::COMPILED);
    let key = cache_key(&img);
    let compiled = compile_cached(&prog, &smc, &mut code, nstrike::platform::HOOKS, key.map(|k| (cache.as_path(), k)));
    let st = &compiled.stats;
    if st.cached {
        eprintln!("compiled code of {} functions loaded in {} ms", st.functions, t.elapsed().as_millis());
    } else {
        eprintln!(
            "compiled {} functions ({} named) in {} ms (finding them {found}, lifting {}, compiling {}); {} complete, {} instructions",
            st.functions,
            ranges.len(),
            t.elapsed().as_millis(),
            st.lift_ms,
            st.compile_ms,
            st.complete,
            st.instructions
        );
        if !st.missing.is_empty() {
            eprintln!("not supported: {:?}", st.missing);
        }
        for (a, e) in &st.failed {
            eprintln!("failed: {} {e}", nstrike::image::named(*a));
        }
    }
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let code = nstrike::platform::run_game(&img, dir);
    drop(compiled);
    code
}

/// What the compiled code depends on: this program (any change to the recompiler) and the
/// game's EXE. None when our own file cannot be read.
fn cache_key(img: &Image) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let own = std::fs::read(std::env::current_exe().ok()?).ok()?;
    let mut h = Sha256::new();
    h.update(&own);
    h.update(&img.data);
    // the memory's base, a constant in the code when fixed
    h.update(nstrike::lift::FIXED_BASE.load(std::sync::atomic::Ordering::Relaxed).to_le_bytes());
    Some(h.finalize().into())
}
