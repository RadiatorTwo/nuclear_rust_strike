# CLAUDE.md — Nuclear Strike in Rust

Hand-over for future sessions. What is done and what is open: **PLAN.md** (keep it current).

## Goal (the user's)

A native port of Nuclear Strike (EA, 1997, PC version of the PlayStation game, with 3Dfx Glide
support) in Rust, in the style of `C:\!Code2\future_rust_cop` (Future Cop: L.A.P.D.): the
game's own EXE recompiled with Cranelift, Windows/DirectX/Glide/Miles implemented in Rust,
Glide drawn with OpenGL. The original hardly runs on modern Windows.

Only code is ever shared; players supply their own game files. **Never commit anything
derived from the EXE or the CD** (translated code, extracted data). `game_files/` and CD
images are git-ignored.

## The user

- New to reverse engineering; explain in plain words, give a recommendation, not a survey.
- Speaks German; works in short sessions and watches usage limits.
- Confirm before anything outward-facing or destructive (pushing, deleting).
- Prefers clean Rust over generated C; performance comes later.
- Never add Co-Authored-By or other AI attribution to commits.

## Relation to future_rust_cop

Decided 2026-11-07: shared code is **copied** from future_rust_cop and adapted (no shared
crate). When porting more of it (platform layer: user32, gdi32, ddraw, dinput, dsound,
window, gl, glide), copy the file, strip Future Cop's hooks and addresses, and keep its
design. Its CLAUDE.md explains each copied module in detail.

Copied so far: `rt.rs`, `lift.rs`, `lift/x87.rs`, `jit.rs`, `discover.rs`, `smc.rs`,
`image.rs`, `iso.rs`, `platform/mod.rs` (core only), `platform/kernel32.rs`,
`platform/advapi32.rs`, `examples/dis.rs`, `tests/` (x87, smc).

## The game

- CD (`Nuclear Strike (USA).bin`, Mode1/2352, volume `NUKEPC`): `setup.esa` (EA installer
  archive: NUKEPC.EXE, MSS32.DLL, MEMVXD.VXD and the `data` folder), `worlds/` (7 levels:
  .bin .bnt .hmp .pal .res .sap .snd .tpc), `video/videos.res` (355 MB), `Glide/`, `directx/`.
- `NUKEPC.EXE` (US, 1 134 940 bytes, SHA-256 `af308478...26daad`): built with **Watcom C**
  (sections BEGTEXT 0x410000, DGROUP 0x4d0000, .bss 0x500000; all virtual sizes 0: `image.rs`
  takes up to the next section). Entry `_wstart2_` 0x4a841e. Watcom's register calling
  convention (eax, edx, ebx, ecx; names end in `_`): matters for our hooks only.
- It carries **CodeView 5 debug info (NB11)** at the end of the file: 2164 named code addresses,
  ~940 data names. `src/cv.rs` reads the publics; they give discovery its function ranges
  (no Ghidra export needed) and `image::symbols()` its names. `data/symbols.txt` is only for
  what has no name there.
- Imports: kernel32, user32, advapi32, winmm (timeGetTime), DirectDraw, DirectInput,
  DirectSound, **Miles Sound System** (`mss32.dll`, 36 `_AIL_*@n` functions: samples, MIDI
  out, timers; music is the game's own sequencer sending MIDI, `AUD_IMS1.SF2` a SoundFont),
  `DeviceIoControl` on `\\.\MEMVXD.VXD` (a Win95 driver: to be faked). Glide 2.x is loaded
  with LoadLibraryA("glide2x.dll") + GetProcAddress of `_grX@n` names, as in Future Cop.
- Install layout (FINSTALL.SSF): NUKEPC.EXE in the folder, groups STRIKE_FILES -> data,
  STRIKE_HUD/SMFD/FONTS/FE/MSGBOX -> data\Hud, SMFD, Fonts, FrontEnd, MsgBox; FULL_SAV_FILE
  (nukepc.sav) -> data; CD worlds -> data\Worlds. Videos stay on the CD (game: `gRootCD`).

## How it works (new here; see future_rust_cop's CLAUDE.md for the copied modules)

- `src/esa.rs`: EA archive entries (name, group, size, method PKWA/NULL, stored size,
  offset) and `explode` (PKWARE DCL, after zlib's blast.c).
- `src/setup.rs`: `game_files/NUKEPC/` (next to nstrike.exe, else in the current folder);
  installs it from a CD image (in game_files, next to nstrike.exe, or the current folder) as
  the installer would, plus `video/`; checks the EXE's SHA-256.
- `src/cv.rs`: CodeView publics; `functions()` = ranges from one public to the next in
  BEGTEXT (the DirectInput data formats `_c_dfDI*` at its end are data).
- `discover.rs`: TEXT 0x410000..0x4c0f58; data pointers scanned in DGROUP at **2-byte
  steps** (Watcom's XI/YI start-up tables have 6-byte entries).
- `lift.rs`: segment registers in mov/push/pop read as flat selectors (CS 0x1b, FS 0x3b,
  GS 0, else 0x23), writes are dropped (Watcom's start-up, `push ds; pop es`).
- `src/main.rs`: `nstrike [NUKEPC.EXE]`; compiles (cached in `compiled.bin` beside the EXE)
  and runs it; imports without implementation stop with "import X is not implemented yet".
- `cargo run --release --example dis -- game_files/NUKEPC/NUKEPC.EXE <addr|all>` disassembles.
- Environment variables are `NSTRIKE_*` (Future Cop's `FCOP_*`): `NSTRIKE_TRACE_API`,
  `NSTRIKE_PROF`, `NSTRIKE_WATCH`, `NSTRIKE_COUNT`, `NSTRIKE_TRACE_REG`.

## Commands

- `cargo build --release`, `cargo test --release` (no game files needed).
- `./target/release/nstrike.exe` from the repository: installs into `game_files/` from the
  CD image there, compiles (~1.4 s, 3081 functions) and runs.
