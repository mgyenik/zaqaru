//! The baker: an image tree flattened into the two data segments a container
//! module carries.
//!
//! The output is a blob of file contents and a packed index over it, in the
//! format `kisal::image` defines — defined *there*, by the thing that reads
//! it, so that a writer with its own idea of the layout cannot exist.
//!
//! What the baker preserves is deliberately more than what kisal honours.
//! Ownership, the setuid, setgid and sticky bits, timestamps, extended
//! attributes: all of it survives the bake, and whether the kernel ever acts
//! on any of it is a separate and open question. Total preservation is what
//! keeps that question open, because nothing downstream can enforce a bit the
//! bake discarded.
//!
//! The index also carries *symbolic* owner names, and the directory path
//! leaves them empty — a directory on disk records numeric ids and nothing
//! else, so there is nothing to preserve. They are filled by the `docker
//! save` path, where tar's `uname`/`gname` records carry them.

pub mod bake;
pub mod dynamic;
pub mod json;
pub mod layers;
pub mod layout;
pub mod object;
pub mod program;
pub mod tar;
pub mod tree;
pub mod xattr;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use kisal::file::MAX_NAME;
use kisal::image::{self, HEADER_SIZE, INODE_SIZE, Inode, directory_entry_type, inode_flags};

/// Contents are packed at this alignment normally.
const CONTENT_ALIGNMENT: usize = 16;
/// Files the baker identifies as mapping candidates get page alignment
/// instead, so their blob offset is congruent with the address a mapping
/// would place them at. This serves the zero-copy aliasing optimisation,
/// which is designed, flagged and off — v0 copies every file mapping. Doing
/// it now costs a little padding on a few files; doing it later would mean
/// re-baking every image.
const PAGE_ALIGNMENT: usize = 4096;

/// The four-byte entry count in front of a directory's dirent array.
const DIRENT_COUNT_PREFIX: usize = 4;

/// A finished bake: the two segments, ready to become an object.
pub struct Image {
    pub blob: Vec<u8>,
    pub index: Vec<u8>,
}

/// Flattens a directory tree.
///
/// The tarball path — a `docker save` layer stack, with its whiteouts and
/// opaque-directory markers — reaches the same baker through the same tree;
/// a layer stack flattens *into* one, and this is what turns one into an
/// image.
pub fn bake_directory(root: &Path) -> Result<Image> {
    bake_tree(&tree::Tree::from_directory(root)?)
}

/// Flattens a `docker save` archive: its layer stack, in order, with the
/// whiteouts and opaque-directory markers applied.
pub fn bake_archive(archive: &[u8]) -> Result<Image> {
    bake_tree(&layers::tree_from_archive(archive)?)
}

/// Flattens an image tree, whatever produced it.
pub fn bake_tree(tree: &tree::Tree) -> Result<Image> {
    let mut builder = Builder::default();
    let root = builder.add_node(tree, tree::ROOT)?;
    builder.finish(root)
}

/// An image with nothing in it but a root directory.
///
/// Not a placeholder: it is what a container that carries no files of its own
/// links, and it exercises the same paths as any other bake — a root inode,
/// an empty dirent array, a header whose regions are all zero-length.
pub fn bake_empty() -> Image {
    bake_tree(&tree::Tree::new()).expect("an image with one empty directory fits in any index")
}

/// An inode under construction, before offsets are known.
struct Pending {
    inode: Inode,
    /// For a regular file, the bytes; for a directory, its entries.
    contents: Contents,
}

enum Contents {
    None,
    Regular(Vec<u8>),
    /// Sorted by name, which is what makes lookup a binary search and what
    /// makes `getdents64`'s order stable across bakes.
    Directory(BTreeMap<Vec<u8>, (u32, u8)>),
}

#[derive(Default)]
struct Builder {
    inodes: Vec<Pending>,
    /// Parallel to `inodes`: the base a translated ELF was placed at, and
    /// zero for everything else. Kept beside the inode rather than in it
    /// because the record is exactly full and because a prelink base is a
    /// fact about a handful of files, not about every one.
    bases: Vec<u32>,
    /// Byte strings, deduplicated. Names repeat heavily across a rootfs.
    strings: Vec<u8>,
    interned: HashMap<Vec<u8>, u32>,
    /// Extended-attribute blocks, one per inode that has any.
    xattrs: Vec<u8>,
    /// Which inode a tree node became, so that a node named twice — a
    /// hardlink — becomes one record with two names.
    by_node: HashMap<tree::NodeId, u32>,
}

