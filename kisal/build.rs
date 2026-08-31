//! Compiles the vDSO, exactly as Linux compiles its own.
//!
//! The alternative is emitting an ELF from Rust by hand, and the reason not
//! to is symbol versioning: glibc looks the vDSO's symbols up *by version*,
//! and one whose `.gnu.version_d` is wrong — or missing — is one glibc
//! silently ignores in favour of the syscall. That is a failure that looks
//! exactly like success, and hand-assembling the tables that prevent it
//! would be writing a linker.
//!
//! So the host toolchain builds it, at build time, from
//! `kisal/vdso/vdso.c` and its version script. It targets x86-64 whatever
//! this crate is being built for, because it is *guest* code — the module
//! carries it as data and the interpreter executes it.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=vdso/vdso.c");
    println!("cargo:rerun-if-changed=vdso/vdso.lds");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let object = out.join("vdso.so");
    let compiler = std::env::var("CC").unwrap_or_else(|_| String::from("cc"));

    let built = Command::new(&compiler)
        .args([
            "-shared",
            "-fPIC",
            "-O2",
            // The vDSO is executed by the interpreter, so it stays to plain
            // integer work and carries nothing the engine would have to
            // unwind through.
            "-fno-stack-protector",
            "-fno-asynchronous-unwind-tables",
            "-fcf-protection=none",
            // Freestanding: it calls nothing and is linked against nothing.
            "-nostdlib",
            "-m64",
            "-Wl,-T,vdso/vdso.lds",
            // A build id would be bytes that change every build for no
            // reason a guest can see.
            "-Wl,--build-id=none",
            // The name a program reading `/proc/self/maps` expects, and the
            // one glibc records.
            "-Wl,-soname=linux-vdso.so.1",
            // Both hash tables, because which one a libc uses is its choice
            // and neither costs anything at this size.
            "-Wl,--hash-style=both",
        ])
        .arg("-o")
        .arg(&object)
        .arg("vdso/vdso.c")
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "kisal needs a C compiler to build its vDSO, and running \
                 `{compiler}` failed: {error}. Set `CC` to one, or see \
                 `kisal/vdso/vdso.c` for what it is for."
            )
        });
    // The linker warns that the image has one RWX segment. It does, and it
    // does not matter: kisal maps the vDSO read-and-execute whatever the
    // header says, because the page is the kernel's and a guest has no
    // business writing it.
    let complaints: Vec<&str> = std::str::from_utf8(&built.stderr)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.contains("LOAD segment with RWX"))
        .collect();
    if !built.status.success() || !complaints.is_empty() {
        panic!(
            "building the vDSO failed:\n{}",
            match built.status.success() {
                true => complaints.join("\n"),
                false => String::from_utf8_lossy(&built.stderr).into_owned(),
            }
        );
    }
    println!("cargo:rustc-env=KISAL_VDSO={}", object.display());
}
