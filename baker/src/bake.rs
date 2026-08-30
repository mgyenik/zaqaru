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
    tree: crate::tree::Tree,
    argv: &[Vec<u8>],
) -> Result<Bake> {
    container_with_invocation(program, search, tree, argv, &[])
}

/// The same, with the environment as well.
///
/// The environment decides which *path* a program takes, not merely what it
/// prints. CPython with no `HOME` falls through `expanduser` into `getpwuid`,
/// which is glibc's NSS, which probes nscd over a socket — so a container
/// booted with an empty environment exercises a surface the run it will be
/// diffed against never touches. See `kisal::image`'s module header for the
/// measurement.
pub fn container_with_invocation(
    program: &Path,
    search: &Path,
    mut tree: crate::tree::Tree,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
) -> Result<Bake> {
    let mut modules = crate::dynamic::closure(program, search)
        .with_context(|| format!("finding what {} loads", program.display()))?;
    // And every other ELF in the image, because `dlopen` names files no
    // closure reaches. The unit of translation is the image, not the
    // executable's closure: an extension module is linked *from* by nobody,
    // so `PT_INTERP` and `DT_NEEDED` will never lead to one, and `import
    // json` needs one on the first line of any real script.
    modules.extend(
        crate::dynamic::sweep(&tree, &modules).context("sweeping the image for ELFs")?,
    );
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
        // Checkpoint-resume, always. The build plan has said the container
        // pipeline builds this way since M6 and the code did not, which
        // nothing noticed because nothing yet blocks: it is M7's scheduler
        // that needs a thread's frames to be a chain of resume IDs, and M7
        // is not built. What brought the debt forward is `setjmp`, whose
        // saved return address *is* one of those IDs — a materialized
        // continuation stored by code that has no idea that is what it is
        // doing — and which is the sentinel instead when resume is off. See
        // `container-plan.md`'s setjmp section.
        //
        // The cost is a second body per function, measured rather than
        // assumed; the worklog entry of 2026-08-30 carries the numbers.
        .with_resume(true)
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
        &crate::bake_tree_with_invocation(&tree, argv, envp).context("baking the image")?,
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

    // A file the image already holds is updated where it is, rather than
    // replaced by a new node under the same name. The difference is not
    // bookkeeping: a swept module is *found* in the tree, and a library
    // routinely has several names there — a hardlink, or the symlink farm a
    // libc arrives as. Linking one name to a fresh node would leave the
    // others pointing at the old one, which is then a second copy of the
    // same library, unpatched, untranslated, and reachable by name.
    //
    // A *regular file* only. `Tree::resolve` walks entries without following
    // symlinks, and a closure module is routinely named through one — an
    // executable's `PT_INTERP` says `/lib64/ld-linux-x86-64.so.2`, which on
    // every modern distribution is a link into `/lib/x86_64-linux-gnu`.
    // Overwriting that node's body leaves its *mode* saying symlink, so the
    // image holds an inode of one type carrying another's contents and the
    // loader looks for a file it cannot find. A symlink falls through to the
    // path below, which replaces the name outright, as it always did.
    if let Some(existing) = tree.resolve(path.as_bytes())
        && matches!(tree.node(existing).body, crate::tree::Body::Regular(_))
    {
        let node = tree.node_mut(existing);
        node.body = crate::tree::Body::Regular(bytes);
        node.meta.prelink_base = Some(base);
        return Ok(());
    }

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
