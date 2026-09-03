//! Tier 1 at bake time: blocks found in an image's ELFs, compiled to wasm
//! functions the engine attaches to the blocks it decodes.
//!
//! The design is `docs/tier1-plan.md`; the run-time half is
//! `targum::tier1`. What lives here runs natively, in the bake:
//!
//! - [`sweep`] finds blocks in an ELF without running it — a descent from
//!   its symbols and branch targets, then a walk over what the descent left
//!   uncovered — shaped exactly as the block cache would decode them, so
//!   that a block the engine decodes at run time is one the sweep saw.
//! - [`compile`] turns one block into one wasm function that honours the
//!   contract in `targum::tier1`: the `Quick` lowering compiled, a helper
//!   for what it declines, registers and the flags record in locals, and
//!   an exit to the interpreter for anything it will not do.
//! - [`object`] gathers the functions, their table slots and the lookup
//!   table into one relocatable object the bake links beside the image.
//!
//! Everything is keyed by the block's bytes, never its address: a compiled
//! block is written as `entry` plus deltas, so it attaches wherever the
//! same bytes are mapped, and a block the sweep guessed wrong — bytes that
//! were never code — is a function nobody enters.

pub mod compile;
pub mod object;
pub mod region;
pub mod sweep;

pub use compile::Helpers;
pub use object::build;
pub use sweep::{Candidate, sweep};
