//! Bakes a directory or an OCI image into a container that carries an
//! interpreter.
//!
//! The whole bake, and what is *not* in it is the point: no translation, no
//! discovery, no witnesses, no jump tables, no prelink, no resume bodies. A
//! filesystem becomes an image, the image becomes one object file, and the
//! object is linked with three staticlibs that were compiled once and are
//! the same in every container.
//!
//! ```text
//! cargo run --release --example bake-vm -- <root|image.tar> <out.wasm> [argv...]
//! ```
//!
//! A `docker save` tarball is read whole: the layer stack in order, with its
//! whiteouts and opaque directories applied, *and* the config — entrypoint,
//! command and environment — so the container starts the way the image says
//! it should. Arguments given here override the image's command, which is
//! what `docker run image cmd...` means.
//!
//! A directory has no config to read, so it runs `/init` and whatever
//! arguments follow.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let (root, output) = match (arguments.next(), arguments.next()) {
        (Some(root), Some(output)) => (PathBuf::from(root), PathBuf::from(output)),
        _ => {
            eprintln!("usage: bake-vm <root> <output.wasm> [argv...]");
            std::process::exit(2);
        }
    };
    // The invocation is a fact about the container, recorded in the image:
    // the same module booted twice runs the same program the same way,
    // which is what makes a run reproducible.
    let given: Vec<Vec<u8>> = arguments.map(|a| a.into_bytes()).collect();

    let started = std::time::Instant::now();
    let baked = if root.is_dir() {
        let argv = match given.is_empty() {
            true => vec![b"/init".to_vec()],
            false => given,
        };
        eprintln!("baking the directory {}", root.display());
        baker::bake_tree_with_command(&baker::tree::Tree::from_directory(&root)?, &argv)?
    } else {
        let archive = std::fs::read(&root)?;
        let (image, invocation) = baker::bake_archive_as_configured(&archive, &given)?;
        anyhow::ensure!(
            !invocation.argv.is_empty(),
            "the image says no entrypoint and no command, and none was given \
             on the command line, so there is nothing to run"
        );
        eprintln!(
            "image says: {}{}",
            invocation
                .argv
                .iter()
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect::<Vec<_>>()
                .join(" "),
            match invocation.environment.is_empty() {
                true => String::new(),
                false => format!("  ({} environment entries)", invocation.environment.len()),
            }
        );
        if !invocation.working_directory.is_empty() {
            eprintln!(
                "  working directory {:?}",
                String::from_utf8_lossy(&invocation.working_directory)
            );
        }
        image
    };
    let object = baker::object::emit(&baked)?;
    let image = output.with_extension("image.o");
    std::fs::write(&image, &object)?;
    eprintln!(
        "image: {:.1} MB in {:.2}s",
        object.len() as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );

    let mut objects = vec![image.clone()];
    for (crate_name, library) in [
        ("kisal", "libkisal.a"),
        ("targum", "libtargum.a"),
        ("x87", "libx87.a"),
    ] {
        objects.push(staticlib(crate_name, library)?);
    }

    let linking = std::time::Instant::now();
    let mut command = Command::new("wasm-ld");
    command
        .args(&objects)
        .args([
            "--no-entry",
            "--fatal-warnings",
            "--export=cabi_realloc",
            "--export=targum_boot",
            "--export-memory",
            // Room below the module's own data for a program that states
            // its own addresses. A position-independent one is placed above
            // it and does not need this, but a static executable's segments
            // are wherever its ELF says.
            "--global-base=67108864",
        ])
        .arg("-o")
        .arg(&output);
    let linked = command.output()?;
    anyhow::ensure!(
        linked.status.success(),
        "wasm-ld failed:\n{}",
        String::from_utf8_lossy(&linked.stderr)
    );
    let _ = std::fs::remove_file(&image);
    eprintln!(
        "module: {:.1} MB, linked in {:.2}s",
        std::fs::metadata(&output)?.len() as f64 / 1e6,
        linking.elapsed().as_secs_f64()
    );
    eprintln!("total: {:.2}s", started.elapsed().as_secs_f64());
    Ok(())
}

/// Builds one workspace crate for wasm32, released.
///
/// These are the same three objects in every container — the engine does not
/// depend on the image — so a real bake would take them off a shelf. Built
/// here because a tool with no shelf is a tool that works.
fn staticlib(crate_name: &str, library: &str) -> anyhow::Result<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = root.join("target").join(format!("wasm-{crate_name}"));
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .env("CARGO_TARGET_DIR", &target)
        .args([
            "build",
            "-p",
            crate_name,
            "--target",
            "wasm32-unknown-unknown",
            "--release",
        ])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "building {crate_name} for wasm32 failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(target
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(library))
}
