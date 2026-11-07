# Plan — what is done, what is open

Updated 2026-11-07. Details on how things work: CLAUDE.md.

## Done

- [x] CD examined: PC version, game EXE inside `setup.esa` (PKWARE DCL), CodeView names
- [x] Repository set up; recompiler core copied from future_rust_cop
- [x] `esa.rs` (archive + explode), `cv.rs` (CodeView publics), `setup.rs` (install from the
      CD image into `game_files/NUKEPC`)
- [x] Discovery from CodeView publics: 3081 functions (2161 named), compiled in ~1.4 s
- [x] Segment register moves (Watcom start-up)
- [x] The game runs its C run-time start-up and reaches WinMain (stops at LoadIconA)

## Open

### 1. Platform layer (next)
Copy from future_rust_cop and adapt, in the order the game needs them:
- [ ] user32 (window class, window, message loop, dialogs: DialogBoxParamA, MessageBoxA,
      SetDlgItemTextA, mouse_event), `window.rs`, `gl.rs` (winit + OpenGL)
- [ ] kernel32 missing: CopyFileA DeleteFileA GetFileSize SetCurrentDirectoryA
      SetFileAttributesA CreateDirectoryA GetFileAttributesA lstrcmpiA SuspendThread
      GetProcessAffinityMask SetPriorityClass InterlockedExchange GetCurrentProcessId
      GetLocalTime GetTimeZoneInformation ReleaseMutex SetConsoleCtrlHandler TlsFree;
      DeviceIoControl for MEMVXD.VXD (find out what the game asks of it)
- [ ] winmm timeGetTime, wsprintfA
- [ ] DirectDraw (software renderer and menus), DirectInput (keyboard, mouse, joystick)
- [ ] CD check: GetVolumeInformationA (label NUKEPC?), GetDriveTypeA, `gRootCD`; where it
      expects `worlds` and `video`
- [ ] Registry: which keys the game reads (RegOpenKeyExA/RegQueryValueExA)

### 2. Sound
- [ ] Miles Sound System (`mss32.dll`): 36 AIL functions in Rust (digital samples, timers)
- [ ] MIDI: AIL_midiOutOpen/send_channel_voice_message to a software synthesizer with the
      game's SoundFont `AUD_IMS1.SF2` (e.g. the rustysynth crate)
- [ ] DirectSoundCreate (the game also uses DirectSound)

### 3. Glide on OpenGL
- [ ] `glide.rs` from future_rust_cop (LoadLibraryA("glide2x.dll") -> our module); check which
      of the ~120 names the game really calls
- [ ] High resolution, widescreen, texture filtering, MSAA as in Future Cop

### 4. Later
- [ ] Videos (`videos.res`: format unknown yet)
- [ ] Gamepads, preferences, fullscreen, Linux
- [ ] Unsupported instructions in discovered code (Aas 204, Iretd 15, Les 6, ...): mostly data
      decoded as code? Check which functions they are in, and whether any runs
- [ ] Differential test against Unicorn (future_rust_cop's `src/bin/difftest.rs`)
