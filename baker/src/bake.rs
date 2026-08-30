//! The bake, as a function: an ELF in, the two objects a container links out.
//!
//! This is the pipeline `zaqaru-bake` puts a command line in front of, and it
//! lives here rather than there so that a test can drive the same code the
//! tool does. A pipeline reachable only through a subprocess is a pipeline
//! nothing tests directly.
//!
//! What it does, in order: find every file the program loads, assign each a
//! base, translate the lot as one unit, route each jump-table patch back to
//! the file it came from, and place every file in the image at the path a
//! loader will look for it under.
//!
//! The single-file case is not a special case. A static executable is a
//! closure of one, translated at the addresses it states, and every step
//! below runs for it unchanged.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::dynamic::Module;

/// What a bake produced.
pub struct Bake {
    /// The translated code, as a relocatable wasm object.
    pub module: Vec<u8>,
    /// The image — blob and index — as a second one.
    pub image: Vec<u8>,
    /// The highest address any module's segments reach, which is what the
    /// link has to place the module's own data above.
    pub top: u64,
    /// Every file translated, with the base it was placed at, in load order.
    pub placed: Vec<(String, u64)>,
    /// Functions that could not be translated. A worklist, not a failure:
    /// a real binary carries code for processors this one is not.
    pub refused: Vec<zaqaru::transpile::Refusal>,
    /// How many functions were translated in total.
    pub functions: usize,
}

/// Bakes a program and everything it loads into a container's two objects.
///
/// `search` is the tree dependencies are resolved against — the host's `/`
/// for a program on this machine, or an unpacked image root. `tree` is the
/// filesystem the container will have; the translated files are placed into
/// it, so anything already there is kept.
pub fn container(program: &Path, search: &Path, tree: crate::tree::Tree) -> Result<Bake> {
    container_with_command(program, search, tree, &[])
}

/// The same, with the command line the container boots with.
///
/// Empty leaves kisal at its default invocation, which is the program under
/// the one name the boot path knows. A program that reads `argv[0]` to
/// decide what to be — busybox is the whole family — needs this.
pub fn container_with_command(
    program: &Path,
    search: &Path,
    mut tree: crate::tree::Tree,
    argv: &[Vec<u8>],
) -> Result<Bake> {
    let modules = crate::dynamic::closure(program, search)
        .with_context(|| format!("finding what {} loads", program.display()))?;
    let bases = crate::dynamic::assign_bases(&modules).context("placing the modules")?;

    let mut inputs = Vec::new();
    for module in &modules {
        let base = bases[&module.path];
        let object = zaqaru::reader::ObjectFile::parse_at(&module.bytes, base)
            .with_context(|| format!("reading {} as an ELF", module.path))?;
        if object.layout != zaqaru::reader::Layout::Linked {
            bail!(
                "{} is a relocatable object, not a linked file — a container \
                 boots a program, so link it first",
                module.path
            );
        }
        inputs.push((module.path.clone(), object));
    }

    let object = match inputs.len() {
        // One file needs no merge, but it does need its name: the patch
        // routing below finds a module by it, and an unnamed module matches
        // nothing — which would silently drop every jump-table patch, the
        // exact defect the worklog records from M6.
        1 => {
            let (name, mut object) = inputs.pop().expect("just checked");
            object.modules[0].name = name;
            object
        }
        _ => zaqaru::reader::ObjectFile::merge(inputs).context("placing the modules together")?,
    };

    let top = object
        .segments
        .iter()
        .map(|segment| segment.address + segment.memory_size)
        .max()
        .context("the program has no loadable segments")?;

    // A function that cannot be translated becomes a body that names itself
    // and stops, rather than refusing the whole program: a program is worth
    // running when the path it takes is translated, whatever else it
    // carries.
    let translation = zaqaru::transpile::Transpiler::new(&object)
        .with_untranslatable(zaqaru::transpile::Untranslatable::Trap)
        .translate()
        .context("translating the program")?;

    for module in &modules {
        let base = bases[&module.path];
        let extent = object
            .modules
            .iter()
            .find(|placed| placed.name == module.path)
            .map(|placed| placed.base..placed.top)
            .with_context(|| format!("{} was translated under no name", module.path))?;
        let mine: Vec<zaqaru::transpile::Patch> = translation
            .patches
            .iter()
            .filter(|patch| extent.contains(&patch.address))
            .cloned()
            .collect();
        let mut placed = module.bytes.clone();
        crate::program::apply_at(&mut placed, &mine, base)
            .with_context(|| format!("patching {} to agree with its translation", module.path))?;
        place(&mut tree, module, placed, base)?;
    }

    let image = crate::object::emit(
        &crate::bake_tree_with_command(&tree, argv).context("baking the image")?,
    )
    .context("emitting the image object")?;

    Ok(Bake {
        module: translation.module,
        image,
        top,
        placed: modules
            .iter()
            .map(|module| (module.path.clone(), bases[&module.path]))
            .collect(),
        refused: translation.refused,
        functions: object.functions.len(),
    })
}

/// Puts one translated module into the image at the path the loader will
/// look for it under, carrying the base the bake placed it at.
///
/// The base travels with the file rather than in a table beside it, because
/// the question it answers is asked about a file: `mmap` of *this* inode has
/// to return *this* address.
fn place(
    tree: &mut crate::tree::Tree,
    module: &Module,
    bytes: Vec<u8>,
    base: u64,
) -> Result<()> {
    let path = &module.path;
    let base = u32::try_from(base).with_context(|| {
        format!("{path} was placed at an address outside a 32-bit linear memory")
    })?;
    let meta = crate::tree::Meta {
        mode: kisal::image::file_type::REGULAR | 0o755,
        prelink_base: Some(base),
        ..Default::default()
    };
    let node = tree.add(meta, crate::tree::Body::Regular(bytes));
    // Every name the file answers to, all pointing at one node — which is
    // one inode, one prelink base, and one translation. A distribution's
    // `libz.so.1` and `libz.so.1.3` are the same library and must be the
    // same library here.
    for name in std::iter::once(path).chain(module.aliases.iter()) {
        let (directory, leaf) = tree
            .place(name.as_bytes())
            .with_context(|| format!("making room for {name} in the image"))?
            .with_context(|| format!("{name} is not a path a file can go at"))?;
        tree.link(directory, &leaf, node)
            .with_context(|| format!("placing {name} in the image"))?;
    }
    Ok(())
}
