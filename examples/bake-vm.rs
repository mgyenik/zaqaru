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
    // `--tier1` compiles the image's code at bake time — see
    // `docs/tier1-plan.md` — and `--budget <MB>` bounds how much. Off, the
    // module still carries the (empty) table the engine looks bytes up in.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut tier1 = false;
    let mut verify = false;
    let mut budget: usize = 32 << 20;
    // `--only lo-hi` compiles only blocks whose address is in the range: a
    // bisection tool, not a feature.
    let mut only: Option<(u64, u64)> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        match raw[index].as_str() {
            "--tier1" => tier1 = true,
            "--verify" => verify = true,
            "--budget" if index + 1 < raw.len() => {
                budget = raw[index + 1].parse::<usize>().unwrap_or(32) << 20;
                index += 1;
            }
            "--only" if index + 1 < raw.len() => {
                let (lo, hi) = raw[index + 1].split_once('-').expect("lo-hi");
                only = Some((
                    u64::from_str_radix(lo.trim_start_matches("0x"), 16).expect("hex"),
                    u64::from_str_radix(hi.trim_start_matches("0x"), 16).expect("hex"),
                ));
                index += 1;
            }
            other => positional.push(other.to_string()),
        }
        index += 1;
    }
    let mut arguments = positional.into_iter();
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
    // The tree is read once and used twice: baked into the image, and — with
    // `--tier1` — swept for the code the bake compiles beside it.
    let tree = match root.is_dir() {
        true => baker::tree::Tree::from_directory(&root)?,
        false => baker::layers::tree_from_archive(&std::fs::read(&root)?)?,
    };
    let baked = if root.is_dir() {
        let argv = match given.is_empty() {
            true => vec![b"/init".to_vec()],
            false => given,
        };
        eprintln!("baking the directory {}", root.display());
        baker::bake_tree_with_command(&tree, &argv)?
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

    // Tier 1: the compiled blocks, or an empty table, linked either way.
    let compiled = output.with_extension("tier1.o");
    let built = match tier1 {
        true => {
            let sweeping = std::time::Instant::now();
            let mut candidates = sweep_tree(&tree);
            if let Some((lo, hi)) = only {
                candidates.retain(|candidate| candidate.address >= lo && candidate.address < hi);
            }
            let built = zaqaru::tier1::build(&candidates, budget);
            eprintln!(
                "tier 1: {} blocks from the sweep, {} compiled in {} regions ({:.1} MB of code, {} instructions, {} deferred), {} mostly deferred and left interpreted, {} past the budget, in {:.2}s",
                candidates.len(),
                built.members,
                built.functions,
                built.code_bytes as f64 / 1e6,
                built.instructions,
                built.deferred,
                built.mostly_deferred,
                built.left_out,
                sweeping.elapsed().as_secs_f64()
            );
            built.object
        }
        false => zaqaru::tier1::object::empty(),
    };
    std::fs::write(&compiled, &built)?;

    let mut objects = vec![image.clone(), compiled.clone()];
    for (crate_name, library) in [
        ("kisal", "libkisal.a"),
        ("targum", "libtargum.a"),
        ("x87", "libx87.a"),
    ] {
        objects.push(staticlib(crate_name, library, verify)?);
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
    let _ = std::fs::remove_file(&compiled);
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
fn staticlib(crate_name: &str, library: &str, verify: bool) -> anyhow::Result<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // A separate target directory for the verifying build, so the two do
    // not keep rebuilding each other.
    let suffix = if verify { "-verify" } else { "" };
    let target = root.join("target").join(format!("wasm-{crate_name}{suffix}"));
    let mut command = Command::new(env!("CARGO"));
    command
        .current_dir(root)
        .env("CARGO_TARGET_DIR", &target)
        .args([
            "build",
            "-p",
            crate_name,
            "--target",
            "wasm32-unknown-unknown",
            "--release",
        ]);
    if verify && crate_name != "x87" {
        command.args(["--features", "targum/verify"]);
    }
    let output = command.output()?;
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

/// Every ELF in the tree, swept, in the order the bake should spend its
/// budget: the runtime and anything under `/usr/local` first, the system's
/// shared libraries next, everything else last.
fn sweep_tree(tree: &baker::tree::Tree) -> Vec<zaqaru::tier1::Candidate> {
    use baker::tree::{Body, ROOT};
    let mut elfs: Vec<(u8, String, &[u8])> = Vec::new();
    let mut stack: Vec<(usize, String)> = vec![(ROOT, String::new())];
    while let Some((id, path)) = stack.pop() {
        match &tree.node(id).body {
            Body::Directory(entries) => {
                for (name, child) in entries {
                    let name = String::from_utf8_lossy(name).into_owned();
                    stack.push((*child, format!("{path}/{name}")));
                }
            }
            Body::Regular(bytes) if bytes.starts_with(b"\x7fELF") => {
                elfs.push((rank(&path), path, bytes.as_slice()));
            }
            _ => {}
        }
    }
    elfs.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    let mut candidates = Vec::new();
    for (module, (_, path, bytes)) in elfs.into_iter().enumerate() {
        match zaqaru::tier1::sweep(bytes) {
            Ok(mut found) => {
                eprintln!("  swept {path}: {} blocks", found.len());
                // Tag every block with its file, so region formation keeps
                // members of one file together: their file addresses collide
                // with another's, but they load at their own base.
                for candidate in &mut found {
                    candidate.module = module as u32;
                }
                candidates.extend(found);
            }
            Err(error) => eprintln!("  skipped {path}: {error}"),
        }
    }
    candidates
}

/// Where an ELF stands in the budget's order. Evidence the bake can read
/// without running anything; `docs/tier1-plan.md` §10.
fn rank(path: &str) -> u8 {
    let name = path.rsplit('/').next().unwrap_or(path);
    if path.starts_with("/usr/local/")
        || name.starts_with("ld-linux")
        || name.starts_with("libc.so")
        || name == "nginx"
        || name.starts_with("python3")
        || path.contains("/lib-dynload/")
    {
        return 0;
    }
    if path.contains("/x86_64-linux-gnu/") || path.starts_with("/lib/") || path.starts_with("/usr/lib/") {
        return 1;
    }
    2
}
