# Plan — what is done, what is open

Updated 2026-11-08. Details on how things work: CLAUDE.md.

## Done

- [x] CD examined: PC version, game EXE inside `setup.esa` (PKWARE DCL), CodeView names
- [x] Repository set up; recompiler core copied from future_rust_cop
- [x] `esa.rs` (archive + explode), `cv.rs` (CodeView publics), `setup.rs` (install from the
      CD image into `game_files/NUKEPC`)
- [x] Discovery from CodeView publics: 3081 functions (2161 named), compiled in ~1.4 s
- [x] Segment register moves (Watcom start-up)
- [x] The game runs its C run-time start-up and reaches WinMain
- [x] Platform layer from future_rust_cop: user32 (+ MessageBoxA, DialogBoxParamA as a log),
      window (winit), gl, gdi32, ddraw, dsound, misc; kernel32 additions; dinput (new:
      keyboard and mouse, state and buffered data)
- [x] Miles Sound System (`miles.rs`): samples through the dsound mixer, timers as threads,
      MIDI handles (messages not played yet)
- [x] Fixes the game needed: 64 KB aligned VirtualAlloc, .bss not loaded from the file,
      command line with the path, current directory, code addresses as immediates,
      `jmp cs:[reg + table]` jump tables, pushf/popf (16 bit), bsf/bsr, the primary sound
      buffer's size, SelectorFromLinearAddress_ hooked (no VGA memory)
- [x] The EA intro video plays (the game's own player, software DirectDraw)

## Open

### 1. Platform layer (next)
- [-] Software renderer (crash in llTextureTri_ 0x4ba360 after the intro video): not needed,
      Glide only (decided 2026-11-09)
- [x] SuspendThread/ResumeThread: the game suspends its game thread when its window loses the
      focus (WM_ACTIVATEAPP 0, now sent; the window is not minimised)
- [x] Thread priorities: the turn goes to the waiting thread with the highest priority
- [ ] Joysticks in DirectInput

### 2. Sound
- [x] Miles: samples and timers (not checked by ear yet)
- [x] MIDI music: send_channel_voice_message to rustysynth with the game's SoundFont
      `AUD_IMS1.SF2` (`midi.rs`), mixed into the sound output

### 3. Glide on OpenGL
- [x] `glide.rs` from future_rust_cop's early version (d1f9ad0: batching, mipmaps, MSAA;
      without its later effects). The game renders on its own thread: it gets a context
      sharing the window's (`window::with_gl`), finished pictures go to the window's thread
      as a texture (`show_texture`)
- [x] The main menu is drawn through Glide (wrong textures, see below)
- [x] Texture coordinates of textures smaller than 256 (s, t span 0..256 over the longer
      side): the movies' 128x128 tiles were misplaced
- [x] P_8 / AP_88 with the palette (grTexDownloadTable/Partial), ALPHA_INTENSITY_44,
      ARGB_8332, the alpha test; grAADraw* drawn as plain primitives
- [x] The first mission runs (Start Game, Enter through the briefing): terrain, helicopter,
      HUD and radar drawn right. Not played yet: controls, sound, speed unchecked
- [x] Front, back and aux buffers that really swap; grLfbReadRegion/WriteRegion (the cursor
      saves what is under it per buffer: no trails). A region read and written back unchanged
      is copied on the GPU at full resolution
- [x] The cursor's known flaw on modern systems (what it puts back is not the background):
      `gl_CopyBitmap_` hooked, the background it saves is kept on the GPU at full resolution
      and drawn back from there. The movies' purple look is their own
- [x] Miles streaming (AIL_sample_buffer_ready/load_sample_buffer): speech. Without it the
      game waited for ever after leaving a mission
- [x] Speed: the menu ran at 30 fps (the whole texture array's mipmaps built after every
      128x128 tile; the cursor's background read back from the GPU every frame); swaps paced
      to 60 Hz as the card's retrace; Sleep(0) with another guest thread running sleeps 1 ms
      (the message loop took the turn back ~800 000 times a second)
- [x] TMU memory kept over grSstWinClose/grSstWinOpen (the game reopens at 800x600 for a
      mission and uses textures from the menu: white HUD digits and radar marks)
- [x] The last movie frame flickering under the text before a mission: a thread's commands
      are flushed at grBufferSwap and grBufferClear before another thread draws
- [x] Glide from more than one thread: the text before and after a mission is drawn by
      another thread (it was invisible: 8 s of "loading"); a wrong palette from it may have
      been the garbled Config text. Textures are clamped to the primitive's rectangle (marks
      over letters at high resolution)
