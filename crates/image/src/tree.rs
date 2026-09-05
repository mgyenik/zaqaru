//! The image tree: what a bake actually consumes.
//!
//! There are two ways an image arrives — a rootfs directory on disk, and a
//! `docker save` layer stack — and they agree about almost nothing. One reads
//! metadata from `statx` and the other from tar headers; one has real
//! hardlinks and the other has `LNK` records naming an earlier entry; one has
//! no whiteouts at all and the other's whole semantics are whiteouts.
//!
//! What they agree about is the answer: a directory tree of files with POSIX
//! metadata. So that is what they both produce, and the packager consumes only
//! this. Two front ends writing inodes directly would be two copies of the
//! inode-construction rules, and a rule that exists twice is a rule that will
//! be true in one place.
//!
//! Held in memory whole. That is what the directory path already did — it
//! reads every file into a `Vec` before any offset is known — and it is what
//! laying layers over each other requires anyway.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use kernel::file::MAX_NAME;
use kernel::image::file_type;

use crate::xattr;

/// A node's index in the tree. Two names sharing one index are a hardlink,
/// which is the only way the tree can say so.
pub type NodeId = usize;

/// The root is always the first node, so a tree is never empty.
pub const ROOT: NodeId = 0;

/// POSIX metadata, from whichever source.
#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    /// For a device node. Zero for everything else.
    pub rdev: u64,
    /// Tar's symbolic owner names. A directory on disk records numeric ids
    /// and nothing else, so the directory path leaves these empty — there is
    /// nothing to preserve, and the host's `/etc/passwd` is not the image's.
    pub uname: Vec<u8>,
    pub gname: Vec<u8>,
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// What a node holds.
#[derive(Clone, Debug)]
pub enum Body {
    /// Sorted by name, which is what makes lookup a binary search in the
    /// baked index and `getdents64`'s order stable across bakes.
    Directory(BTreeMap<Vec<u8>, NodeId>),
    Regular(Vec<u8>),
    Symlink(Vec<u8>),
    /// A fifo, socket or device node: no contents, and `rdev` in the
    /// metadata for the last of those.
    Special,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub meta: Meta,
    pub body: Body,
}

impl Node {
    pub fn is_directory(&self) -> bool {
        matches!(self.body, Body::Directory(_))
    }
}

#[derive(Debug)]
pub struct Tree {
    nodes: Vec<Node>,
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    /// A tree with nothing in it but a root directory.
    pub fn new() -> Self {
        Self {
            nodes: vec![Node {
                meta: Meta {
                    mode: file_type::DIRECTORY | 0o755,
                    ..Meta::default()
                },
                body: Body::Directory(BTreeMap::new()),
            }],
        }
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Adds a node with no name yet. Naming it is [`Self::link`].
    pub fn add(&mut self, meta: Meta, body: Body) -> NodeId {
        self.nodes.push(Node { meta, body });
        self.nodes.len() - 1
    }

    /// Names `child` inside `directory`, replacing whatever was there.
    ///
    /// Replacing rather than merging is what a later layer does to an earlier
    /// one, and it is what a second `add` of the same path means. A directory
    /// replaced by a directory is the one case that *is* a merge, and it is
    /// handled by the caller that knows which it wants.
    pub fn link(&mut self, directory: NodeId, name: &[u8], child: NodeId) -> Result<()> {
        if name.len() > MAX_NAME {
            bail!(
                "`{}` is {} bytes, past the {MAX_NAME}-byte limit a directory \
                 entry can carry",
                String::from_utf8_lossy(name),
                name.len()
            );
        }
        if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
            bail!(
                "`{}` is not a name a directory entry can carry",
                String::from_utf8_lossy(name)
            );
        }
        match &mut self.nodes[directory].body {
            Body::Directory(entries) => {
                entries.insert(name.to_vec(), child);
                Ok(())
            }
            _ => bail!(
                "`{}` cannot be placed: its parent is not a directory",
                String::from_utf8_lossy(name)
            ),
        }
    }

    pub fn unlink(&mut self, directory: NodeId, name: &[u8]) {
        if let Body::Directory(entries) = &mut self.nodes[directory].body {
            entries.remove(name);
        }
    }

    /// The child of `directory` with this name, if it has one.
    pub fn lookup(&self, directory: NodeId, name: &[u8]) -> Option<NodeId> {
        match &self.nodes[directory].body {
            Body::Directory(entries) => entries.get(name).copied(),
            _ => None,
        }
    }

    /// Walks a slash-separated path from the root, without following
    /// symlinks — a layer's entries name paths in the layer, and a symlink
    /// in an earlier layer is a file the later one is free to replace.
    pub fn resolve(&self, path: &[u8]) -> Option<NodeId> {
        let mut current = ROOT;
        for component in path.split(|byte| *byte == b'/') {
            if component.is_empty() || component == b"." {
                continue;
            }
            current = self.lookup(current, component)?;
        }
        Some(current)
    }

    /// Walks a path, creating any missing directory along the way.
    ///
    /// Tar archives are not required to carry an entry for every parent, and
    /// real ones routinely do not — a layer that changes one file lists that
    /// file and nothing else. The implied directories are created with the
    /// mode a later explicit entry will overwrite if the archive has one.
    pub fn resolve_or_create(&mut self, path: &[u8]) -> Result<NodeId> {
        let mut current = ROOT;
        for component in path.split(|byte| *byte == b'/') {
            if component.is_empty() || component == b"." {
                continue;
            }
            if component == b".." {
                bail!(
                    "the archive names `{}`, which climbs out of the image",
                    String::from_utf8_lossy(path)
                );
            }
            current = match self.lookup(current, component) {
                Some(existing) if self.nodes[existing].is_directory() => existing,
                // A path component that is not a directory in an earlier
                // layer *becomes* one: `/usr/lib` may be a symlink in the
                // base and a real directory in a layer above it.
                _ => {
                    let created = self.add(
                        Meta {
                            mode: file_type::DIRECTORY | 0o755,
                            ..Meta::default()
                        },
                        Body::Directory(BTreeMap::new()),
                    );
                    self.link(current, component, created)?;
                    created
                }
            };
        }
        Ok(current)
    }

