# Plan — what is done, what is open

Updated 2026-11-07. Details on how things work: CLAUDE.md.

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
- [ ] After the intro video: crash in the software renderer llTextureTri_ 0x4ba360 (read at
      0xffffffd2). Glide first (the goal anyway), then software mode again
- [ ] SuspendThread is ignored (WindowProc 0x4a13f8 suspends a thread on deactivation?)
- [ ] Thread priorities are ignored (the movie streamer and the audio timer raise theirs)
- [ ] Joysticks in DirectInput

### 2. Sound
- [x] Miles: samples and timers (not checked by ear yet)
- [ ] MIDI: send_channel_voice_message to a software synthesizer with the game's SoundFont
      `AUD_IMS1.SF2` (e.g. the rustysynth crate)

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
- [ ] Present with vsync (now without: tearing possible); frames paced by the display itself
- [ ] High resolution, widescreen, texture filtering, MSAA as in Future Cop

### 4. Later
- [ ] Videos (`videos.res`: format unknown yet)
- [ ] Gamepads, preferences, fullscreen, Linux
- [ ] Unsupported instructions in discovered code (Aas 204, Iretd 15, Les 6, ...): mostly data
      decoded as code? Check which functions they are in, and whether any runs
- [ ] Differential test against Unicorn (future_rust_cop's `src/bin/difftest.rs`)
