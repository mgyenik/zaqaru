//! Builds the guest archive for wasm32 and hands its path to `include_bytes!`.
//!
//! The archive is the kernel, the interpreter and the FPU compiled for the
//! module; it is the same in every container, so the tool carries it. The
//! build goes to its own target directory (see `bake::Guest::build`), and
//! reruns when any of the guest's sources change.

fn main() -> anyhow::Result<()> {
    for crate_name in ["guest", "kernel", "cpu", "x87"] {
        println!("cargo:rerun-if-changed=../{crate_name}/src");
        println!("cargo:rerun-if-changed=../{crate_name}/Cargo.toml");
    }
    println!("cargo:rerun-if-changed=../kernel/build.rs");
    let guest = bake::Guest::build()?;
    println!("cargo:rustc-env=ZAQARU_GUEST_ARCHIVE={}", guest.archive().display());
    Ok(())
}