- [x] Frame rate unlocked: paced at the display's refresh rate (`NSTRIKE_FRAMERATE`), the
      game's clock in µs at the frame's due time (it moved in 10 ms steps: shaking at 60 fps),
      the camera's smoothing by fractions of 1/60 s. Physics checked: same speed at any rate
- [x] Speed: x87 24-bit rounding inline, dense call lookup, fewer turn hand-overs (~+25%)
- [x] Speed: x87 stack kept in Cranelift values along straight runs, control word tests once
      per run, a GL uniform lookup per frame cached (~415 -> ~520 fps unlimited)
- [x] Speed: the alpha test is a vertex attribute, not a batch's state (it ended 97% of the
      batches: ~250 draws a picture, the driver ~10% of the game thread). 2026-11-08, CPU-bound,
      unlimited: ~240-280 -> ~300-340 fps, drawing a picture 3.0-3.6 -> 2.2-2.7 ms (the 520
      above was before pictures, widescreen and the later fixes). A mapped ring buffer instead
      of glBufferData per batch and cached uniforms gained ~3%, within the noise: left out
- [x] Speed: x87 24-bit rounding's usual cases cover exact results and zeros, cut in a vector
      register, the mode made once a run: projection routines 23.6% -> 16.6% of the game
      thread, ~300 -> ~350 fps unlimited (2026-11-08)
- [x] Speed: straight runs of x87 code lifted twice, for 0xc7f with the SSE rounding set to
      truncate (no rounding checks): projection routines 16.6% -> 7.1%, ~307 -> ~337 fps in an
      A/B run (2026-11-08)
- [x] Speed: vec3fp16_Project*ListASM_ in Rust (`projection.rs`, ~276 -> ~294 fps), the
      quad renderers 0x475314 / 0x475a34 in Rust (`render.rs`, ~316 -> ~381 fps, 2026-11-12).
      Type 4 renderer 0x473c4c in Rust too. Next: Glide::add, the depth lookup.
- [x] Vsync, setting `vsync` (F10 menu, `NSTRIKE_VSYNC`, default on; 2026-11-09): the game
      thread's context swaps with interval 1 (fullscreen: the swaps' times then fall on whole refreshes; in a window the
      compositor kept tearing away anyway). Pacing stays ours: below the display's rate the
      driver queues a picture and returns at once (swap and glFinish never waited at 480 Hz),
      so the swap cannot say when the refresh is; glFinish after it bought nothing
- [x] High resolution, texture filtering, MSAA as in Future Cop
- [x] Widescreen (Hor+) in missions (`wide.rs`): 3D picture as wide as the window, HUD and
      message boxes in the 4:3 middle, terrain trapezoid widened (sized for 16:9 at least);
      setting and F10 menu row, 16:9 window sizes in the menu
- [x] Widescreen checked (2026-11-09): the HUD places nothing from 3D positions (HUDUserDraw_
      and its helpers never project); 3D sprites (0x423008, 0x430970) size their width by the
      x scale, so they stay round when stretched. The cockpit HUD modes (1, 2: H cycles them) are 4:3 (they were drawn stretched and in
      parts twice), and work at any resolution (the game allowed them only at 640x480:
      patched; the panel is redrawn whole when the Glide target is made anew).
      The map screen (SMFD) is 4:3. The text after a mission is drawn outside the HUD: 4:3
- [x] Mission logic at a fixed 60 frames a second (`frames.rs`); pictures in between at the
      display's rate, objects and camera interpolated (`pictures.rs`, as future_rust_cop's
      `fps.rs`); hook pick-up autopilot fixed for 60 frames
- [x] recomp_kit: the generic part of fixed logic rate + pictures between frames, GUIDE 8b;
      widescreen GUIDE 8c; code patches, vsync, drawing-once lessons (2026-11-09)
- [x] Settings file `game_files/nstrike.toml` (`settings.rs`): window, fullscreen (Alt+Enter
      saves it), frame rate, MSAA, texture filter; `NSTRIKE_*` override for one run
- [x] Overlay menu (`menu.rs`, F10): fullscreen, window size, frame rate, MSAA, texture
      filter; used at once and saved; a mission stands still while it is open. Decided: the
      game's Config menu is not extended, except for what it has already (the resolution)