    /// Splits a path into its parent directory and the final name, creating
    /// the parents. `None` for a path that names the root itself.
    pub fn place(&mut self, path: &[u8]) -> Result<Option<(NodeId, Vec<u8>)>> {
        let trimmed: Vec<&[u8]> = path
            .split(|byte| *byte == b'/')
            .filter(|component| !component.is_empty() && *component != b".")
            .collect();
        let Some((name, parents)) = trimmed.split_last() else {
            return Ok(None);
        };
        if *name == b".." || parents.contains(&b"..".as_slice()) {
            bail!(
                "the archive names `{}`, which climbs out of the image",
                String::from_utf8_lossy(path)
            );
        }
        let mut directory = ROOT;
        for component in parents {
            directory = match self.lookup(directory, component) {
                Some(existing) if self.nodes[existing].is_directory() => existing,
                _ => {
                    let created = self.add(
                        Meta {
                            mode: file_type::DIRECTORY | 0o755,
                            ..Meta::default()
                        },
                        Body::Directory(BTreeMap::new()),
                    );
                    self.link(directory, component, created)?;
                    created
                }
            };
        }
        Ok(Some((directory, name.to_vec())))
    }

    /// Reads a rootfs directory into a tree.
    pub fn from_directory(root: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(root)
            .with_context(|| format!("reading {}", root.display()))?;
        if !metadata.is_dir() {
            bail!("{} is not a directory", root.display());
        }
        let mut tree = Self::new();
        let mut hardlinks: HashMap<(u64, u64), NodeId> = HashMap::new();
        tree.nodes[ROOT].meta = read_meta(root, &metadata)?;
        tree.read_into(ROOT, root, &mut hardlinks)?;
        Ok(tree)
    }

    fn read_into(
        &mut self,
        directory: NodeId,
        path: &Path,
        hardlinks: &mut HashMap<(u64, u64), NodeId>,
    ) -> Result<()> {
        let mut children: Vec<PathBuf> = std::fs::read_dir(path)
            .with_context(|| format!("listing {}", path.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("listing {}", path.display()))?;
        // The map sorts, but reading in a stable order keeps node numbering —
        // and therefore the whole index — reproducible.
        children.sort();

        for child in children {
            let name = child
                .file_name()
                .expect("a directory entry has a name")
                .as_bytes()
                .to_vec();
            let metadata = std::fs::symlink_metadata(&child)
                .with_context(|| format!("reading {}", child.display()))?;
            let kind = metadata.mode() & file_type::MASK;

            // A hardlink is the *same* file, and the tree says so by naming
            // one node twice. Getting this wrong would report `nlink` as 1
            // for every name and make `python3` and `python3.11` two files.
            if kind != file_type::DIRECTORY && metadata.nlink() > 1 {
                let key = (metadata.dev(), metadata.ino());
                if let Some(existing) = hardlinks.get(&key) {
                    self.link(directory, &name, *existing)
                        .with_context(|| format!("placing {}", child.display()))?;
                    continue;
                }
                let node = self.read_node(&child, &metadata, kind, hardlinks)?;
                hardlinks.insert(key, node);
                self.link(directory, &name, node)
                    .with_context(|| format!("placing {}", child.display()))?;
                continue;
            }

            let node = self.read_node(&child, &metadata, kind, hardlinks)?;
            self.link(directory, &name, node)
                .with_context(|| format!("placing {}", child.display()))?;
        }
        Ok(())
    }

    fn read_node(
        &mut self,
        path: &Path,
        metadata: &std::fs::Metadata,
        kind: u32,
        hardlinks: &mut HashMap<(u64, u64), NodeId>,
    ) -> Result<NodeId> {
        let meta = read_meta(path, metadata)?;
        let body = match kind {
            file_type::DIRECTORY => Body::Directory(BTreeMap::new()),
            file_type::REGULAR => Body::Regular(
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
            ),
            file_type::SYMLINK => Body::Symlink(
                std::fs::read_link(path)
                    .with_context(|| format!("reading the link {}", path.display()))?
                    .as_os_str()
                    .as_bytes()
                    .to_vec(),
            ),
            // Device nodes, fifos and sockets carry no contents. Base images
            // ship `/dev` entries and the format keeps their `rdev`, whether
            // or not the kernel ever honours one.
            _ => Body::Special,
        };
        let node = self.add(meta, body);
        if kind == file_type::DIRECTORY {
            self.read_into(node, path, hardlinks)?;
        }
        Ok(node)
    }
}

fn read_meta(path: &Path, metadata: &std::fs::Metadata) -> Result<Meta> {
    Ok(Meta {
        mode: metadata.mode(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mtime_sec: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec() as u32,
        rdev: if matches!(
            metadata.mode() & file_type::MASK,
            file_type::CHARACTER | file_type::BLOCK
        ) {
            metadata.rdev()
        } else {
            0
        },
        uname: Vec::new(),
        gname: Vec::new(),
        xattrs: xattr::read(path)
            .with_context(|| format!("reading extended attributes of {}", path.display()))?,
    })
}