impl Builder {
    /// Adds a tree node and everything under it, returning its inode number.
    ///
    /// A node named twice — a hardlink — is added once: the map from node to
    /// inode is what carries that, and without it `nlink` would read 1 for
    /// every name and `python3` and `python3.11` would be two files.
    fn add_node(&mut self, tree: &tree::Tree, id: tree::NodeId) -> Result<u32> {
        if let Some(existing) = self.by_node.get(&id) {
            return Ok(*existing);
        }
        let node = tree.node(id);
        let contents = match &node.body {
            tree::Body::Directory(_) => Contents::Directory(BTreeMap::new()),
            tree::Body::Regular(bytes) => Contents::Regular(bytes.clone()),
            tree::Body::Symlink(_) | tree::Body::Special => Contents::None,
        };
        let number = self.reserve(&node.meta, contents)?;
        self.by_node.insert(id, number);

        match &node.body {
            tree::Body::Symlink(target) => {
                let reference = self.intern(target);
                self.inodes[number as usize].inode.payload = reference as u64;
                self.inodes[number as usize].inode.size = target.len() as u64;
            }
            tree::Body::Special => {
                self.inodes[number as usize].inode.payload = node.meta.rdev;
            }
            tree::Body::Regular(_) => {}
            tree::Body::Directory(entries) => {
                for (name, child) in entries {
                    // Refused at the bake rather than trusted at the read. A
                    // tar archive can carry a longer name than any filesystem
                    // will create, and `getdents64` has a fixed record to fit
                    // it into.
                    if name.len() > MAX_NAME {
                        bail!(
                            "`{}` is {} bytes, past the {MAX_NAME}-byte limit a \
                             directory entry can carry",
                            String::from_utf8_lossy(name),
                            name.len()
                        );
                    }
                    let child_number = self.add_node(tree, *child)?;
                    // Interned here, where the name enters the image.
                    // `finish` reads the reference back out to write the
                    // entry, so a name that reached an entry without being
                    // interned would have no place in the strings region to
                    // point at.
                    self.intern(name);
                    let entry_type = directory_entry_type::of_mode(
                        self.inodes[child_number as usize].inode.mode,
                    );
                    match &mut self.inodes[number as usize].contents {
                        Contents::Directory(entries) => {
                            entries.insert(name.clone(), (child_number, entry_type));
                        }
                        _ => unreachable!("just reserved as a directory"),
                    }
                }
            }
        }
        Ok(number)
    }

    fn reserve(&mut self, meta: &tree::Meta, contents: Contents) -> Result<u32> {
        let xattr_ref = self.add_xattrs(&meta.xattrs);

        let mut flags = 0;
        if let Contents::Regular(bytes) = &contents
            && bytes.starts_with(b"\x7fELF")
        {
            flags |= inode_flags::MMAP_ALIGNED;
        }
        // The bake translated this file and chose where it goes. The flag is
        // the cheap test — `mmap` consults the prelink records only for an
        // inode carrying it — and it is also what makes mapping an
        // *untranslated* ELF executable a loud error rather than a hang.
        if meta.prelink_base.is_some() {
            flags |= inode_flags::EXEC_TRANSPILED;
        }

        let uname_ref = self.intern(&meta.uname);
        let gname_ref = self.intern(&meta.gname);

        let number = self.inodes.len() as u32;
        self.bases.push(meta.prelink_base.unwrap_or(0));
        self.inodes.push(Pending {
            inode: Inode {
                mode: meta.mode,
                uid: meta.uid,
                gid: meta.gid,
                // Filled in once every name is known; a hardlink's twin may
                // not have been added yet.
                nlink: 0,
                size: 0,
                mtime_sec: meta.mtime_sec,
                mtime_nsec: meta.mtime_nsec,
                xattr_ref,
                payload: 0,
                uname_ref,
                gname_ref,
                flags,
                // Filled in by `finish`, which is the first point at which
                // every directory's children are known.
                parent: 0,
            },
            contents,
        });
        Ok(number)
    }

    fn add_xattrs(&mut self, attributes: &[(Vec<u8>, Vec<u8>)]) -> u32 {
        if attributes.is_empty() {
            return 0;
        }
        // Offset zero means "none", so no real block may live there.
        if self.xattrs.is_empty() {
            self.xattrs.extend_from_slice(&0u32.to_le_bytes());
        }
        let reference = self.xattrs.len() as u32;
        self.xattrs
            .extend_from_slice(&(attributes.len() as u32).to_le_bytes());
        // Two passes: interning may reallocate `self.strings`, and the
        // references have to be known before any is written.
        let references: Vec<(u32, u32)> = attributes
            .iter()
            .map(|(name, value)| (self.intern(name), self.intern(value)))
            .collect();
        for (name, value) in references {
            self.xattrs.extend_from_slice(&name.to_le_bytes());
            self.xattrs.extend_from_slice(&value.to_le_bytes());
        }
        reference
    }

