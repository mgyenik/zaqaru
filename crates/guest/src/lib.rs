//! The guest side of a container module, as one staticlib.
//!
//! A container module is this archive plus an image object, linked. The
//! archive carries the kernel, the interpreter and the FPU, and adds the two
//! things only the module itself can define: the export the host calls to
//! run the container ([`boot`]), and the store the kernel reaches the host
//! through — two imports, lowered onto the canonical ABI ([`abi`]).
//!
//! Only the wire shapes ([`wire`]) exist natively, so that the contract with
//! the host is checked by ordinary unit tests; the rest is a wasm32 target
//! and compiles to an empty archive anywhere else, which is what lets the
//! workspace build as a whole.

#[cfg(target_arch = "wasm32")]
pub mod abi;
#[cfg(target_arch = "wasm32")]
pub mod boot;
pub mod wire;
