//! `zaqaru-bake`: a linked x86-64 program in, a container module out.
//!
//! This is the pipeline the tests drive, with a command line in front of it:
//! translate every function in the program to wasm, bake the program into an
//! image the kernel can open, and link the two against the kernel and its
//! seam. What comes out is a self-contained `.wasm` that `zaqaru-run` — or
//! any wasmtime embedder that supplies the store imports — can boot.
//!
//! The program becomes `/init`, which is what kisal's boot path opens. It
//! stays a whole ELF inside the image rather than being replaced by its
//! translation: the loader reads its headers, places its segments and
//! applies its relocations exactly as Linux would, and the translated code
//! is reached through the exec map. That is why a patch step exists at all —
//! the bytes in the image and the wasm beside it have to agree about where
//! each function went.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut program: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut root: Option<PathBuf> = None;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-o" | "--output" => {
                output = Some(PathBuf::from(
                    arguments.next().context("`-o` needs a path after it")?,
                ));
            }
            "--root" => {
                root = Some(PathBuf::from(
                    arguments.next().context("`--root` needs a path after it")?,
                ));
            }
            "-h" | "--help" => {
                println!(
                    "usage: zaqaru-bake <program> [-o <container.wasm>] \
                     [--root <directory>]"
                );
                return Ok(());
            }
            other if other.starts_with('-') => bail!("unknown option `{other}`"),
            other => program = Some(PathBuf::from(other)),
        }
    }
    let program = program
        .context("usage: zaqaru-bake <program> [-o <container.wasm>] [--root <directory>]")?;
    let output = output.unwrap_or_else(|| program.with_extension("wasm"));

    // The filesystem the container will have, before the translated files
    // go into it.
    let workspace = tempdir(&output)?;
    let image_root = workspace.join("image");
    std::fs::create_dir_all(&image_root).context("creating the image tree")?;
    match &root {
        Some(source) => copy_tree(source, &image_root)
            .with_context(|| format!("copying {} into the image", source.display()))?,
        None => {
            // A program with no filesystem cannot open a file, and the first
            // thing an ordinary one does is write a temporary. Directories
            // rather than files: what goes in them is the guest's business,
            // and the overlay is what makes them writable.
            for directory in ["tmp", "var", "var/tmp", "home", "dev", "proc", "etc"] {
                std::fs::create_dir_all(image_root.join(directory))
                    .with_context(|| format!("creating /{directory} in the image"))?;
            }
        }
    }
    let tree = baker::tree::Tree::from_directory(&image_root).context("reading the image tree")?;

    let search = root.clone().unwrap_or_else(|| PathBuf::from("/"));
    let baked = baker::bake::container(&program, &search, tree)?;

    if baked.placed.len() > 1 {
        eprintln!(
            "zaqaru-bake: {} files translated:{}",
            baked.placed.len(),
            baked
                .placed
                .iter()
                .map(|(path, base)| format!("\n  {base:#012x}  {path}"))
                .collect::<String>()
        );
    }
    if !baked.refused.is_empty() {
        eprintln!(
            "zaqaru-bake: {} of {} functions were not translated and will stop \
             the container if called",
            baked.refused.len(),
            baked.functions
        );
    }

    let guest = workspace.join("program.wasm.o");
    std::fs::write(&guest, &baked.module).context("writing the translated program")?;
    let image_object = workspace.join("image.wasm.o");
    std::fs::write(&image_object, &baked.image).context("writing the image object")?;
    let top = baked.top;

    let seam = workspace.join("seam.wasm.o");
    std::fs::write(
        &seam,
        zaqaru::seam::build_seam_object().context("building the kernel seam")?,
    )
    .context("writing the seam object")?;

    let objects = vec![
        guest,
        seam,
        image_object,
        staticlib("kisal", "libkisal.a")?,
        staticlib("x87", "libx87.a")?,
    ];
    link(&objects, &output, top)?;
    eprintln!("zaqaru-bake: wrote {}", output.display());
    Ok(())
}

/// Copies a directory tree, so that the program can be placed into a copy of
/// it rather than into the caller's own.
///
/// Symbolic links are followed rather than preserved, which is a simplifying
/// choice this will have to give up when it bakes a real distribution image:
/// a libc's `.so` symlink farm is not decoration. It is enough for a tree
/// someone assembled to run one program.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// A scratch directory beside the output, so that the intermediate objects
/// are somewhere a failed bake can be looked at.
fn tempdir(output: &Path) -> Result<PathBuf> {
    let directory = output.with_extension("bake");
    // Whatever is there, not just a directory: the name is derived from the
    // output's, so it collides with anything a caller happened to put beside
    // it — a log named after the same stem, for instance.
    std::fs::remove_dir_all(&directory).ok();
    std::fs::remove_file(&directory).ok();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    Ok(directory)
}

/// The kernel, and the floating-point unit it cannot do without.
///
/// Built on demand rather than expected: both are workspace crates compiled
/// for a different target than the tool itself, and a target directory of
/// their own is not optional — cargo takes a lock per directory, and this
/// process is often itself running under cargo.
fn staticlib(package: &str, name: &str) -> Result<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("the workspace root")?
        .to_path_buf();
    let target = root.join("target").join(format!("wasm-{package}"));
    let built = target
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(name);
    if !built.exists() {
        eprintln!("zaqaru-bake: building {package} for wasm32");
    }
    let output = Command::new(env!("CARGO"))
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", &target)
        .args([
            "build",
            "-p",
            package,
            "--target",
            "wasm32-unknown-unknown",
            "--release",
        ])
        .output()
        .with_context(|| format!("running cargo to build {package} for wasm32"))?;
    if !output.status.success() {
        bail!(
            "building {package} for wasm32 failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(built)
}

/// The final link.
///
/// `--fatal-warnings` is the recipe rather than decoration: the seam's whole
/// value is that a disagreement about the kernel's shape stops the build
/// instead of becoming a wrong call at run time.
fn link(objects: &[PathBuf], output: &Path, program_top: u64) -> Result<()> {
    let mut arguments: Vec<String> = [
        "--no-entry",
        "--fatal-warnings",
        "--export=cabi_realloc",
        // How a host starts a container. Nothing inside the module calls it,
        // so without this the linker would drop it as unreachable.
        "--export=kisal_boot",
        "--export=x86_slot_of",
        "--export=x86_yield_slot",
        // The host needs the indirect function table to install a
        // continuation's slot, which is how a thread starts from outside.
        "--export-table",
        "--growable-table",
    ]
    .iter()
    .map(|argument| argument.to_string())
    .collect();
    arguments.extend(baker::layout::link_arguments(Some(program_top)));
    arguments.extend(
        objects
            .iter()
            .map(|object| object.to_string_lossy().into_owned()),
    );
    arguments.push("-o".into());
    arguments.push(output.to_string_lossy().into_owned());

    let result = Command::new("wasm-ld")
        .args(&arguments)
        .output()
        .context("running wasm-ld — is lld installed?")?;
    if !result.status.success() {
        bail!(
            "wasm-ld failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(())
}
