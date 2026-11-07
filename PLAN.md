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
- [ ] Texture formats: magenta and misplaced pictures (8-bit palettised P_8 with
      grTexDownloadTable? other formats than FCop's 1555)
- [ ] The cursor leaves trails, black bands: grLfbLock/grLfbWriteRegion/grLfbReadRegion
      (the menu's 2D parts?), grBufferNumPending
- [ ] Miles streaming (AIL_sample_buffer_ready/load_sample_buffer, through pointers)
- [ ] High resolution, widescreen, texture filtering, MSAA as in Future Cop

### 4. Later
- [ ] Videos (`videos.res`: format unknown yet)
- [ ] Gamepads, preferences, fullscreen, Linux
- [ ] Unsupported instructions in discovered code (Aas 204, Iretd 15, Les 6, ...): mostly data
      decoded as code? Check which functions they are in, and whether any runs
- [ ] Differential test against Unicorn (future_rust_cop's `src/bin/difftest.rs`)
