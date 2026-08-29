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
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-o" | "--output" => {
                output = Some(PathBuf::from(
                    arguments.next().context("`-o` needs a path after it")?,
                ));
            }
            "-h" | "--help" => {
                println!("usage: zaqaru-bake <program> [-o <container.wasm>]");
                return Ok(());
            }
            other if other.starts_with('-') => bail!("unknown option `{other}`"),
            other => program = Some(PathBuf::from(other)),
        }
    }
    let program = program.context("usage: zaqaru-bake <program> [-o <container.wasm>]")?;
    let output = output.unwrap_or_else(|| program.with_extension("wasm"));

    let bytes =
        std::fs::read(&program).with_context(|| format!("reading {}", program.display()))?;
    let object = zaqaru::reader::ObjectFile::parse(&bytes)
        .with_context(|| format!("reading {} as an ELF program", program.display()))?;
    if object.layout != zaqaru::reader::Layout::Linked {
        bail!(
            "{} is a relocatable object, not a linked program — a container \
             boots a program, so link it first",
            program.display()
        );
    }

    // Where the loader will have placed the program, which is where the
    // module's own data has to start above.
    let top = object
        .segments
        .iter()
        .map(|segment| segment.address + segment.memory_size)
        .max()
        .context("the program has no loadable segments")?;

    // A function that cannot be translated becomes a body that names itself
    // and stops, rather than refusing the whole program: a program is worth
    // running when the path it takes is translated, whatever else it
    // carries. What could not be translated is reported here, because it is
    // a worklist.
    let translation = zaqaru::transpile::Transpiler::new(&object)
        .with_untranslatable(zaqaru::transpile::Untranslatable::Trap)
        .translate()
        .context("translating the program")?;
    if !translation.refused.is_empty() {
        eprintln!(
            "zaqaru-bake: {} of {} functions were not translated and will stop \
             the container if called",
            translation.refused.len(),
            object.functions.len()
        );
    }

    let workspace = tempdir(&output)?;
    let guest = workspace.join("program.wasm.o");
    std::fs::write(&guest, &translation.module).context("writing the translated program")?;

    // The image: the program itself, at the path kisal's boot path opens.
    let root = workspace.join("image");
    std::fs::create_dir_all(&root).context("creating the image tree")?;
    let mut placed = bytes.clone();
    baker::program::apply(&mut placed, &translation.patches)
        .context("patching the program to agree with its translation")?;
    std::fs::write(root.join("init"), &placed).context("placing the program in the image")?;
    let image = baker::object::emit(&baker::bake_directory(&root).context("baking the image")?)
        .context("emitting the image object")?;
    let image_object = workspace.join("image.wasm.o");
    std::fs::write(&image_object, &image).context("writing the image object")?;

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

/// A scratch directory beside the output, so that the intermediate objects
/// are somewhere a failed bake can be looked at.
fn tempdir(output: &Path) -> Result<PathBuf> {
    let directory = output.with_extension("bake");
    if directory.exists() {
        std::fs::remove_dir_all(&directory).ok();
    }
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