    /// Adds a byte string, returning where it lives. Deduplicated, because a
    /// rootfs repeats names relentlessly.
    fn intern(&mut self, bytes: &[u8]) -> u32 {
        if let Some(existing) = self.interned.get(bytes) {
            return *existing;
        }
        // The empty string sits at zero, so a zero reference reads as empty
        // rather than as whatever happened to be first.
        if self.strings.is_empty() {
            self.strings.extend_from_slice(&0u16.to_le_bytes());
            self.interned.insert(Vec::new(), 0);
            if bytes.is_empty() {
                return 0;
            }
        }
        let reference = self.strings.len() as u32;
        // A `u16` length in the index, so a longer string cannot be recorded
        // at all — silently truncating one to `len mod 65536` would give a
        // 64 KiB extended attribute a recorded length of zero.
        let length = u16::try_from(bytes.len()).unwrap_or_else(|_| {
            panic!(
                "a byte string of {} bytes does not fit the index's 16-bit \
                 length; the caller must refuse it before it gets here",
                bytes.len()
            )
        });
        self.strings.extend_from_slice(&length.to_le_bytes());
        self.strings.extend_from_slice(bytes);
        self.interned.insert(bytes.to_vec(), reference);
        reference
    }

    fn finish(mut self, root: u32) -> Result<Image> {
        // `nlink` is the number of names pointing at a record, which is only
        // knowable once every name exists. A directory's is two plus its
        // subdirectories, exactly as POSIX counts `.` and `..`.
        let mut names = vec![0u32; self.inodes.len()];
        let mut subdirectories = vec![0u32; self.inodes.len()];
        for pending in &self.inodes {
            if let Contents::Directory(entries) = &pending.contents {
                for (child, _) in entries.values() {
                    names[*child as usize] += 1;
                }
            }
        }
        for (number, pending) in self.inodes.iter().enumerate() {
            if let Contents::Directory(entries) = &pending.contents {
                subdirectories[number] = entries
                    .values()
                    .filter(|(child, _)| self.inodes[*child as usize].inode.is_directory())
                    .count() as u32;
            }
        }
        for (number, pending) in self.inodes.iter_mut().enumerate() {
            pending.inode.nlink = if pending.inode.is_directory() {
                2 + subdirectories[number]
            } else {
                names[number].max(1)
            };
        }

        // A directory's parent, which `..` and `getcwd` walk. The root is
        // its own parent, exactly as `/..` is `/`.
        let mut parents = vec![root; self.inodes.len()];
        for (number, pending) in self.inodes.iter().enumerate() {
            if let Contents::Directory(entries) = &pending.contents {
                for (child, _) in entries.values() {
                    if self.inodes[*child as usize].inode.is_directory() {
                        parents[*child as usize] = number as u32;
                    }
                }
            }
        }
        for (number, pending) in self.inodes.iter_mut().enumerate() {
            if pending.inode.is_directory() {
                pending.inode.parent = parents[number];
            }
        }

        // The blob, and each regular file's place in it.
        let mut blob = Vec::new();
        for pending in &mut self.inodes {
            let Contents::Regular(bytes) = &pending.contents else {
                continue;
            };
            let alignment = if pending.inode.flags & inode_flags::MMAP_ALIGNED != 0 {
                PAGE_ALIGNMENT
            } else {
                CONTENT_ALIGNMENT
            };
            let start = blob.len().next_multiple_of(alignment);
            blob.resize(start, 0);
            blob.extend_from_slice(bytes);
            pending.inode.payload = start as u64;
            pending.inode.size = bytes.len() as u64;
        }

        // The dirent region, and each directory's place in it.
        let mut dirents = Vec::new();
        for pending in &mut self.inodes {
            let Contents::Directory(entries) = &pending.contents else {
                continue;
            };
            pending.inode.payload = dirents.len() as u64;
            // A directory's `st_size` is the size of the directory's own
            // record, which is what every filesystem reports and what each
            // one means differently: ext4 answers its block size, tmpfs a
            // count-derived number, squashfs the length of its listing. The
            // image answers the length of its entry block — a real number
            // about its own storage rather than a copy of whatever
            // filesystem the bake happened to read from, which is what a
            // tar archive could never supply anyway.
            pending.inode.size =
                (DIRENT_COUNT_PREFIX + entries.len() * kisal::image::DIRENT_SIZE) as u64;
            dirents.extend_from_slice(&(entries.len() as u32).to_le_bytes());
            for (name, (child, entry_type)) in entries {
                let reference = *self
                    .interned
                    .get(name)
                    .expect("every entry name was interned when it was added");
                dirents.extend_from_slice(&reference.to_le_bytes());
                dirents.extend_from_slice(&child.to_le_bytes());
                dirents.push(*entry_type);
                dirents.extend_from_slice(&[0; 3]);
            }
        }

        // The prelink records: which inodes are translated ELFs and where
        // the bake placed each. Sorted by inode, because that is how kisal
        // searches them when a mapping asks where a library goes.
        let mut modules: Vec<(u32, u32)> = self
            .inodes
            .iter()
            .enumerate()
            .filter_map(|(number, pending)| {
                (pending.inode.flags & inode_flags::EXEC_TRANSPILED != 0)
                    .then_some((number as u32, self.bases[number]))
            })
            .collect();
        modules.sort_unstable();
        let mut module_records = Vec::with_capacity(modules.len() * kisal::image::MODULE_SIZE);
        for (inode, base) in &modules {
            module_records.extend_from_slice(&inode.to_le_bytes());
            module_records.extend_from_slice(&base.to_le_bytes());
        }

        let inode_offset = HEADER_SIZE;
        let dirent_offset = inode_offset + self.inodes.len() * INODE_SIZE;
        let string_offset = dirent_offset + dirents.len();
        let xattr_offset = string_offset + self.strings.len();
        let module_offset = xattr_offset + self.xattrs.len();

        // Every region's extent is a `u32` in the index, so a bake that
        // overflows one has to fail rather than record a length modulo 2^32 —
        // which would validate against a shorter blob and read zeros for
        // every file above the wrap. A rootfs over four gigabytes is routine
        // for the images this is aimed at.
        let total = module_offset + module_records.len();
        for (what, size) in [
            ("the file contents", blob.len()),
            ("the directory entries", dirents.len()),
            ("the strings", self.strings.len()),
            ("the extended attributes", self.xattrs.len()),
            ("the index", total),
        ] {
            if u32::try_from(size).is_err() {
                bail!(
                    "{what} come to {size} bytes, past the four gigabytes an \
                     image index can address"
                );
            }
        }

        let mut index = Vec::with_capacity(total);
        let mut header = [0u8; HEADER_SIZE];
        image::write_header(
            &mut header,
            self.inodes.len() as u32,
            inode_offset as u32,
            dirent_offset as u32,
            dirents.len() as u32,
            string_offset as u32,
            self.strings.len() as u32,
            xattr_offset as u32,
            self.xattrs.len() as u32,
            root,
            blob.len() as u32,
            module_offset as u32,
            modules.len() as u32,
        );
        index.extend_from_slice(&header);
        for pending in &self.inodes {
            let mut record = [0u8; INODE_SIZE];
            pending.inode.write(&mut record);
            index.extend_from_slice(&record);
        }
        index.extend_from_slice(&dirents);
        index.extend_from_slice(&self.strings);
        index.extend_from_slice(&self.xattrs);
        index.extend_from_slice(&module_records);

        Ok(Image { blob, index })
    }
}

