# nuclear_rust_strike

A native port of **Nuclear Strike** (Electronic Arts, 1997, PC) in Rust, in the making. The
game's own program is recompiled: the player's `NUKEPC.EXE` is decoded with iced-x86, every
function is lifted to Cranelift IR and compiled to native code when the game starts, and the
Windows, DirectX, Glide and Miles Sound System functions it calls are implemented in Rust.
The recompiler comes from [future_rust_cop](https://github.com/RadiatorTwo/future_rust_cop) (Future Cop: L.A.P.D.).

No game code or data is part of this repository: you need your own copy of the game.

## State

Early: the game is installed from the CD image, compiled, and runs its start-up up to its
window. See PLAN.md.

## Trying it

1. Put an image of your Nuclear Strike CD (`.bin`/`.cue` or `.iso`) into the repository folder.
2. `cargo run --release`: the game is installed from the image into `game_files/NUKEPC`
   (as the CD's installer would), compiled and started.
