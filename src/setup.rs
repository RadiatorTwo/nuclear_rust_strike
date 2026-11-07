//! The player's game files: the folder `game_files` next to nstrike.exe (in a checkout: in the
//! current folder). Without an installed game there, it is installed from an image of the CD
//! (.iso or .bin, in `game_files`, next to nstrike.exe or in the current folder) as the CD's
//! installer would ("Full Install"): the files in `setup.esa` (`esa`) by their group, the
//! worlds from the CD, and the CD's videos beside them:
//!
//! ```text
//! game_files/NUKEPC/          NUKEPC.EXE
//! game_files/NUKEPC/data/     the game's data, nukepc.sav; Fonts, FrontEnd, Hud, MsgBox,
//!                             SMFD, Worlds
//! game_files/NUKEPC/video/    videos.res (the installed game reads it from the CD)
//! ```

use crate::iso::Iso;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const GAME: &str = "NUKEPC";
pub const EXE: &str = "NUKEPC.EXE";
/// the game's compiled code, kept for the next start (`jit::compile_cached`)
pub const COMPILED: &str = "compiled.bin";

/// SHA-256 of the one NUKEPC.EXE we know (US, 1997-10-04).
pub const EXE_SHA256: &str = "af30847879a10b26cea4e9ba28d673446534a9430bae3530b576b968c826daad";

/// Which group of `setup.esa` goes to which folder of the game (FINSTALL.SSF).
const GROUPS: [(&str, &str); 8] = [
    ("STRIKE_EXE_FILES", ""),
    ("STRIKE_FILES", "data"),
    ("FULL_SAV_FILE", "data"),
    ("STRIKE_HUD", "data/Hud"),
    ("STRIKE_SMFD", "data/SMFD"),
    ("STRIKE_FONTS", "data/Fonts"),
    ("STRIKE_FE", "data/FrontEnd"),
    ("STRIKE_MSGBOX", "data/MsgBox"),
];
/// of STRIKE_EXE_FILES only the EXE: Miles (`mss32.dll`) and the VxD are ours
const LEFT_OUT: [&str; 2] = ["MSS32.DLL", "MEMVXD.VXD"];

/// The folder with the game files: `game_files` next to nstrike.exe, else in the current folder.
pub fn root() -> PathBuf {
    match home().map(|d| d.join("game_files")) {
        Some(d) if d.is_dir() || !Path::new("game_files").is_dir() => d,
        _ => PathBuf::from("game_files"),
    }
}

fn home() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// NUKEPC.EXE in `root`, installed from a CD image first if needed; or what the player must do.
pub fn game_exe(root: &Path) -> Result<PathBuf, String> {
    let exe = root.join(GAME).join(EXE);
    if !exe.is_file() {
        let image = [root.to_path_buf(), home().unwrap_or_default(), PathBuf::from(".")]
            .iter()
            .find_map(|d| find_image(d))
            .ok_or_else(|| {
                format!(
                    "Put an image of your Nuclear Strike CD (.iso or .bin) next to nstrike.exe or into this folder:\n\n{}\n\nThen start nstrike.exe again.",
                    root.display()
                )
            })?;
        install(&image, &root.join(GAME))?;
    }
    check(&exe)?;
    Ok(exe)
}

/// Is this the NUKEPC.EXE we know?
pub fn check(exe: &Path) -> Result<(), String> {
    let data = std::fs::read(exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let hash: String = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    if hash != EXE_SHA256 {
        return Err(format!("{} is not the version we know (SHA-256 {hash}).", exe.display()));
    }
    Ok(())
}

/// A CD image in `dir` (not below it): one Iso can open.
fn find_image(dir: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    found.sort();
    found.into_iter().find(|p| {
        let ext = p.extension().map(|e| e.to_ascii_lowercase());
        matches!(ext.as_ref().and_then(|e| e.to_str()), Some("iso" | "bin" | "img")) && Iso::open(p).is_some()
    })
}

/// Install the game from a CD image into `game`.
pub fn install(image: &Path, game: &Path) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", image.display());
    let mut iso = Iso::open(image).ok_or_else(|| format!("{} is not a CD image", image.display()))?;
    let root = iso.root();
    let esa = iso.get(&root, "setup.esa").ok_or_else(|| format!("{}: no setup.esa: not a Nuclear Strike CD", image.display()))?;
    eprintln!("installing from {}", image.display());
    let mut archive = Vec::new();
    iso.read(&esa, &mut archive).map_err(err)?;
    let entries = crate::esa::entries(&archive).ok_or("setup.esa: not an EA archive")?;
    for e in &entries {
        let Some((_, to)) = GROUPS.iter().find(|g| g.0 == e.group) else { continue };
        if LEFT_OUT.iter().any(|n| n.eq_ignore_ascii_case(&e.name)) {
            continue;
        }
        let stored = archive.get(e.offset as usize..(e.offset + e.stored) as usize).ok_or("setup.esa ends early")?;
        let dir = game.join(to);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::write(dir.join(&e.name), e.unpack(stored)?).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    for (from, to) in [("worlds", "data/Worlds"), ("video", "video")] {
        let d = iso.get(&root, from).ok_or_else(|| format!("{}: no {from} folder", image.display()))?;
        iso.extract(&d, &game.join(to)).map_err(err)?;
    }
    Ok(())
}
