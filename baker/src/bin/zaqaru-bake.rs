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
    let mut argv: Vec<Vec<u8>> = Vec::new();
    let mut envp: Vec<Vec<u8>> = Vec::new();
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        // Everything after `--` is the guest's command line, not ours.
        if argument == "--" {
            argv = arguments.map(|rest| rest.as_bytes().to_vec()).collect();
            break;
        }
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
            "-e" | "--env" => {
                let entry = arguments
                    .next()
                    .context("`--env` needs a `NAME=value` after it")?;
                if !entry.contains('=') {
                    bail!("`--env {entry}` is not a `NAME=value`");
                }
                envp.push(entry.as_bytes().to_vec());
            }
            "-h" | "--help" => {
                println!(
                    "usage: zaqaru-bake <program> [-o <container.wasm>] \
                     [--root <directory>] [--env NAME=value]... [-- <argv>...]\n\
                     \n\
                     The program is the host path of a linked x86-64 ELF. It \
                     goes before the options; everything after `--` is the \
                     guest's own command line, so a program named only there \
                     is `argv[0]` and not the thing to bake.\n\
                     \n\
                     `--root` is the filesystem the container will have *and* \
                     the tree its libraries are resolved against — a dynamic \
                     program's interpreter and every `DT_NEEDED` must be \
                     inside it, because the bake does not look at the host's \
                     `/` once a root is given. Without `--root` the host's `/` \
                     is searched and the container gets a few empty \
                     directories."
                );
                return Ok(());
            }
            other if other.starts_with('-') => bail!("unknown option `{other}`"),
            other => program = Some(PathBuf::from(other)),
        }
    }
    let program = match (program, argv.first()) {
        (Some(program), _) => program,
        // The mistake this catches is the one that gets made: the program
        // named after `--`, where the *guest's* command line starts. Saying
        // "usage" about a command line that plainly contains a program is
        // baffling, so say which one was read as what.
        (None, Some(first)) => bail!(
            "no program was given. `{}` came after `--`, which begins the \
             guest's own command line — the program to bake goes before the \
             options, and is usually repeated after `--` as `argv[0]`:\n  \
             zaqaru-bake {0} -o out.wasm -- {0} …",
            String::from_utf8_lossy(first)
        ),
        (None, None) => bail!(
            "usage: zaqaru-bake <program> [-o <container.wasm>] \
             [--root <directory>] [--env NAME=value]... [-- <argv>...]"
        ),
    };
    let output = output.unwrap_or_else(|| program.with_extension("wasm"));

    // The filesystem the container will have, before the translated files
    // go into it.
    //
    // A `--root` tree is read where it is. An earlier version copied it into
    // the workspace first, and the copy was both pointless and lossy: the
    // translated files are placed into the *in-memory* tree, so nothing was
    // ever written back to it — while `std::fs::copy` follows symlinks, so
    // an image built from a real root filesystem arrived with every symlink
    // replaced by a copy of its target, and with a dangling one refusing the
    // bake outright. A root filesystem is a symlink farm (`/lib`, every
    // versioned `.so`, and CPython's own `config-*/libpython3.12.so`, which
    // dangles by design), and `Tree::from_directory` already reads symlinks,
    // hardlinks, device nodes and fifos faithfully. The copy stood between
    // it and the truth.
    let workspace = tempdir(&output)?;
    let tree = match &root {
        Some(source) => baker::tree::Tree::from_directory(source)
            .with_context(|| format!("reading {} as the image tree", source.display()))?,
        None => {
            // A program with no filesystem cannot open a file, and the first
            // thing an ordinary one does is write a temporary. Directories
            // rather than files: what goes in them is the guest's business,
            // and the overlay is what makes them writable.
            let image_root = workspace.join("image");
            for directory in ["tmp", "var", "var/tmp", "home", "dev", "proc", "etc"] {
                std::fs::create_dir_all(image_root.join(directory))
                    .with_context(|| format!("creating /{directory} in the image"))?;
            }
            baker::tree::Tree::from_directory(&image_root).context("reading the image tree")?
        }
    };

    let search = root.clone().unwrap_or_else(|| PathBuf::from("/"));
    let baked = baker::bake::container_with_invocation(&program, &search, tree, &argv, &envp)?;
    if !argv.is_empty() {
        eprintln!(
            "zaqaru-bake: command line: {}",
            argv.iter()
                .map(|argument| String::from_utf8_lossy(argument).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

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
    if !envp.is_empty() {
        eprintln!(
            "zaqaru-bake: environment: {}",
            envp.iter()
                .map(|entry| String::from_utf8_lossy(entry).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
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
    let mut command = Command::new(env!("CARGO"));
    command
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", &target)
        .args([
            "build",
            "-p",
            package,
            "--target",
            "wasm32-unknown-unknown",
            "--release",
        ]);
    // A measurement hook: `ZAQARU_ENGINE_FEATURES=bytecode` builds the engine
    // staticlib with that cargo feature, so a container can be baked with the
    // bytecode accelerator on and its rate compared against a default bake.
    // Only the engine crate carries these features, so only its build gets
    // them.
    if package == "kisal"
        && let Ok(features) = std::env::var("ZAQARU_ENGINE_FEATURES")
        && !features.is_empty()
    {
        command.args(["--features", &features]);
    }
    let output = command
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
