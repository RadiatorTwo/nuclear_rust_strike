//! Nuclear Strike (1997) recompiled to native code with Cranelift.
//!
//! The player's NUKEPC.EXE is decoded (iced-x86), every function is lifted to Cranelift IR
//! and compiled to host code; game pointers stay 32-bit in a 4 GB guest address space.

pub mod cv;
pub mod discover;
pub mod esa;
pub mod image;
pub mod iso;
pub mod jit;
pub mod lift;
pub mod platform;
pub mod rt;
pub mod setup;
pub mod smc;
