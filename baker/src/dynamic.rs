//! What a dynamic program needs before it can run: its interpreter, and the
//! transitive closure of its libraries.
//!
//! A static executable is one file and the bake translates it. A dynamic one
//! is a *set* of files — the executable, `ld.so`, and every library between
//! them — and every byte of code in the set has to be translated ahead of
//! time, because there is no way to translate one at run time. Finding the
//! set is this module's whole job.
//!
//! It is deliberately a bake-time answer to a run-time question. On Linux,
//! `ld.so` finds `libc.so.6` by walking `DT_RUNPATH`, `/etc/ld.so.cache` and
//! a default search path, at start-up, on the real filesystem. Here the same
//! walk happens once, over the tree that is about to become the image, and
//! what it finds is what gets translated. The loader still does its own
//! search at run time and still has to find the same files — which it will,
//! because they are in the image at the paths it looks in.
//!
//! Two things this deliberately does not do. It does not read
//! `/etc/ld.so.cache`, because the cache is a *cache*: it names files the
//! search path also names, and a bake that trusted it would depend on the
//! host's cache being right about the host's filesystem. And it does not
//! resolve symbols — nothing here cares which library defines `printf`, only
//! which files are loaded, because translation is per file.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use kisal::exec::{Kind, Program};

/// What one ELF's dynamic section asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    /// `PT_INTERP`: the loader this program must be started through.
    pub interpreter: Option<Vec<u8>>,
    /// `DT_NEEDED`, in order: the libraries to load, by the name recorded.
    pub needed: Vec<Vec<u8>>,
    /// `DT_SONAME`: what this file calls itself, which is the name others
    /// will have recorded for it.
    pub soname: Option<Vec<u8>>,
    /// `DT_RUNPATH` and `DT_RPATH`, split on `:`. Searched before the
    /// default directories, as the loader searches them.
    pub search: Vec<Vec<u8>>,
    /// Whether `DF_1_NOW` or `DF_BIND_NOW` is already set. The bake needs
    /// eager binding — `_dl_runtime_resolve` is the one function that must
    /// never run — so a file without it is one the bake has to change.
    pub binds_now: bool,
}

const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_SONAME: u64 = 14;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;
const DT_FLAGS: u64 = 30;
const DT_FLAGS_1: u64 = 0x6fff_fffb;
const DF_BIND_NOW: u64 = 0x8;
const DF_1_NOW: u64 = 0x1;

/// Reads one file's dynamic requirements.
///
/// The addresses in `PT_DYNAMIC` are virtual, so every one of them has to be
/// turned back into a file offset through the load segments — which is the
/// same conversion `baker::program` does for patches, and for the same
/// reason: at bake time there is no loader and nothing has been placed.
pub fn needs(bytes: &[u8]) -> Result<Needs> {
    let program = Program::parse_at(bytes, 0).map_err(|error| {
        let mut message = String::new();
        error.message(&mut message);
        anyhow::anyhow!("{message}")
    })?;

    let mut needs = Needs {
        interpreter: program.interpreter_path(bytes).map(<[u8]>::to_vec),
        ..Needs::default()
    };
    let Some((dynamic, size)) = program.dynamic else {
        return Ok(needs);
    };

    let at = |address: u64| -> Option<usize> {
        program.loads.iter().find_map(|load| {
            (address >= load.address && address < load.address + load.file_size)
                .then(|| (load.offset + (address - load.address)) as usize)
        })
    };
    let start = at(dynamic).context("`PT_DYNAMIC` is in no loaded segment")?;
    let end = start
        .checked_add(size as usize)
        .filter(|end| *end <= bytes.len())
        .context("`PT_DYNAMIC` runs past the end of the file")?;

    // Two passes: an entry naming a string is an offset into `DT_STRTAB`,
    // and the entry that says where the string table is may come after it.
    let entries: Vec<(u64, u64)> = bytes[start..end]
        .chunks_exact(16)
        .map(|entry| {
            (
                u64::from_le_bytes(entry[..8].try_into().expect("eight bytes")),
                u64::from_le_bytes(entry[8..].try_into().expect("eight bytes")),
            )
        })
        .take_while(|(tag, _)| *tag != DT_NULL)
        .collect();

    let strings = entries
        .iter()
        .find_map(|(tag, value)| (*tag == DT_STRTAB).then_some(*value))
        .and_then(at);
    let string = |offset: u64| -> Option<Vec<u8>> {
        let start = strings?.checked_add(offset as usize)?;
        let rest = bytes.get(start..)?;
        let end = rest.iter().position(|byte| *byte == 0)?;
        Some(rest[..end].to_vec())
    };

    for (tag, value) in &entries {
        match *tag {
            DT_NEEDED => {
                if let Some(name) = string(*value) {
                    needs.needed.push(name);
                }
            }
            DT_SONAME => needs.soname = string(*value),
            DT_RUNPATH | DT_RPATH => {
                if let Some(paths) = string(*value) {
                    needs
                        .search
                        .extend(paths.split(|byte| *byte == b':').map(<[u8]>::to_vec));
                }
            }
            DT_FLAGS => needs.binds_now |= value & DF_BIND_NOW != 0,
            DT_FLAGS_1 => needs.binds_now |= value & DF_1_NOW != 0,
            _ => {}
        }
    }
    Ok(needs)
}