/// Every name the tree contains, in the order the index stores them — a
/// diagnostic, and what a bake-to-bake diff compares.
pub fn describe(image: &Image) -> Result<Vec<String>> {
    let parsed = kisal::image::Image::parse(&image.index, &image.blob)
        .map_err(|error| anyhow::anyhow!("parsing the index just written: {error:?}"))?;
    let mut lines = Vec::new();
    let mut stack = vec![(String::new(), parsed.root())];
    while let Some((prefix, number)) = stack.pop() {
        let inode = parsed
            .inode(number)
            .map_err(|error| anyhow::anyhow!("inode {number}: {error:?}"))?;
        if !inode.is_directory() {
            continue;
        }
        let count = parsed
            .entry_count(&inode)
            .map_err(|error| anyhow::anyhow!("inode {number}: {error:?}"))?;
        for position in (0..count).rev() {
            let entry = parsed
                .entry(&inode, position)
                .map_err(|error| anyhow::anyhow!("inode {number}: {error:?}"))?;
            let name = parsed
                .string(entry.name_ref)
                .map_err(|error| anyhow::anyhow!("inode {number}: {error:?}"))?;
            let path = format!("{prefix}/{}", String::from_utf8_lossy(name));
            lines.push(path.clone());
            stack.push((path, entry.inode));
        }
    }
    lines.sort();
    Ok(lines)
}
