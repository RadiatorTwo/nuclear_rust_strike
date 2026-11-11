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
- The game's functions that produce faults may be replaced by new code (recomp_kit rule):
  no complex work-arounds to keep a faulty original.
- The user tests; on "commit und push" do it at once. Keep own test runs to a minimum.

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
- Division by zero does not stop the game: the event handler logs it once per address and the
  division goes on by 1 (the PlayStation's does not trap; matRotTransPers_ 0x413e16 divides
  by a point's depth, exactly 0 at times: a crash seen 2026-11-07). The default `rt` handler
  (tests) still dies.
- `discover.rs` also takes 32-bit immediates pointing at not yet decoded code as entries
  (`push offset thread_proc`), and `jmp cs:[reg + table]` (reg = index*4) as jump tables.
- `image.rs`: a section with raw offset 0 (Watcom's .bss) is uninitialised: zeros.
- `platform/mod.rs`: the game sees itself at `C:\NUKEPC\NUKEPC.EXE` (the command line too:
  it takes its folder from it) and the CD as D: (`D:\worlds` = `data\Worlds`);
  SetCurrentDirectoryA is followed (`guest_parts`). HOOKS: SelectorFromLinearAddress_,
  gl_CopyBitmap_ (`glide.rs`).
- `kernel32.rs`: VirtualAlloc is 64 KB aligned (Windows' granularity): the game's memory
  manager SSMem (0x440dd0...) breaks its free list otherwise (SSMemAllocAlign's padding).
  DeviceIoControl (MEMVXD) fails. Sleep(0) sleeps 1 ms when another
  guest thread runs: guest threads take turns (`turn_take`), and WinMain's PeekMessage/Sleep(0)
  loop otherwise took the turn back at every loop head of the game thread. It also sleeps in a
  thread below normal priority (WinMain's, LOWEST): it yielded at once while the game thread
  was in host code, took the turn ~340 000 times a second and kept a core busy; the game
  thread was ~10% slower (2026-11-08). The frame rate still swings in phases of seconds with
  the same work: the game thread runs on whichever logical CPU Windows picks, and is slower on
  a core's second hyperthread beside other processes (pinned to one core: ~425 instead of
  ~375 fps, still swinging; not done).
- Guest threads (`platform::GuestThread`, 2026-11-08): SuspendThread/ResumeThread (and
  CREATE_SUSPENDED) stop a thread at its next loop head or before guest code after an import
  (never in host code: it holds none of our locks). The turn goes to waiters by
  SetThreadPriority's priority: a holder yields at once at a loop head when a higher one waits,
  else after `SLICE`, then politely (behind all waiters: no starvation). The game: WinMain's
  thread LOWEST, the mission text, `virtual_timer` and a Miles timer callback HIGHEST, the CPU
  speed test TIME_CRITICAL. Losing the window's focus sends WM_ACTIVATEAPP 0 (`user32`, not in
  scripted runs): in a mission the game pauses (HUDPauseGame_, P resumes) and suspends its game
  thread (WindowProc 0x4a13f8; not at the first deactivation: its flag `[0x4eee88]` starts at
  1); WM_ACTIVATEAPP 1 resumes it. Its ShowWindow(SW_MINIMIZE) is ignored.
- `dinput.rs` (new): system keyboard (DIK codes from `user32::poll`) and mouse (raw motion
  from winit's DeviceEvent), GetDeviceState and buffered GetDeviceData. `NSTRIKE_DI_LOG=1`.
- `miles.rs` (new): AIL_* on the dsound mixer (`dsound::Sample`: linear gain, signed 8-bit,
  loop counts; double buffering through `dsound::sample_queue`: speech, waited for); the driver's +0x4c is an IDirectSound (the movie player uses it); timers
  are threads calling `callback(user)`. MIDI (`midi.rs`, 2026-11-08): the game's sequencer
  (timer callback 0x497d3c, 60 Hz) sends GM channel voice messages; they play on rustysynth
  with `data\AUD_IMS1.SF2` (19 presets: exactly the programs the music uses, drums bank 128),
  queued and applied at the output's next block, mixed in `dsound`'s output callback;
  AIL_set_sequence_volume ramps the gain (`NSTRIKE_MIDI_LOG=1` logs the messages).
  Without Miles the game stops: "Unable to initialize audio system". `NSTRIKE_AIL_LOG=1`.
- `dsound.rs`: the primary buffer reports 64 KB (the movie player sizes its buffer by it).
- `frames.rs` (new): the game's clock (sysUpdateTick_ 0x4823d0 hooked, µs). In a mission the
  logic runs 60 frames a second whatever the display (2026-11-07): much of it counts frames,
  not time (above 60 fps explosions never ended, missiles left a trail piece a frame, the
  hook's pick-up took the controls for good). The mission loop 0x46be9c waits at the start
  of each frame (its sysUpdateTick_ calls, return 0x46bfc8 / 0x46c2f1) until it is due; its
  clock goes on by exactly 1/60 s (`logic_frame_us`). Menus: paced at grBufferSwap
  (display's refresh rate or `frame_rate`). `NSTRIKE_FPS=1` logs frames a second.
- `pictures.rs` (new): pictures between logic frames, as
  future_rust_cop's `fps.rs`. Each picture shows the game a frame late, interpolated between
  the last two logic frames: the camera (StrkCam_Update_ hooked: position [0x503f54], angles
  [0x503f64]; the matrices made with Camera_SetYPR_, the sun with Actor3D_UpdateSun_) is
  swapped in only while drawing (Rndr_TerrainRender_, ObjMgr's draw callback 0x42c018,
  Actor3D_DrawDeathDot_, HUDUserDraw_ hooked); an object (+0x2c position, +0x38 matrix 3x3
  i16; children [+8], next [+0xc]) only while 0x42c018 draws it. ObjMgr_ProcessAllObj_: per
  visible tile 0x42c000 moves (Obj_Process_), 0x42c018 draws. In the wait before the next
  logic frame the loop's drawing runs alone (`picture`: disp_FrameChange_ with the loop's
  argument, terrain, the visible tiles' and the high-rate list's objects, HUD,
  Order_DrawOT_); afterwards the static data (0x4d0000..0x56c200) and the drawn objects'
  memory are put back (not SSMem's heap lists 0x4e8454.., 0x51d990..: their records live
  elsewhere, putting them back broke the heap); the terrain draws from its data as before the
  frame's own drawing. While a picture draws, the game thread keeps the turn (`hold_turn`:
  the audio timer's changes were undone, sounds went missing). `NSTRIKE_FPS=1` also logs
  pictures a second and their swap/draw times. A picture is drawn only when the swap it
  will be shown at (the one after the pending one: `next_shown() + picture_period()`) comes
  before the next logic frame is due; else that logic frame runs at once (up to a period
  early, its clock unchanged). Before (2026-11-08), such pictures were clamped to the newest
  frame: steps of 0.69 / 0.31 frame alternating at 120 Hz, moving things looked doubled.
  The camera of a picture is not made with Camera_SetYPR_: StrkCam_Update_ keeps its angles
  in 16.16 (`[0x52301c]`..), but builds the camera from whole 1/4096 turns, which step
  unevenly in a turn (7, 8, 6, 8 a frame) and swing the camera's place (back by `[0x523030]`
  along the matrix's third column, aside by `[0x523068]`): the picture shook in turns
  (2026-11-08). Per logic frame the point followed is noted (`base`: the camera less that
  offset, with the game's own matrix); a picture interpolates the fine angles, builds the
  matrix in floats (`rot_yxz` = BuildRotMatrixYXZ_) and places the camera from `base`.
  The game thread shows its pictures itself (`window::show_texture` -> `gl::Shared::present`:
  blit and swap with its shared context, right after `frames::pace`; `gl::PAINT` keeps the
  window's thread from drawing at the same time, `window::LOOK` carries overlay, video and
  window size over). The window's thread showed them at its next turn: 1.2 to 7 ms apart at
  240 fps while their contents were 4.2 ms apart (the helicopter shook against the ground).
  `pace` sleeps up to 1.5 ms before the due time and spins the rest (now 4.0 to 4.4 ms).
  With the setting `vsync` (F10, `NSTRIKE_VSYNC`) the shared context swaps with interval 1
  (`frames::vsync`, set in `Shared::present` when it changes); the pace stays ours (the driver
  queues pictures while the game is slower than the display: the swap returns at once).
- The chopper's autopilot (flynVehiIntrBehavior_ 0x4251dc, the hook pick-up flies it over the
  load) steps by time / 0x114 with the rest dropped: at 60 frames (256) no step ever, the
  controls stayed taken. `frames::autopilot` carries the rest. `NSTRIKE_CTRL_LOG=1` logs
  controller contexts, the hook's states (0x41ec8c) and the player's pick-up fields.
- A test run into the first mission: `NSTRIKE_INPUT="500:focus 4000:Enter 7000:Enter
  10000:Enter 25000:Enter 30000:Enter 35000:Enter"` (key names are case-sensitive: `Enter`,
  `Up`, `Left`; unknown ones are logged), flying from ~45 s.
- Profile 2026-11-11 (mission 1 flying, all effects, ~180 fps): guest code only ~30% of the
  game thread, our Rust ~40%, GL driver calls ~26%. Fixed then (~179 -> ~193 fps): the glFlush
  above; the per-primitive tags (who made it, ring, object, HUD) one map `wide::TAGS` (four
  SipHash sets/maps before); `ao` inserts a projection's points under one borrow and
  `depth_near` searches a flat grid. Open: `pictures` snapshot (~8%), `_bezBezierParameterize`
  6%, 0x475314 / 0x475a34 (~8.5%), GL uploads (shadow casters, `Glide::copy`). Our own code
  symbolised: `cargo rustc --release --bin nstrike -- -C link-arg=/MAP:<file>` (own target
  dir) and `NSTRIKE_SAMPLE_RAW`.
- Speed (mission ~330 -> ~415 fps unlimited, 2026-11-07): the renderer runs x87 with control
  word 0xc7f (24 bits, truncating). `lift/x87.rs` `round24` rounds those results inline (the
  double result cut to 24 bits; when it lies right on a 24-bit number or a middle, the error's
  sign from TwoSum/fma decides); only other modes, subnormals and infinities call
  `rt_fpu_round` (it took 24% of the game thread). `tests/x87.rs` `x87_precision` checks it
  against `rt_fpu_round`. The common cases (`round24_between`, 2026-11-08, ~300 -> ~350 fps):
  s between two 24-bit numbers, or on one with no error (exact: the error from TwoSum/fma is
  only a branch's condition), or an exact zero; the game's results are often exact (~20
  million a second went to the slow `round24`, its branch mispredicted). The cut is done in a
  vector register (`vpaddq`/`vpand`, not via an integer register: the next operation waits
  for it); the mode's values are stored once a run into `Cpu::x87_mode` (nobody reads them:
  without a use in a dominating block Cranelift made them again in every operation's block).
  `tests/x87_bench.rs` (ignored; `--ignored --nocapture`) times a projection-like loop per
  point under 0xc7f and 0x27f. Straight runs with at least 4 x87 operations (`rz_runs`) are
  lifted twice (`versions`, 2026-11-08, projection 16.6% -> 7.1%, bench 71 -> 42 ns): if
  the control word says 24 bits truncating, `rt_mxcsr_rz` sets the SSE rounding to truncate
  and each operation is the double one cut to 24 bits (truncating twice is truncating once),
  tested only for a normal result (`arith_rz`), a float store is fdemote; `x87_settle` puts
  MXCSR back at the run's end. The cache is written back before the run (no value computed
  before the switch is shared into it); results are branch conditions (made after the
  switch). `rt_fpu_round` runs with the default rounding itself. Not in such runs: fldcw,
  helpers, fild qword. A float store beyond a float's range under 0xc7f gives the largest
  float (the FPU's masked overflow), not infinity (`rt_fpu_round` gave infinity before). `rt_lookup` (indirect calls) reads a dense table over the code;
  thunk addresses miss at once. Glide imports that never wait keep the turn (`SLOW_GLIDE`
  lists those that give it up). A thread keeps the turn at loop heads for `SLICE` (0.5 ms,
  `tsc`): the game and controller threads traded it 60 000 times a second.
  `NSTRIKE_SAMPLE=1` (`sample.rs`): a sampling profiler of the game thread (function or host
  module, every 2 s; with `NSTRIKE_DUMP=<addr>` that function by offset in its machine code, as
  the dump shows it; `NSTRIKE_SAMPLE_RAW=<file>` all samples, our own code as `nstrike.exe+off`:
  symbolise with a build with `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only` and
  llvm-symbolizer; in system code the stack is unwound to our frame: "ntdll <- nvoglv64 <-
  nstrike.exe+off").
- x87 registers cached (2026-11-07, ~415 -> ~520 fps with the next item; outdated: 2026-11-08
  ~240-280 fps, CPU-bound as always): `lift/x87.rs`
  `Cache` keeps ST(i) as Cranelift values along straight runs of instructions (by place
  relative to TOP at the run's start), with the control word and its rounding tests (`Mode`);
  written back by `x87_settle` before join points (`Lifter::joins`: branch targets), anything
  not a plain integer instruction, helpers. Off in functions with jump tables or rewritten
  instructions. fild/fistp qword's exact integer stays in memory, not moved by fxch.
  `tests/x87_cache.rs` runs random x87 chains cached and uncached (`lift::X87_CACHE`) and
  compares. glGetUniformLocation in `Glide::copy` (a frame's MSAA copy) cost 11%: NVIDIA's
  lookup makes system calls; `gl::uniform` caches. `NSTRIKE_DUMP=<hex addr>` disassembles a
  function's compiled code (the cache file is not read). Left: guest code ~68% of the game
  thread, the projection routines (vec3fp16_Project*ListASM_) ~25%.
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
  `show_texture` from there hands the texture over with a fence (`SHOW`). The game draws from
  two threads (the game's, and `trantext` for the text before and after a mission): Glide's
  state is global (`GLIDE` mutex); what contexts do not share (framebuffers, VAOs) is made
  per thread (`Local`), and while another thread drew within the last two buffer swaps every call
  ends with glFlush (swaps and clears always do; `drawers`. Before 2026-11-11 by the count of
  `Local`s: the mission text's thread keeps its own after it ends, every call in a mission
  flushed). The alpha
  test is a vertex attribute (the game changes it between almost all primitives: one draw
  each, 2026-11-08); a batch ends only at a change of blend, chroma key, filter, clamp, clip. The TMU's
  memory (texture arrays, textures, palette) outlives grSstWinClose (`KEPT`), as on the card.
  `NSTRIKE_SHOT_MS` sets the picture interval of NSTRIKE_SHOT. The game's Glide
  layer is `gl_*` (0x478cc8...): `gl_ScaledTransferToLFB_` draws 16-bit pictures (movies,
  menu backgrounds) as 128x128 tiles, `gl_CopyBitmap_` saves what is under the cursor with
  grLfbReadRegion (hooked: the save is kept on the GPU and drawn back from there, flags 7 save,
  6 restore, 0xa the cursor itself) over rectangles rounded outwards (`target_rect_out`: at 1080 high, scale
  2.25, an edge on a pixel's middle left cursor lines behind), `gl_SetTMUPaletteRange_` the palette. `NSTRIKE_GLIDE_LOG=1` logs downloads.
- `src/settings.rs` (new): the port's settings in `game_files/nstrike.toml` (`key = value`,
  written whole with comments at the first start and on every change: `update`); wrong values
  keep the default and are reported. `get()` = the file with `NSTRIKE_WINDOW`,
  `NSTRIKE_FULLSCREEN`, `NSTRIKE_FRAMERATE`, `NSTRIKE_VSYNC`, `NSTRIKE_MSAA`, `NSTRIKE_FILTER`,
  `NSTRIKE_WIDESCREEN` over it (not saved). The game's own settings stay in its Config menu / nukepc.sav; the game's menu is
  not extended, except for what it has (resolution).
- `src/platform/text.rs` (new): the text before and after a mission is written letter by
  letter by its own thread (0x49d4d8, `trantextDisplay_`), paced by `clock_` (1/100 s: a
  picture and a letter every 10 ticks, 30 after a line; between them `Sleep(ticks * 10)`).
  Written 4 times as fast (`SPEED`, not a setting): in that thread (hooked, thread-local
  flag) `clock_` (0x4a5c7e, hooked) runs 4 times as fast and `Sleep` is 4 times shorter
  (kernel32 `sleep`). Mission 1's text: 8.4 s originally, 3.4 s at 3x.
- `src/platform/menu.rs` (new): our menu, F10 settings page, F11 effects page (VK 0x79 /
  0x7a, not passed to the game; the other key turns the page). Drawn into
  the window's overlay (640x480 ARGB, over the picture's 4:3 middle; font `font.rs` from
  future_rust_cop). `window.rs` owns the `Menu` while open: `App::push` hands it all input
  (mouse moves converted to overlay pixels), the game gets only keys/buttons going up, focus
  and close; raw mouse motion is held back, the cursor shown and released. Changes apply at
  once (`frames::set_rate`, `glide::set_msaa/set_filtering`, fullscreen and window size by
  the window) and are saved. While open, the mission loop waits in `logic_frame_us`
  (`menu::wait_closed`, drawing pictures so changes show); afterwards the logic clock goes
  on with the guest's (whole ticks) and the mission start `[0x4ed728]` and last tick
  `[0x54d920]` move by the pause too, as strikeClockPause_ does. (Before, the logic clock
  lagged the guest's: the game's Esc menu set the mission start from the guest clock, the
  mission time went back, Slerp_ 0x413f40 looped ~4 billion times: frozen.) Menus outside missions keep running. `NSTRIKE_INPUT` items go through it too
  (`50000:F10 52000:Down 53000:Right`).
- `src/platform/wide.rs` (new, as future_rust_cop's): widescreen (Hor+) in missions. The
  projection's x scale `[0x568af0]` (View_SetWindow_ 0x48d7d0 makes it, with the clip margin
  `[0x568af8]` and bound `[0x568b00]`) is narrowed by k = (4/3) / aspect before every logic
  frame and picture; the Glide target takes the window's aspect and the 3D picture is
  stretched over it. The HUD shares the 3D picture's ordering table (its own window only in
  some HUD modes): primitives made in HUDUserDraw_ (disp_AllocPoly_ hooked) or under another
  window (message boxes) are drawn in the 4:3 middle (gl_RenderPoly_ hooked, `glide::View`);
  while the HUD runs the x scale is the 4:3 one. The terrain trapezoid (ViewPort_Init_
  0x46f368, corners in tiles) is widened by 1/k; its tile count sizes TileMgr's lists at the
  mission's start, so it is made for at least 16:9. The visible tiles come from
  0x467b04: 0x4672bc writes the trapezoid's rows into a stack buffer (`[ebp-168h]`, 64 rows),
  0x4676d4 reads them; the wide trapezoid, turned, has more and overwrote the saved ebp
  (streaks of missing terrain flying forward and turning, then crashes): both hooked onto a
  buffer of ours (2026-11-08). The mission menu (R, SMFD_Enter_/SMFD_Exit_ hooked) is 4:3 from its
  start: it draws its frame once, and the target, still wide for the HUD's last 250 ms, turned
  4:3 afterwards, made anew black (`glide::follow_window` at SMFD_Enter_). The cockpit HUD
  modes (1 and 2; H cycles 0, 1, 2, off) are 4:3 as well (HUDUserModeSet_ 0x45f2b4 hooked).
  The game allows them only at 640x480: both checks are patched out (`wide::PATCHES`, applied
  to the image before compiling by `Image::patch`, which checks the old bytes); the panel is
  placed and scaled by the screen's size already. HUDUserDraw_ draws the whole panel only when
  `[0x51f1b9]` says so, else the changing parts: when the Glide target is made anew (4:3,
  window size, MSAA) `wide::redraw` sets it for the next two HUD frames.
  A message box (the Esc menu's, every frame) saves the window set with View_GetWindow_ (a
  new copy), draws under window 0 and sets the copy back: copies of a 3D window count as 3D
  (`COPIES`, View_GetWindow_/View_FreeWindow_ hooked; before, the 3D picture was stretched
  4:3 from the first Esc on, 2026-11-10).
  The HUD places nothing from 3D positions; 3D sprites size by the x scale. Wide only while the HUD is drawn (else
  4:3). Setting `widescreen` (F10), `NSTRIKE_WIDE=<aspect>` forces one (0 off).
  `window::window_size` from other threads reads the size of the window's last drawing (it
  was 1280x960 there: the Glide target was always 960 high).
- `src/platform/ao.rs` (new, as future_rust_cop's): depth for the modern effects and SSAO.
  Glide vertices have no depth (oow 1, ooz 0.1): the two projections (0x4b96c0 terrain,
  0x4b98c2 models; out 8 bytes a point: sx, sy in 1/8 pixel, z u16) are hooked and a table
  (sx, sy) -> z gives each 3D vertex its z (89% found). `glide.rs` writes it into a second
  attachment (RG32F) while `wide::render_poly` says 3D (`glide::set_3d`); the first HUD
  primitive of a frame runs the passes, else the buffer swap (HUD off). The Glide layer adds
  a draw offset `[0x4ede5c]`, `[0x4ede60]` (1/8 pixels) to the projected points: the lookup
  takes it off (cockpit HUD modes: the 3D window's corner, (10, 10) at 800x600; without it
  no vertex was found and the cockpit had no AO). Camera from the terrain projection's view (800x600:
  3000 pixels per unit, narrow and far). F11 menu, `NSTRIKE_SSAO`, `NSTRIKE_GBUFFER=depth|ao`.
- `src/platform/lights.rs` (new, as future_rust_cop's): dynamic lights. The game's effects are
  pyros (sequences from PYROS.PAK, global), all drawn by 0x430970 (hooked: sequence at pyro
  +0x38); `wide::alloc_poly` notes per primitive who made it (pyro sequence, else
  disp_AllocPoly_'s return address), `wide::render_poly` which is drawn. Additive 3D primitives
  (blend 1,4) of the sequences in `LIGHT_PYROS` (0-7, 0x15: fire, explosions; not 0x18/0x19
  smoke, 0x1d a boat's wake, 0x1e water spray: by colour they could not be told apart) become
  lights: depth from the projected points, for sprites the nearest one within 32 px
  (`ao::depth_near`); colour = local colour x texture rectangle mean (CPU copy `Texture::rgb`)
  x alpha by the alpha combine (the glows' constant has alpha 0: function 6, texture alpha
  minus local). Merged, 32 at most, applied after the AO (picture * (1 + 1.6 L) + 0.12 L).
  The depth is made when AO or lights want it (`ao::depth_wanted`). Setting `dynamic_lights`
  (F11, `NSTRIKE_LIGHTS`), `NSTRIKE_GBUFFER=lights`, `NSTRIKE_LIGHT_STRENGTH`/`_REACH`,
  `NSTRIKE_LIGHT_PYROS` (hex list), `NSTRIKE_LIGHT_LOG=1` (glows by sequence). Fire keys in
  the test script: Space, Q, E after flying forward (`46000:Up*3000 49500:Space*3000`).
- `src/platform/shadows.rs` (new): sun shadows from a shadow map of the game's own triangles.
  `glide::flush` keeps the 3D triangles that write depth (gmode 1, all corners with depth) as
  casters (batched vertices, chroma key in the constant colour's place); `post_3d` draws them
  from the sun (orthographic, 2048², view space rebuilt from screen position and depth; keyed
  and low-alpha texels cut: palm fronds) and the AO's half-resolution pass looks them up
  (3x3 hardware PCF; applied with the AO, also with it off). The sun is the game's:
  Actor3D_UpdateSun_ 0x410310 turns the light's direction `[0x503f28]` ((-147,-147,-147),
  world y up) into camera space `[0x503f2e]` (y down; it points away from the sun); read with
  the camera at the terrain's projection (`ao::sun`). A screen-space ray march came first: it
  flickered under the rotor and stayed grey whatever the strength. Off-screen casters: after
  ObjMgr_ProcessAllObj_ (0x42c088, hooked) and in pictures, `pictures::shadow_ring` draws the
  object lists of ObjMgr's visible rectangle, the tiles around it and the high-rate list once
  more; objects' primitives in the picture itself are no casters (`wide::object` in
  `draw_obj`, `glide::set_no_cast`, `no_cast_parts` of the batch): the game leaves out
  objects and triangles off its screen, a shadow popped at the screen's edge (2026-11-11; only
  the ring's tiles before). It also leaves out a model's faces turned away from the camera
  (0x411ca4, called by Actor3D_Render_ per face: normal against the view direction, unless the
  face's first byte says two-sided): in the ring every face is two-sided while it is drawn
  (`shadows::model_face`; ring faces made 57% -> 99%; the far side of the start boat cast
  nothing). Lists (rows `[0x51b65c]..[0x51b654]`,
  columns `[0x51b658]..[0x51b650]`, lists at 0x507e50 + row 0x600 + column 0x18; a tile is
  2048 units) with the projection zoomed out by 2 (`wide::zoom_out`, y scale `[0x568af4]`,
  margin `[0x568afc]`, bound `[0x568b04]`), objects put back afterwards (`in_picture`); their
  primitives are tagged in disp_AllocPoly_ (`wide::casting`) and in gl_RenderPoly_ go only
  into the casters (`glide::set_cast_only`, `cast`: corners scaled back about the projection's
  centre `ao::centre`). Tried before: a guard band zooming the whole picture and widening the
  terrain trapezoid: the terrain's visibility table 0x520510 (9 x 33 x 34 cells of ~2 tiles,
  read by 0x469574/0x469098 under Rndr_TerrainRender_, strides 0x467/0x22 and limit 0x21
  fixed in ~18 functions) fits ~66 tiles at any heading, 16:9 already needs 65: streaks, then
  crashes; a memory of drawn triangles in the world (camera matrix `[0x503f6a]` 3x3 i16 4096,
  position `[0x503f54]`) lost moving things. Mission 1: shadows ~30% of the frame rate
  (ring ~15%). Setting `shadows` (F11, `NSTRIKE_SHADOWS`); `shadow_quality` (F11,
  `NSTRIKE_SHADOW_QUALITY=low|mid|high|ultra`, `shadows::look`): map 1024/2048/4096/4096,
  PCF 3x3/5x5/5x5/5x5 taps 1/1.5/1.5/1 texels apart; low looks the shadow up in the AO's
  half-resolution pass, blurred with it (~18 px: the map's size hardly mattered), the others
  per pixel in the last pass; low without the ring (objects cast from the screen: may pop at
  its edge); `NSTRIKE_GBUFFER=shadows`,
  `NSTRIKE_SHADOW_STRENGTH` (0...1).
- `src/platform/bloom.rs` (new, future_rust_cop's): bloom, the last pass of `glide`'s
  `post_3d` (HUD not bloomed): the 3D picture copied (resolved), its bright part at half
  resolution (soft threshold 0.72, 0.5 where the depth attachment's mask G says additive), 6
  RGBA16F levels down (13 taps) and up (tent, ONE/ONE), added at strength 0.35. Needs the
  depth attachment for the mask (`ao::depth_wanted`); only pixels with depth or mask glow
  (menus, loading pictures and the cockpit panel stay over frames: the glow piled up, white). Setting `bloom` (F11, `NSTRIKE_BLOOM`),
  `NSTRIKE_GBUFFER=bloom` (debug view 5: the AO and lights passes draw as usual under it),
  tuning `NSTRIKE_BLOOM_STRENGTH`, `NSTRIKE_BLOOM_THRESHOLD`.
- `src/platform/heat.rs` (new, future_rust_cop's): heat shimmer after the lights, before the
  bloom. Sources from `glide`'s `add` where the lights' are (`lights::gives_light`, also with
  the lights off), size capped at `MAX_SIZE` 1200 units; the picture copied and read a few
  pixels off near them (waves rising), not where the pixel's geometry is in front of the
  source. Setting `heat_shimmer` (F11, `NSTRIKE_HEAT`), `NSTRIKE_HEAT_AMPLITUDE`.
- Lit smoke (`lights.rs`): smoke pyros (`SMOKE_PYROS` 0x18/0x19, additive) are tinted per
  primitive in `glide`'s `add` (`lights::smoke_tint`, vertex attribute 7 `tint`, VERTEX 24
  words) by the last picture's merged lights (`Glide::smoke_lights`, set in `post_3d`).
  Setting `lit_smoke` (F11, `NSTRIKE_LIT_SMOKE`), only with the lights on;
  `NSTRIKE_SMOKE_STRENGTH` (3.0).
- `src/platform/zbuffer.rs` (new): a depth buffer for the 3D picture (the game's ordering
  table sorts with mistakes: triangles behind others came out in front). A DEPTH_COMPONENT32F
  renderbuffer in `Gdepth` beside the depth attachment (same samples, attached and cleared
  with it). The vertex shader writes 1 - 100 / z; `glide`'s `add` gives corners without depth
  the mean of the others (`zbuffer::fill`, at least half known) and splits batches by
  `batch_z`; `flush` tests LEQUAL, writes for gmode 1 only, the others drawn nearer by `BIAS`
  (0.4%). Without depth (HUD, menus, sprites): the game's order. Setting `depth_buffer` (F11,
  `NSTRIKE_ZBUFFER`).
- `src/main.rs`: `nstrike [NUKEPC.EXE]`; compiles (cached in `compiled.bin` beside the EXE)
  and runs it; imports without implementation stop with "import X is not implemented yet".
- `cargo run --release --example dis -- game_files/NUKEPC/NUKEPC.EXE <addr|all>` disassembles.
- Environment variables are `NSTRIKE_*` (Future Cop's `FCOP_*`): `NSTRIKE_TRACE_API` (`1`
  every import, else those whose name contains the value: `NSTRIKE_TRACE_API=glide2x`),
  `NSTRIKE_PROF`, `NSTRIKE_SAMPLE`, `NSTRIKE_DUMP`, `NSTRIKE_FPS`, `NSTRIKE_FRAMERATE`, `NSTRIKE_WATCH`, `NSTRIKE_COUNT`, `NSTRIKE_TRACE_REG`, `NSTRIKE_SHOT`
  (`{t}` in the name: a picture every 0.5 s), `NSTRIKE_INPUT` (`move@x,y` also moves
  DirectInput's mouse; with it set the window does not catch the mouse or take the foreground), `NSTRIKE_WINDOW`.

## Commands

- `cargo build --release`, `cargo test --release` (no game files needed).
- `./target/release/nstrike.exe` from the repository: installs into `game_files/` from the
  CD image there, compiles (~1.4 s, 3081 functions) and runs.
