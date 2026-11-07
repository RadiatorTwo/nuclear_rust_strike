//! Nuclear Strike recompiled: compiles the player's NUKEPC.EXE and runs it.
//!
//!     nstrike [path to NUKEPC.EXE]
//!
//! Without a path: the game in `game_files` (installed there from a CD image first: `setup`).

use nstrike::discover::{Code, Program, TEXT_END};
use nstrike::image::Image;
use nstrike::jit::compile_cached;
use nstrike::setup;
use nstrike::smc::Smc;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    // guest calls nest on the host stack like the original's: run on a thread with a large one
    let code = std::thread::Builder::new().stack_size(256 << 20).spawn(run).unwrap().join().unwrap();
    nstrike::platform::kernel32::terminate(code);
}

fn run() -> u32 {
    let found = match std::env::args().nth(1) {
        Some(p) => setup::check(Path::new(&p)).map(|_| PathBuf::from(p)),
        None => setup::game_exe(&setup::root()),
    };
    let exe = found.unwrap_or_else(|text| {
        eprintln!("{text}");
        std::process::exit(2)
    });
    let img = Image::load(&exe).unwrap_or_else(|e| panic!("{}: {e}", exe.display()));
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
    Some(h.finalize().into())
}
