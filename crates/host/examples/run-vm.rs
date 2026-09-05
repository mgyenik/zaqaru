//! Runs a container module under wasmtime, and reports what it did.
//!
//! ```text
//! cargo run --release --example run-vm -- <container.wasm>
//! ```
//!
//! The host boundary is the design's, unchanged: two `ll-store` imports and
//! `cabi_realloc`, with the guest's console, its clock and its entropy all
//! arriving as *resources* under `/iso` rather than as syscalls. Nothing
//! here knows what the guest's instruction set is.

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let module = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: run-vm <container.wasm>");
        std::process::exit(2);
    }));

    let mut mounts = host::store::MountTable::new();
    mounts.mount(&[b"iso", b"console"], Box::new(host::store::Sink::new()));
    mounts.mount(&[b"iso", b"log"], Box::new(host::store::Sink::new()));
    mounts.mount(&[b"iso", b"random"], Box::new(host::store::Sink::new()));
    // A fixed seed, because the design's claim is that the same container
    // with the same inputs produces the same run. A container given no
    // `/iso/random` mount has no entropy at all, which is the capability
    // model rather than an oversight — CPython stops during
    // pre-initialisation without one, by name.
    mounts
        .write(
            &[
                b"iso".to_vec(),
                b"random".to_vec(),
                b"bytes".to_vec(),
                b"32".to_vec(),
            ],
            &[0x5a; 32],
        )
        .map_err(anyhow::Error::msg)?;

    let compiling = std::time::Instant::now();
    let bytes = std::fs::read(&module)?;
    let mut container = host::Container::instantiate(&bytes, mounts)?;
    eprintln!(
        "compiled {:.1} MB in {:.2}s",
        bytes.len() as f64 / 1e6,
        compiling.elapsed().as_secs_f64()
    );

    let running = std::time::Instant::now();
    let status = container.call::<(), i32>("zaqaru_boot", ());
    let elapsed = running.elapsed().as_secs_f64();
    for stream in ["stdout", "stderr"] {
        let written = container
            .mounts()
            .read(&[b"iso".to_vec(), b"console".to_vec(), stream.as_bytes().to_vec()])
            .ok()
            .flatten()
            .unwrap_or_default();
        if !written.is_empty() {
            print!("{}", String::from_utf8_lossy(&written));
        }
    }
    let log = container
        .mounts()
        .read(&[b"iso".to_vec(), b"log".to_vec(), b"error".to_vec()])
        .ok()
        .flatten()
        .unwrap_or_default();
    if !log.is_empty() {
        eprintln!("kernel log: {}", String::from_utf8_lossy(&log));
    }
    eprintln!("ran in {elapsed:.2}s");
    match status {
        Ok(status) => std::process::exit(status),
        Err(error) => {
            eprintln!("the container did not finish: {error:?}");
            std::process::exit(1);
        }
    }
}
