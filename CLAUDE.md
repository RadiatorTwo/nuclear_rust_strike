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
`image.rs`, `iso.rs`, `platform/mod.rs` (core only), `kernel32`, `advapi32`, `user32`,
`gdi32`, `ddraw`, `dsound`, `misc`, `window`, `gl`, `examples/dis.rs`, `tests/` (x87, smc).
Left out: FCop's dialogs (`dialog/`), gamepads, cheats, recording, display settings.

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
- `discover.rs` also takes 32-bit immediates pointing at not yet decoded code as entries
  (`push offset thread_proc`), and `jmp cs:[reg + table]` (reg = index*4) as jump tables.
- `image.rs`: a section with raw offset 0 (Watcom's .bss) is uninitialised: zeros.
- `platform/mod.rs`: the game sees itself at `C:\NUKEPC\NUKEPC.EXE` (the command line too:
  it takes its folder from it) and the CD as D: (`D:\worlds` = `data\Worlds`);
  SetCurrentDirectoryA is followed (`guest_parts`). HOOKS: SelectorFromLinearAddress_.
- `kernel32.rs`: VirtualAlloc is 64 KB aligned (Windows' granularity): the game's memory
  manager SSMem (0x440dd0...) breaks its free list otherwise (SSMemAllocAlign's padding).
  DeviceIoControl (MEMVXD) fails; SuspendThread is ignored.
- `dinput.rs` (new): system keyboard (DIK codes from `user32::poll`) and mouse (raw motion
  from winit's DeviceEvent), GetDeviceState and buffered GetDeviceData. `NSTRIKE_DI_LOG=1`.
- `miles.rs` (new): AIL_* on the dsound mixer (`dsound::Sample`: linear gain, signed 8-bit,
  loop counts); the driver's +0x4c is an IDirectSound (the movie player uses it); timers
  are threads calling `callback(user)`; MIDI messages are dropped (`NSTRIKE_MIDI_LOG=1`).
  Without Miles the game stops: "Unable to initialize audio system". `NSTRIKE_AIL_LOG=1`.
- `dsound.rs`: the primary buffer reports 64 KB (the movie player sizes its buffer by it).
- `rt_yield(cpu, loop head)`: `NSTRIKE_WATCH=` logs the last loop head a spinning thread
  yielded at, `NSTRIKE_WATCH=568b14,...` also dwords of guest memory, every 2 s.
- Fault reports name the nearest compiled function below the host address: a hooked
  function's original is not in that table, so a fault in it names the function before it.
- Debugging that worked: hooks logging the game's own functions (SSMemAlloc_ ...), and
  write-protecting a guest page (VirtualProtect on `mem().base + a`) to catch the writer.
- `glide.rs`: future_rust_cop's version d1f9ad0 (before its effects, widescreen, hooks);
  4:3 at the window's height. The game draws Glide on its game thread, not the window's:
  `window::with_gl` gives such a thread a context sharing the window's (`gl::Gl::share`,
  made by the window thread at its next pump; the window's surface is shared by pointer);
  `show_texture` from there finishes and hands the texture over (`SHOW`).
- `src/main.rs`: `nstrike [NUKEPC.EXE]`; compiles (cached in `compiled.bin` beside the EXE)
  and runs it; imports without implementation stop with "import X is not implemented yet".
- `cargo run --release --example dis -- game_files/NUKEPC/NUKEPC.EXE <addr|all>` disassembles.
- Environment variables are `NSTRIKE_*` (Future Cop's `FCOP_*`): `NSTRIKE_TRACE_API`,
  `NSTRIKE_PROF`, `NSTRIKE_WATCH`, `NSTRIKE_COUNT`, `NSTRIKE_TRACE_REG`, `NSTRIKE_SHOT`
  (`{t}` in the name: a picture every 0.5 s), `NSTRIKE_INPUT`, `NSTRIKE_WINDOW`.

## Commands

- `cargo build --release`, `cargo test --release` (no game files needed).
- `./target/release/nstrike.exe` from the repository: installs into `game_files/` from the
  CD image there, compiles (~1.4 s, 3081 functions) and runs.
