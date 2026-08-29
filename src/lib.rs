//! zaqaru — a native-code-to-wasm transpiler.
//!
//! The pipeline, in dependency order:
//!
//! ```text
//! ELF .o ──(reader)────► sections + symbols + relocations
//!        ──(lifter)────► per-function basic-block CFG, symbolic operands
//!        ──(structurer)► wasm structured control flow
//!        ──(emitter)───► relocatable wasm object
//! ```
//!
//! See `docs/design.md` for the rationale behind each stage.

pub mod abi;
pub mod cfg;
pub mod dump;
pub mod discover;
pub mod eh_frame;
pub mod emitter;
pub mod jump_table;
pub mod lifter;
pub mod machine;
pub mod reader;
pub mod seam;
pub mod structurer;
pub mod thunks;
pub mod translate;
pub mod transpile;
pub mod wasm_reader;
