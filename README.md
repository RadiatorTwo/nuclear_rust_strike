# nuclear_rust_strike

A native port of **Nuclear Strike** (Electronic Arts, 1997, PC) in Rust. The game's own
program is recompiled: the player's `NUKEPC.EXE` is decoded with [iced-x86], every function
is lifted to [Cranelift] IR and compiled to native code when the game starts, and the
Windows, DirectX, 3Dfx Glide and Miles Sound System functions it calls are implemented in
Rust (Glide drawn with OpenGL). The recompiler comes from
[future_rust_cop](https://github.com/RadiatorTwo/future_rust_cop) (Future Cop: L.A.P.D.).

No game code or data is part of this repository or its releases: you need your own copy of
the game.

## Playing

1. Download `nstrike-...-windows-x64.zip` from the [releases] (Windows, x86-64, a graphics
   card with OpenGL 3.3) and unpack it.
2. Put an image of your Nuclear Strike CD (PC version, `.bin`/`.cue` or `.iso`) next to
   `nstrike.exe` or into its `game_files` folder.
3. Start `nstrike.exe`. The first start installs the game from the image into
   `game_files/NUKEPC` (as the CD's installer would, the videos too: this takes a moment
   before a window opens) and compiles it (a second or two; kept in
   `game_files/NUKEPC/compiled.bin`). Later starts are quick; the image is not needed any more.

The US version is supported (`NUKEPC.EXE` with SHA-256 `af308478...26daad`); with any other,
or without one, nstrike says so. Its messages go to `nstrike.log` next to it.

What is new compared to the original:
- Any resolution, widescreen (Hor+), fullscreen (Alt+Enter), anti-aliasing, texture
  filtering
- Frame rates up to unlimited with the game's logic unchanged (60 a second), pictures
  in between interpolated
- Modern effects: ambient occlusion, sun shadows, dynamic lights from fire and explosions,
  lit smoke, bloom, heat shimmer, sparks and debris, a depth buffer (the game's own sorting
  makes mistakes)
- Gamepads alongside the keyboard, with their own bindings; analog flying, menus by pad.
  Default (Xbox layout): left stick forward, reverse and jink, right stick turns, RT chain
  gun, RB Hydras, LB Hellfires, B wingtips, LT set target, A cargo / rescue, Back map,
  Y camera, X radar zoom, R3 HUD, L3 pause, Start menu
- The text before and after a mission written four times as fast
- Runs on modern Windows: no 3Dfx card, DirectX 5 or Win95 driver needed

Our own menu, over the game:
- **F10** settings: window, fullscreen, frame rate, vsync, anti-aliasing, texture filter,
  widescreen
- **F11** effects: each one on or off, shadow quality
- **F12** gamepad bindings (Enter / A: press a pad control to bind it, Delete / X unbinds)

The settings are saved in `game_files/nstrike.toml`, the gamepad's bindings in
`game_files/gamepad.toml`. The game's own settings (keyboard, sound, difficulty) and saves
stay in its menus and `nukepc.sav`.

## Building

```
cargo build --release
cargo run --release -- path/to/NUKEPC.EXE
```

Without a path it uses `game_files` (next to nstrike.exe, else in the current folder) as
above, installed from a CD image there, next to nstrike.exe or in the current folder.
`cargo test --release` needs no game files. How it works and what is open: `CLAUDE.md` and
`PLAN.md`.

[iced-x86]: https://github.com/icedland/iced
[Cranelift]: https://cranelift.dev
[releases]: https://github.com/RadiatorTwo/nuclear_rust_strike/releases