/// Where the loader looks when `DT_RUNPATH` does not say.
///
/// The multiarch directories first, which is where a Debian-family system
/// puts everything, then the plain ones. This stands in for
/// `/etc/ld.so.cache` — see the module header for why the cache is not read.
const DEFAULT_SEARCH: [&str; 6] = [
    "/lib/x86_64-linux-gnu",
    "/usr/lib/x86_64-linux-gnu",
    "/lib64",
    "/usr/lib64",
    "/lib",
    "/usr/lib",
];

/// One file the bake will translate and place.
#[derive(Clone, Debug)]
pub struct Module {
    /// Where it goes in the image, which is where the loader will look for
    /// it — so it is the path the loader would have used, not a name of our
    /// choosing.
    ///
    /// **The name it was found by, not the file it resolves to.** A
    /// distribution's libraries are a symlink farm: `DT_NEEDED` says
    /// `libz.so.1` and the file is `libz.so.1.3`, and an image holding only
    /// the second is an image where the loader's search fails. Which it
    /// does silently as far as any syscall is concerned — the `openat`
    /// returns `ENOENT` exactly as it would for a library that really is
    /// absent.
    pub path: String,
    /// Other names the same file answers to, placed as further links to one
    /// inode. The canonical path, when it differs, so that a library naming
    /// the real file finds it too.
    pub aliases: Vec<String>,
    pub bytes: Vec<u8>,
    /// Whether it is the executable, the interpreter, or a library. Only
    /// the first two are named by the boot path.
    pub role: Role,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Executable,
    Interpreter,
    Library,
}

/// The whole set of files a program loads, in load order: the executable
/// first, then its interpreter, then the libraries breadth-first.
///
/// `root` is the tree the search happens in — the host's `/` for a bake of a
/// program on this machine, or an unpacked image root. A library named by a
/// program and not found there is an error and not a warning: a bake that
/// quietly left one out would produce a container that dies at load time
/// with a message about a file, when the truth is about the bake.
pub fn closure(program: &Path, root: &Path) -> Result<Vec<Module>> {
    let bytes = std::fs::read(program)
        .with_context(|| format!("reading {}", program.display()))?;
    let first = needs(&bytes)?;

    let mut modules = vec![Module {
        // The path a container's boot opens. The executable is the one file
        // whose name in the image is ours to choose, because nothing looks
        // it up by name.
        path: "/init".to_string(),
        aliases: Vec::new(),
        bytes,
        role: Role::Executable,
    }];
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();

    if let Some(interpreter) = &first.interpreter {
        let path = String::from_utf8_lossy(interpreter).into_owned();
        let (real, bytes) = read_under(root, &path)
            .with_context(|| format!("the interpreter `{path}` this program names"))?;
        placed.insert(path.clone());
        placed.insert(real.clone());
        let inner = needs(&bytes)?;
        queue.push((Vec::new(), inner.search.clone()));
        modules.push(Module {
            aliases: alias(&path, &real),
            path,
            bytes,
            role: Role::Interpreter,
        });
    }

    // Breadth-first, so the order is the order a loader would open them in,
    // which is what makes a bake's module list readable against an `ldd`.
    let mut pending: Vec<(Vec<u8>, Vec<Vec<u8>>)> = first
        .needed
        .iter()
        .map(|name| (name.clone(), first.search.clone()))
        .collect();
    queue.clear();
    while !pending.is_empty() {
        let mut next = Vec::new();
        for (name, search) in pending.drain(..) {
            let name = String::from_utf8_lossy(&name).into_owned();
            let (path, real, bytes) = find(root, &name, &search)
                .with_context(|| format!("the library `{name}` this program needs"))?;
            // Deduplicated by the *file*, not by the name: two libraries
            // naming `libz.so.1` and `libz.so.1.3` want one translation at
            // one base, and translating the same code twice would put two
            // sets of exec-map entries at two addresses for one library.
            if !placed.insert(real.clone()) {
                continue;
            }
            placed.insert(path.clone());
            let inner = needs(&bytes)?;
            next.extend(
                inner
                    .needed
                    .iter()
                    .map(|name| (name.clone(), inner.search.clone())),
            );
            modules.push(Module {
                aliases: alias(&path, &real),
                path,
                bytes,
                role: Role::Library,
            });
        }
        pending = next;
    }
    Ok(modules)
}

