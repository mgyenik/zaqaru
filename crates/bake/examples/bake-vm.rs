//! Bakes a directory or an OCI image into a container.
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

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    let (root, output) = match (arguments.next(), arguments.next()) {
        (Some(root), Some(output)) => (PathBuf::from(root), PathBuf::from(output)),
        _ => {
            eprintln!("usage: bake-vm <root|image.tar> <output.wasm> [argv...]");
            std::process::exit(2);
        }
    };
    // The invocation is a fact about the container, recorded in the image:
    // the same module booted twice runs the same program the same way,
    // which is what makes a run reproducible.
    let given: Vec<Vec<u8>> = arguments.map(|a| a.into_bytes()).collect();

    let started = std::time::Instant::now();
    let baked = if root.is_dir() {
        let tree = image::tree::Tree::from_directory(&root)?;
        let argv = match given.is_empty() {
            true => vec![b"/init".to_vec()],
            false => given,
        };
        eprintln!("baking the directory {}", root.display());
        image::bake_tree_with_command(&tree, &argv)?
    } else {
        let archive = std::fs::read(&root)?;
        let (baked, invocation) = image::bake_archive_as_configured(&archive, &given)?;
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
        baked
    };
    eprintln!(
        "image: {:.1} MB in {:.2}s",
        (baked.blob.len() + baked.index.len()) as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );

    let guest = bake::Guest::build()?;
    let linking = std::time::Instant::now();
    bake::link(&baked, &guest, &output)?;
    eprintln!(
        "module: {:.1} MB, linked in {:.2}s",
        std::fs::metadata(&output)?.len() as f64 / 1e6,
        linking.elapsed().as_secs_f64()
    );
    eprintln!("total: {:.2}s", started.elapsed().as_secs_f64());
    Ok(())
}