- [ ] Overlay menu: gamepad, more settings as they come (resolution scale)

### 3a. Modern graphics effects (as future_rust_cop's 2a, one after another)
- [x] Step 0, depth (2026-11-10, `ao.rs`): Glide vertices carry none (oow 1, ooz 0.1, painter's
      order). vec3fp16_ProjectTerrainListASM_ 0x4b96c0 / ...ModelListASM_ 0x4b98c2 hooked:
      out 8 bytes a point (sx, sy s16 in 1/8 pixel, z u16, on-screen byte); a table (sx, sy)
      -> z, looked up per Glide vertex (x * 8): 89% of all vertices found (the rest HUD and
      menus). Camera from the terrain projection's view: 24000 / 8 pixels per unit at 800x600,
      centre 3200, 2400 (narrow, far: z ~11000-16000). Second attachment RG32F as in FCop;
      `wide::render_poly` says 3D or HUD (`glide::set_3d`), the first HUD primitive of a frame
      runs the passes. `NSTRIKE_GBUFFER=depth|ao`
- [x] Step 1, SSAO (2026-11-10): FCop's passes, focal in x and y apart (widescreen), radius
      500 units (a tree ~700), depth edges in units (0.5 radius), not relative to z (the
      narrow camera's depth spread is small). Setting `ambient_occlusion` (default on),
      F11 effects menu, `NSTRIKE_SSAO`; tuning `NSTRIKE_AO_RADIUS`, `NSTRIKE_AO_STRENGTH`.
      Mission 1 at 1600x900: ~367 -> ~360 fps. Open: the user's look; other missions
      Fixed: HUD off (passes at the swap) and cockpit HUD (Glide draw offset `[0x4ede5c]`)
- [x] Step 2, dynamic lights (2026-11-10, `lights.rs`): additive primitives of the game's fire
      and explosion pyros (sequence by hooking the pyro drawer 0x430970; colour could not tell
      a boat's wake from fire) light their surroundings after the AO. Setting `dynamic_lights`
      (F11). Open: strength and reach by the user's look, other missions' sequences
- [x] Step 3, shadows (2026-11-11, `shadows.rs`): shadow map of the game's triangles from the
      game's sun; the objects of the tiles around the picture are drawn too, zoomed out, only
      as casters (`pictures::shadow_ring`); in the ring faces turned away from the camera are
      made too (`shadows::model_face`, 2026-11-11). Open: the terrain beyond the picture casts nothing
      (its table is full at 16:9); cost ~30%
- [x] Step 4, bloom (2026-11-11, `bloom.rs`, FCop's): the bright part of the 3D picture
      (threshold lower where the mask says additive), a chain of 6 levels down and up, added
      before the HUD. Setting `bloom` (F11). Mission 1: no measurable cost (~180 fps either way)
- [x] Step 5, heat shimmer (2026-11-11, `heat.rs`, FCop's): the lights' sources (fire and
      explosion pyros) with a depth; the picture read a few pixels off near them, rising, not
      where something is in front. Setting `heat_shimmer` (F11). Open: the user's look
- [x] Depth buffer (2026-11-11, `zbuffer.rs`): the game's ordering table put single triangles
      behind other geometry in front; 3D primitives with depth at their corners are tested
      (solid ones write). Setting `depth_buffer` (F11). Open: the user's look (decals, sprites)
- [x] Step 6, lit smoke (2026-11-11, `lights.rs`, as FCop's): the smoke and dust pyros (0x18,
      0x19, additive) times 1 + 3 * the light at their centre from the last picture's merged
      lights (by distance; half from lights behind, aside within 1.5 reach), per primitive (a
      vertex attribute `tint`). Setting `lit_smoke` (F11), only with the lights on;
      `NSTRIKE_SMOKE_STRENGTH`. Open: the user's look
- [ ] Next: our own particles (sparks, debris, smoke) at the game's explosions. Needs the
      pyro's place, speed and age (reverse engineering pyroCreate_ and the pyro record);
      simulated per logic frame, drawn as sprites with the depth buffer

### 4. Later
- [x] Videos (`videos.res`)
- [ ] Gamepads, Linux
- [ ] Unsupported instructions in discovered code (Aas 204, Iretd 15, Les 6, ...): mostly data
      decoded as code? Check which functions they are in, and whether any runs
- [ ] Differential test against Unicorn (future_rust_cop's `src/bin/difftest.rs`)