/// Reads a guest-absolute path out of a root, following symlinks *within*
/// that root and reporting the path the loader will use.
///
/// A libc is a symlink farm — `/lib64/ld-linux-x86-64.so.2` points at
/// `../lib/x86_64-linux-gnu/ld-linux-x86-64.so.2` — and both names have to
/// work in the image, because the executable's `PT_INTERP` names one and
/// `DT_NEEDED` may name the other.
fn read_under(root: &Path, path: &str) -> Result<(String, Vec<u8>)> {
    let host = under(root, path);
    let bytes = std::fs::read(&host).with_context(|| format!("reading {}", host.display()))?;
    // The real path, so that two names for one file are recognised as one
    // file rather than translated twice at two different bases.
    let real = std::fs::canonicalize(&host)
        .ok()
        .and_then(|real| {
            real.strip_prefix(std::fs::canonicalize(root).ok()?)
                .ok()
                .map(|rest| format!("/{}", rest.display()))
        })
        .unwrap_or_else(|| path.to_string());
    Ok((real, bytes))
}

/// The canonical path as a second name, when it is a different one.
fn alias(path: &str, real: &str) -> Vec<String> {
    match path == real {
        true => Vec::new(),
        false => std::vec![real.to_string()],
    }
}

fn under(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// Finds a library by the name `DT_NEEDED` recorded, in the order the loader
/// searches: `DT_RUNPATH` first, then the default directories.
fn find(root: &Path, name: &str, search: &[Vec<u8>]) -> Result<(String, String, Vec<u8>)> {
    // A name with a slash in it is a path, not a name to search for.
    if name.contains('/') {
        let (real, bytes) = read_under(root, name)?;
        return Ok((name.to_string(), real, bytes));
    }
    let directories = search
        .iter()
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .chain(DEFAULT_SEARCH.iter().map(|entry| entry.to_string()));
    let mut looked = Vec::new();
    for directory in directories {
        if directory.is_empty() {
            continue;
        }
        let path = format!("{}/{name}", directory.trim_end_matches('/'));
        if under(root, &path).exists() {
            let (real, bytes) = read_under(root, &path)?;
            // The searched path first: it is the name the loader will look
            // for at run time, and so the name the image must answer to.
            return Ok((path, real, bytes));
        }
        looked.push(path);
    }
    bail!(
        "`{name}` is in none of the directories the loader searches: {}",
        looked.join(", ")
    )
}

/// Where each module is placed, keyed by its path in the image.
///
/// Assigned once, at bake time, and never again — that is the whole of
/// "prelink at bake". `mmap` of one of these files returns the base it was
/// given, so the loader asks for "anywhere", gets the same address every
/// run, and its `MAP_FIXED` carving of the segments lands exactly where the
/// translation assumed.
pub fn assign_bases(modules: &[Module]) -> Result<BTreeMap<String, u64>> {
    let mut bases = BTreeMap::new();
    let mut next = crate::layout::DYNAMIC_BASE;
    for module in modules {
        let program = Program::parse_at(&module.bytes, 0).map_err(|error| {
            let mut message = String::new();
            error.message(&mut message);
            anyhow::anyhow!("{}: {message}", module.path)
        })?;
        match program.kind {
            // It states its own addresses, so there is nothing to assign
            // and nothing that may be moved.
            Kind::Fixed => {
                if program.base() < crate::layout::MINIMUM_FIXED_ADDRESS {
                    bail!(
                        "{} is linked at {:#x}, below {:#x} — that low, the \
                         program's text sits where its own integer constants \
                         sit, and discovery cannot tell an address from a \
                         number. Relink it, or build it position-independent \
                         so the bake can place it",
                        module.path,
                        program.base(),
                        crate::layout::MINIMUM_FIXED_ADDRESS
                    );
                }
                bases.insert(module.path.clone(), 0);
            }
            Kind::PositionIndependent => {
                bases.insert(module.path.clone(), next);
                next = (next + program.top()).next_multiple_of(crate::layout::MODULE_ALIGNMENT);
            }
        }
    }
    Ok(bases)
}
