//! The writable layer: an in-memory upper over the baked image.
//!
//! The image is read-only and a container is not. What makes it writable is
//! the same construction every container runtime uses — an overlay — and it
//! is worth being precise about why, because a simpler answer looks
//! available: the image could just be copied into memory at boot and edited
//! in place.
//!
//! It could not. The image is a data segment of the module, which wasmtime
//! instantiates copy-on-write from a prepared page image: every instance
//! shares the same physical pages, and instantiation is an `mmap` rather than
//! a copy. Copying it would give that up for every container, including the
//! overwhelming majority of files that are never written. The overlay keeps
//! the sharing and pays only for what changes.
//!
//! Three ideas, and they are the whole design:
//!
//! - **Copy-up.** Writing to a file that exists only below copies it up
//!   first. Afterwards the upper copy is the file; the lower one is
//!   unreachable and still shared with every other instance.
//! - **Whiteouts.** Deleting a file that exists only below cannot remove it,
//!   so the upper records that the *name* is gone. A lookup that finds a
//!   whiteout stops there rather than falling through.
//! - **Merged directories.** A directory present in both is listed as the
//!   union, upper winning on a name collision, minus the whiteouts. A
//!   directory present only below is listed straight out of the image, which
//!   keeps the common case exactly as fast as it was.
//!
//! Node numbering carries which layer a file is in: the high bit set means an
//! upper node, clear means a lower inode. So a vnode is still one `u32`, and
//! every part of the kernel that holds one — a descriptor, a working
//! directory, a resolution in flight — is unchanged.

use std::collections::BTreeMap;
use std::vec::Vec;

use crate::errno::Errno;
use crate::image::{Image, Inode, directory_entry_type, file_type};

/// The largest file the writable layer will hold.
///
/// Not a number of nature: it is what this filesystem can actually store. A
/// file's contents live in the guest's own linear memory, which on the
/// target that ships is at most four gigabytes *including* the program, the
/// kernel and every other file — so a file approaching that size cannot
/// exist, and pretending otherwise means finding out by dying.
///
/// Linux answers `EFBIG` when a write would exceed what the filesystem can
/// represent, and so does this. A `lseek` past the limit is still fine:
/// seeking is not storing, and every filesystem lets a caller seek into a
/// hole it has not written.
pub const MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// Set in a node number to mean "this is in the upper layer".
///
/// The high bit, so that a lower inode number is itself: an image with two
/// billion inodes is one no machine can hold, and the alternative — a side
/// table mapping numbers to layers — would be a second thing to keep in step
/// with the first.
pub const UPPER: u32 = 1 << 31;

pub const fn is_upper(number: u32) -> bool {
    number & UPPER != 0
}

const fn upper_index(number: u32) -> usize {
    (number & !UPPER) as usize
}

/// What an upper directory holds under a name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Entry {
    /// A node in the upper layer.
    Node(u32),
    /// The name is deleted. A lookup stops here rather than falling through
    /// to the lower layer, which is the entire point.
    Whiteout,
}

/// An upper node's contents.
#[derive(Debug)]
#[derive(Clone)]
enum Body {
    Directory {
        entries: BTreeMap<Vec<u8>, Entry>,
        /// The lower directory this one shadows, if it has one. `None` means
        /// nothing below shows through — either because the directory was
        /// created here, or because it was emptied and recreated.
        lower: Option<u32>,
    },
    Regular(Vec<u8>),
    Symlink(Vec<u8>),
    /// A device node, fifo or socket. `rdev` lives in the inode's payload,
    /// as it does in the image.
    Special,
    /// The node had its last name removed and its last descriptor closed,
    /// so its bytes are gone.
    ///
    /// The node itself stays. Its *slot* is deliberately not reused: a
    /// stale number — in a descriptor, a mapping, a saved working
    /// directory — would then name a different file, silently, which is the
    /// class of bug two reviews have been spent removing. Sixty-four bytes
    /// per deleted file is not worth that; the contents, which are the
    /// megabytes, are freed.
    Released,
}

#[derive(Debug)]
#[derive(Clone)]
struct Node {
    inode: Inode,
    body: Body,
}

/// One directory entry, from whichever layer.
#[derive(Clone, Copy, Debug)]
pub struct Dirent<'a> {
    pub name: &'a [u8],
    pub inode: u32,
    pub entry_type: u8,
}

#[derive(Clone)]
pub struct Overlay<'a> {
    lower: Image<'a>,
    upper: Vec<Node>,
    /// Which lower *directories* have been copied up, and to where.
    ///
    /// Needed because a directory's identity has to survive its copy-up even
    /// when it is reached the long way round: `..` from a directory still
    /// below reports its lower parent, and that parent may have been copied
    /// up. Without this, `stat("/a")` and `stat("/a/b/..")` would name
    /// different files.
    ///
    /// Directories only, and that is the whole reason it is safe. A
    /// directory has exactly one name, so mapping its lower number to its
    /// copy is unambiguous. A file can have several — an image is full of
    /// hard links — and mapping one would make every name resolve to a copy
    /// that only one of them reaches.
    copied_up: BTreeMap<u32, u32>,
    /// See [`Overlay::take_orphans`].
    orphaned: Vec<u32>,
}

impl<'a> Overlay<'a> {
    /// An overlay with an empty upper: every read goes to the image, and the
    /// first write is what creates anything here.
    pub fn new(lower: Image<'a>) -> Self {
        let mut overlay = Self {
            lower,
            upper: Vec::new(),
            copied_up: BTreeMap::new(),
            orphaned: Vec::new(),
        };
        // The root is always upper. It has to be: the first file created at
        // `/` needs a directory here to be created *in*, and building it
        // lazily would mean every path walk checking whether the root exists
        // yet.
        let root = lower.root();
        let inode = lower.inode(root).unwrap_or(Inode {
            mode: file_type::DIRECTORY | 0o755,
            uid: 0,
            gid: 0,
            nlink: 2,
            size: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            xattr_ref: 0,
            payload: 0,
            uname_ref: 0,
            gname_ref: 0,
            flags: 0,
            parent: 0,
        });
        let number = overlay.push(Node {
            inode: Inode {
                parent: UPPER,
                ..inode
            },
            body: Body::Directory {
                entries: BTreeMap::new(),
                lower: Some(root),
            },
        });
        debug_assert_eq!(number, UPPER, "the root is the first upper node");
        overlay.copied_up.insert(root, number);
        let _ = overlay.refresh(number);
        overlay
    }

    /// A lower node's upper twin, if it has one. Applied wherever a number
    /// leaves the lower layer, so that one file has one identity.
    fn promote(&self, number: u32) -> u32 {
        if is_upper(number) {
            return number;
        }
        self.copied_up.get(&number).copied().unwrap_or(number)
    }

    /// How many bytes the writable layer is holding, for the tests that
    /// assert it gives them back.
    pub fn held_bytes(&self) -> usize {
        self.upper
            .iter()
            .map(|node| match &node.body {
                Body::Regular(bytes) => bytes.len(),
                Body::Symlink(target) => target.len(),
                _ => 0,
            })
            .sum()
    }

    /// How many nodes have had their contents released.
    pub fn released(&self) -> usize {
        self.upper
            .iter()
            .filter(|node| matches!(node.body, Body::Released))
            .count()
    }

    pub fn lower(&self) -> &Image<'a> {
        &self.lower
    }

    pub fn root(&self) -> u32 {
        UPPER
    }

    fn push(&mut self, node: Node) -> u32 {
        self.upper.push(node);
        UPPER | (self.upper.len() as u32 - 1)
    }

    /// An upper node by number.
    ///
    /// The `is_upper` check is not a formality: without it a *lower* number
    /// indexes the upper arena and reads whichever node happens to sit
    /// there, which is a wrong answer with no symptom. Every caller that
    /// reaches here has already decided the number is upper, and this is
    /// what makes that a fact.
    fn node(&self, number: u32) -> Result<&Node, Errno> {
        if !is_upper(number) {
            return Err(Errno::Invalid);
        }
        self.upper.get(upper_index(number)).ok_or(Errno::NoEntry)
    }

    fn node_mut(&mut self, number: u32) -> Result<&mut Node, Errno> {
        if !is_upper(number) {
            return Err(Errno::Invalid);
        }
        self.upper
            .get_mut(upper_index(number))
            .ok_or(Errno::NoEntry)
    }

    // ---- the read interface, which is the image's ----------------------

    pub fn inode(&self, number: u32) -> Result<Inode, Errno> {
        if is_upper(number) {
            let node = self.node(number)?;
            let mut inode = node.inode;
            inode.parent = self.promote(inode.parent);
            // A file's size follows its contents; a directory's is stored,
            // refreshed when its entries change. Deriving a directory's here
            // would put a walk of the directory on every path resolution.
            inode.size = match &node.body {
                Body::Regular(bytes) => bytes.len() as u64,
                Body::Symlink(target) => target.len() as u64,
                Body::Released => 0,
                Body::Directory { .. } | Body::Special => inode.size,
            };
            return Ok(inode);
        }
        let mut inode = self.lower.inode(number).map_err(|_| Errno::Io)?;
        inode.parent = self.promote(inode.parent);
        Ok(inode)
    }

    /// The name of an entry, and what it points at.
    pub fn lookup(
        &self,
        directory: &Inode,
        number: u32,
        name: &[u8],
    ) -> Result<Option<u32>, Errno> {
        if is_upper(number) {
            let Body::Directory { entries, lower } = &self.node(number)?.body else {
                return Err(Errno::NotDir);
            };
            match entries.get(name) {
                Some(Entry::Node(child)) => return Ok(Some(*child)),
                // The name is deleted. Not falling through is the whole
                // meaning of a whiteout.
                Some(Entry::Whiteout) => return Ok(None),
                None => {}
            }
            let Some(lower) = lower else {
                return Ok(None);
            };
            let below = self.lower.inode(*lower).map_err(|_| Errno::Io)?;
            return Ok(self
                .lower
                .lookup(&below, name)
                .map_err(|_| Errno::Io)?
                .map(|entry| self.promote(entry.inode)));
        }
        Ok(self
            .lower
            .lookup(directory, name)
            .map_err(|_| Errno::Io)?
            .map(|entry| self.promote(entry.inode)))
    }

    pub fn entry_count(&self, directory: &Inode, number: u32) -> Result<u32, Errno> {
        if is_upper(number) {
            return self.merged_count(number);
        }
        self.lower.entry_count(directory).map_err(|_| Errno::Io)
    }

    /// One entry of a directory, by position.
    ///
    /// A directory that exists only below is read straight out of the image,
    /// which is the common case and stays exactly as cheap as it was. One
    /// with an upper presence is merged.
    pub fn entry(
        &self,
        directory: &Inode,
        number: u32,
        position: u32,
    ) -> Result<Dirent<'_>, Errno> {
        if is_upper(number) {
            let (name, child) = self.merged_nth(number, position)?;
            let entry_type = directory_entry_type::of_mode(self.inode_raw(child)?.mode);
            return Ok(Dirent {
                name,
                inode: child,
                entry_type,
            });
        }
        let _ = directory;
        let entry = self
            .lower
            .entry(directory, position)
            .map_err(|_| Errno::Io)?;
        Ok(Dirent {
            name: self.lower.string(entry.name_ref).map_err(|_| Errno::Io)?,
            inode: self.promote(entry.inode),
            entry_type: entry.entry_type,
        })
    }

    pub fn symlink_target(&self, inode: &Inode, number: u32) -> Result<&[u8], Errno> {
        if is_upper(number) {
            return match &self.node(number)?.body {
                Body::Symlink(target) => Ok(target),
                _ => Err(Errno::Invalid),
            };
        }
        self.lower.symlink_target(inode).map_err(|_| Errno::Io)
    }

    pub fn contents(&self, inode: &Inode, number: u32) -> Result<&[u8], Errno> {
        if is_upper(number) {
            return match &self.node(number)?.body {
                Body::Regular(bytes) => Ok(bytes),
                // A descriptor outliving the last name is legal, and what
                // it reads is what is left.
                Body::Released => Ok(&[]),
                _ => Err(Errno::Invalid),
            };
        }
        self.lower.contents(inode).map_err(|_| Errno::Io)
    }

    /// The base the bake placed a translated ELF at.
    ///
    /// An upper file has none, and that is the design rather than an
    /// omission: a file the container wrote at run time was never
    /// translated, so there is no address its code was resolved at. Mapping
    /// one executable is the loud error the container plan names.
    pub fn prelink_base(&self, number: u32) -> Option<u64> {
        match is_upper(number) {
            true => None,
            false => self.lower.prelink_base(number),
        }
    }

    pub fn xattr_count(&self, inode: &Inode, number: u32) -> Result<u32, Errno> {
        if is_upper(number) {
            // Extended attributes are not copied up. Nothing writes one —
            // `setxattr` is refused — so an upper node has exactly the
            // attributes it was copied up with, and copy-up carries none.
            // Stated here rather than left as a silent zero.
            return Ok(0);
        }
        self.lower.xattr_count(inode).map_err(|_| Errno::Io)
    }

    pub fn xattr(
        &self,
        inode: &Inode,
        number: u32,
        position: u32,
    ) -> Result<(&[u8], &[u8]), Errno> {
        if is_upper(number) {
            return Err(Errno::NoData);
        }
        self.lower.xattr(inode, position).map_err(|_| Errno::Io)
    }

    // ---- the merge ------------------------------------------------------

    /// A cursor over a merged directory, in name order.
    ///
    /// Both sides are sorted — a `BTreeMap` and the image's dirent array, in
    /// the same byte order — so the union is one parallel pass. Written as a
    /// cursor rather than as a `Vec` because materialising it would put an
    /// allocation on the path `stat` and `getdents64` take, and as a cursor
    /// rather than a callback because the names it yields are borrowed from
    /// the overlay and have to outlive the step that produced them.
    pub fn merge(&self, number: u32) -> Result<Merge<'_>, Errno> {
        let Body::Directory { entries, lower } = &self.node(number)?.body else {
            return Err(Errno::NotDir);
        };
        let below = match lower {
            Some(lower) => Some(self.lower.inode(*lower).map_err(|_| Errno::Io)?),
            None => None,
        };
        let count = match &below {
            Some(inode) => self.lower.entry_count(inode).map_err(|_| Errno::Io)?,
            None => 0,
        };
        Ok(Merge {
            overlay: self,
            entries,
            above: entries.iter().peekable(),
            below,
            count,
            position: 0,
            pending: None,
        })
    }

    /// How many entries a merged directory has.
    fn merged_count(&self, number: u32) -> Result<u32, Errno> {
        let mut merge = self.merge(number)?;
        let mut count = 0;
        while merge.next()?.is_some() {
            count += 1;
        }
        Ok(count)
    }

    /// The entry at a position, in name order.
    fn merged_nth(&self, number: u32, position: u32) -> Result<(&[u8], u32), Errno> {
        let mut merge = self.merge(number)?;
        for _ in 0..position {
            if merge.next()?.is_none() {
                return Err(Errno::NoEntry);
            }
        }
        merge.next()?.ok_or(Errno::NoEntry)
    }

    /// A merged directory's stored size and link count, computed from
    /// scratch.
    ///
    /// Walks the whole listing, so it runs once — when a directory enters
    /// the upper layer and its counts have to start from what the layer
    /// below says. Every change after that adjusts them incrementally; see
    /// [`Self::adjust`].
    fn refresh(&mut self, number: u32) -> Result<(), Errno> {
        let mut count = 0u32;
        let mut subdirectories = 0u32;
        {
            // Both borrows are shared, so the cursor and the inode lookups
            // coexist and nothing has to be collected first.
            let mut merge = self.merge(number)?;
            while let Some((_, child)) = merge.next()? {
                count += 1;
                if self.inode_raw(child)?.is_directory() {
                    subdirectories += 1;
                }
            }
        }
        let node = self.node_mut(number)?;
        node.inode.size = (4 + 12 * count as usize) as u64;
        node.inode.nlink = 2 + subdirectories;
        Ok(())
    }

    /// An inode without the derived fields, for the internal walks that
    /// would otherwise recurse into computing them.
    fn inode_raw(&self, number: u32) -> Result<Inode, Errno> {
        if is_upper(number) {
            return Ok(self.node(number)?.inode);
        }
        self.lower.inode(number).map_err(|_| Errno::Io)
    }
}

/// The parallel walk itself. See [`Overlay::merge`].
///
/// Public because `getdents64` walks it directly: asking for the entry at a
/// position restarts the walk, so a listing done that way is quadratic in
/// the number of entries. Measured at 35 ms for two thousand of them, which
/// is a directory a build produces. Handed out as a cursor, one listing is
/// one walk.
pub struct Merge<'o> {
    overlay: &'o Overlay<'o>,
    entries: &'o BTreeMap<Vec<u8>, Entry>,
    above: core::iter::Peekable<std::collections::btree_map::Iter<'o, Vec<u8>, Entry>>,
    below: Option<Inode>,
    count: u32,
    position: u32,
    /// A lower name taken from the array and not yet used, because the upper
    /// side offered a smaller one.
    pending: Option<(&'o [u8], u32)>,
}

impl<'o> Merge<'o> {
    /// The next entry in name order, or `None` at the end.
    pub fn next(&mut self) -> Result<Option<(&'o [u8], u32)>, Errno> {
        // Skip whiteouts: they are not entries, they are the absence of one.
        while let Some((_, entry)) = self.above.peek() {
            if matches!(entry, Entry::Node(_)) {
                break;
            }
            self.above.next();
        }
        let lower = match self.pending.take() {
            Some(pending) => Some(pending),
            None => self.next_below()?,
        };
        match (self.above.peek(), lower) {
            (None, None) => Ok(None),
            (Some((name, entry)), None) => {
                let Entry::Node(child) = entry else {
                    unreachable!("whiteouts were skipped")
                };
                let answer = (name.as_slice(), *child);
                self.above.next();
                Ok(Some(answer))
            }
            (None, Some(lower)) => Ok(Some(lower)),
            (Some((upper, entry)), Some(lower)) => {
                if upper.as_slice() <= lower.0 {
                    let Entry::Node(child) = entry else {
                        unreachable!("whiteouts were skipped")
                    };
                    let answer = (upper.as_slice(), *child);
                    self.above.next();
                    // The lower name is still owed.
                    self.pending = Some(lower);
                    Ok(Some(answer))
                } else {
                    Ok(Some(lower))
                }
            }
        }
    }

    /// The next lower name the upper layer has no opinion about. A name the
    /// upper layer *does* carry is either a replacement or a whiteout, and
    /// in both cases what is below does not show through.
    fn next_below(&mut self) -> Result<Option<(&'o [u8], u32)>, Errno> {
        let Some(inode) = &self.below else {
            return Ok(None);
        };
        while self.position < self.count {
            let entry = self
                .overlay
                .lower
                .entry(inode, self.position)
                .map_err(|_| Errno::Io)?;
            let name = self
                .overlay
                .lower
                .string(entry.name_ref)
                .map_err(|_| Errno::Io)?;
            self.position += 1;
            if self.entries.contains_key(name) {
                continue;
            }
            return Ok(Some((name, self.overlay.promote(entry.inode))));
        }
        Ok(None)
    }
}

/// Extends a file's contents to `to`, filling the gap with zeros.
///
/// Two things stand between a guest and a dead instance here, and neither is
/// optional. The first is [`MAX_FILE_SIZE`]: a write at a large offset asks
/// this to materialise the hole below it, because contents are a flat
/// buffer, so `pwrite(1 byte, offset = 1 TiB)` would otherwise ask for a
/// terabyte. The second is `try_reserve`: `Vec`'s ordinary growth *aborts*
/// on allocation failure, and an abort inside a wasm module is a trap that
/// takes the whole container with it. Asking first turns a request the
/// allocator cannot meet into `ENOSPC`, which is what a filesystem out of
/// room says.
///
/// A real filesystem would store the hole sparsely and charge nothing for
/// it. This one cannot, and answers `EFBIG` rather than pretending — which
/// is a real Linux answer for a filesystem whose maximum file size is
/// smaller than what was asked for.
fn grow(contents: &mut Vec<u8>, to: u64) -> Result<(), Errno> {
    if to > MAX_FILE_SIZE {
        return Err(Errno::TooBig);
    }
    let to = to as usize;
    if contents.len() >= to {
        return Ok(());
    }
    contents
        .try_reserve(to - contents.len())
        .map_err(|_| Errno::NoSpace)?;
    contents.resize(to, 0);
    Ok(())
}

// ---- mutation ---------------------------------------------------------

impl Overlay<'_> {
    /// Ensures a node exists in the upper layer, copying it up if it does
    /// not, and returns its upper number.
    ///
    /// The parent has to be upper first — a copy needs a directory here to
    /// live in — so this walks up as far as it must. That is what overlayfs
    /// does, and it is why the first write to a deeply nested file is more
    /// expensive than the second.
    pub fn copy_up(&mut self, number: u32) -> Result<u32, Errno> {
        if is_upper(number) {
            return Ok(number);
        }
        if let Some(existing) = self.copied_up.get(&number) {
            return Ok(*existing);
        }
        let inode = self.lower.inode(number).map_err(|_| Errno::Io)?;
        // Only a *directory* knows its own parent — the index stores the
        // field for directories and leaves it meaningless for everything
        // else, because a file can have several names and a directory
        // cannot. So a file is copied up by the name it was reached
        // through, which is [`Self::copy_up_child`], and this handles the
        // chain of directories above it.
        if !inode.is_directory() {
            return Err(Errno::Invalid);
        }
        let parent = self.copy_up(inode.parent)?;
        let name = self.name_in(inode.parent, number)?.to_vec();
        let body = match inode.file_type() {
            file_type::DIRECTORY => Body::Directory {
                entries: BTreeMap::new(),
                lower: Some(number),
            },
            file_type::REGULAR => {
                Body::Regular(self.lower.contents(&inode).map_err(|_| Errno::Io)?.to_vec())
            }
            file_type::SYMLINK => Body::Symlink(
                self.lower
                    .symlink_target(&inode)
                    .map_err(|_| Errno::Io)?
                    .to_vec(),
            ),
            _ => Body::Special,
        };
        let created = self.push(Node {
            inode: Inode {
                parent,
                // Attributes are not carried up: nothing writes one, and a
                // copy that silently dropped some would be worse than one
                // that never had them. Left at zero, and `xattr_count` says
                // so rather than reading a stale reference.
                xattr_ref: 0,
                ..inode
            },
            body,
        });
        self.copied_up.insert(number, created);
        if inode.is_directory() {
            self.refresh(created)?;
        }
        self.set_entry(parent, &name, Entry::Node(created))?;
        Ok(created)
    }

    /// Copies up whatever `name` refers to in `parent`, and returns it in
    /// the upper layer.
    ///
    /// The name is what makes this possible: a file does not know its own
    /// parent, so the only way to copy one up is to already be holding the
    /// name it was reached through. That is why the copy happens at
    /// *open* time — the path is still in hand — rather than at the first
    /// write, when only a descriptor is left.
    pub fn copy_up_child(&mut self, parent: u32, name: &[u8]) -> Result<u32, Errno> {
        let parent = self.copy_up(parent)?;
        let directory = self.inode_raw(parent)?;
        let child = self
            .lookup(&directory, parent, name)?
            .ok_or(Errno::NoEntry)?;
        if is_upper(child) {
            return Ok(child);
        }
        let inode = self.lower.inode(child).map_err(|_| Errno::Io)?;
        if inode.is_directory() {
            return self.copy_up(child);
        }
        // Deliberately *not* recorded in `copied_up`. That map exists so a
        // lower node reached the long way round — `..` from a directory
        // below a copied-up one — resolves to the copy, and a directory has
        // exactly one name so the mapping is unambiguous. A file can have
        // several. Recording one here would make every other name for the
        // same file resolve to this copy, which is the opposite of breaking
        // the link: writing through one name would appear through all of
        // them. The parent's own entry map is what finds this copy, and it
        // finds it under one name only.
        let body = match inode.file_type() {
            file_type::REGULAR => {
                Body::Regular(self.lower.contents(&inode).map_err(|_| Errno::Io)?.to_vec())
            }
            file_type::SYMLINK => Body::Symlink(
                self.lower
                    .symlink_target(&inode)
                    .map_err(|_| Errno::Io)?
                    .to_vec(),
            ),
            _ => Body::Special,
        };
        let created = self.push(Node {
            inode: Inode {
                parent,
                xattr_ref: 0,
                // Copying up breaks a hard link, and the link count has to
                // say so. An image can hold one file under several names —
                // every busybox applet is that — and a copy is reached by
                // exactly one of them; the others still resolve to the
                // original below. Kernel overlayfs breaks the link the same
                // way unless it is built with its index feature on, which
                // is off by default. Carrying the lower `nlink` across
                // would have the copy claim names that no longer reach it.
                nlink: 1,
                ..inode
            },
            body,
        });
        self.set_entry(parent, name, Entry::Node(created))?;
        Ok(created)
    }

    /// Which of a lower directory's entries names a given inode.
    fn name_in(&self, directory: u32, child: u32) -> Result<&[u8], Errno> {
        let inode = self.lower.inode(directory).map_err(|_| Errno::Io)?;
        let count = self.lower.entry_count(&inode).map_err(|_| Errno::Io)?;
        for position in 0..count {
            let entry = self.lower.entry(&inode, position).map_err(|_| Errno::Io)?;
            if entry.inode == child {
                return self.lower.string(entry.name_ref).map_err(|_| Errno::Io);
            }
        }
        // The child's parent pointer and the parent's entries disagree,
        // which is a corrupt image rather than anything the guest did.
        Err(Errno::NoEntry)
    }

    /// Puts a name into an upper directory, and keeps the stored counts
    /// right without walking the whole listing.
    ///
    /// Incrementally, because the walk is O(n): a build that creates a
    /// thousand files in one directory would otherwise pay a million merge
    /// steps for the counts alone. What changes is decided by what the name
    /// meant before and what it means now, and the only lookup is a binary
    /// search of the layer below.
    fn set_entry(&mut self, directory: u32, name: &[u8], entry: Entry) -> Result<(), Errno> {
        let before = self.visible(directory, name)?;
        let after = match entry {
            Entry::Node(child) => Some(self.inode_raw(child)?.is_directory()),
            Entry::Whiteout => None,
        };
        let Body::Directory { entries, .. } = &mut self.node_mut(directory)?.body else {
            return Err(Errno::NotDir);
        };
        entries.insert(name.to_vec(), entry);
        self.adjust(directory, before, after)
    }

    /// Whether a name is currently visible in a directory, and whether what
    /// it names is a directory — the two facts the stored counts depend on.
    fn visible(&self, directory: u32, name: &[u8]) -> Result<Option<bool>, Errno> {
        let inode = self.inode_raw(directory)?;
        match self.lookup(&inode, directory, name)? {
            Some(child) => Ok(Some(self.inode_raw(child)?.is_directory())),
            None => Ok(None),
        }
    }

    /// Moves a directory's stored size and link count by what one name did.
    fn adjust(
        &mut self,
        directory: u32,
        before: Option<bool>,
        after: Option<bool>,
    ) -> Result<(), Errno> {
        let entries = i64::from(after.is_some()) - i64::from(before.is_some());
        let subdirectories = i64::from(after == Some(true)) - i64::from(before == Some(true));
        let node = self.node_mut(directory)?;
        let count = ((node.inode.size as i64 - 4) / 12 + entries).max(0) as u64;
        node.inode.size = 4 + 12 * count;
        node.inode.nlink = (node.inode.nlink as i64 + subdirectories).max(2) as u32;
        Ok(())
    }

    /// Creates a node in an upper directory.
    pub fn create(
        &mut self,
        directory: u32,
        name: &[u8],
        inode: Inode,
        contents: Option<Vec<u8>>,
    ) -> Result<u32, Errno> {
        let directory = self.copy_up(directory)?;
        // Checked before anything is pushed. The arena has no way to take a
        // node back — a descriptor may already be holding one, so nothing
        // can be reference-counted away — and a node that was created and
        // never named is one that can never be reached or freed.
        if !matches!(self.node(directory)?.body, Body::Directory { .. }) {
            return Err(Errno::NotDir);
        }
        let body = match inode.file_type() {
            file_type::DIRECTORY => Body::Directory {
                entries: BTreeMap::new(),
                // Created here, so nothing shows through from below — even
                // if a directory of that name once existed and was removed.
                lower: None,
            },
            file_type::REGULAR => Body::Regular(contents.unwrap_or_default()),
            file_type::SYMLINK => Body::Symlink(contents.unwrap_or_default()),
            _ => Body::Special,
        };
        let created = self.push(Node {
            inode: Inode {
                parent: directory,
                ..inode
            },
            body,
        });
        if self.inode_raw(created)?.is_directory() {
            self.refresh(created)?;
        }
        self.set_entry(directory, name, Entry::Node(created))?;
        Ok(created)
    }

    /// Gives an existing node a second name.
    ///
    /// The upper layer can hold real hard links, which is what makes this
    /// possible at all: two entries pointing at one node. `nlink` counts
    /// the names, and it has to be right — reclamation turns on it, and so
    /// does every program that compares link counts to decide whether a
    /// file is shared.
    ///
    /// Directories are refused: POSIX forbids hard links to them, and a
    /// filesystem that allowed one would have a cycle nothing could walk.
    pub fn link(&mut self, directory: u32, name: &[u8], node: u32) -> Result<(), Errno> {
        let directory = self.copy_up(directory)?;
        if self.inode_raw(node)?.is_directory() {
            return Err(Errno::Perm);
        }
        if !matches!(self.node(directory)?.body, Body::Directory { .. }) {
            return Err(Errno::NotDir);
        }
        self.node_mut(node)?.inode.nlink += 1;
        self.set_entry(directory, name, Entry::Node(node))
    }

    /// Removes a name from a directory.
    ///
    /// A name that exists below cannot be removed, so what is recorded is
    /// that it is gone: the whiteout is the deletion. A name that exists
    /// only above is dropped outright, and the node it pointed at becomes
    /// unreachable — no reference counting, because a descriptor holds a
    /// number and the arena never shrinks.
    pub fn unlink(&mut self, directory: u32, name: &[u8]) -> Result<(), Errno> {
        let node = self.detach(directory, name)?;
        if let Some(node) = node {
            let remaining = {
                let inode = &mut self.node_mut(node)?.inode;
                inode.nlink = inode.nlink.saturating_sub(1);
                inode.nlink
            };
            if remaining == 0 {
                // No name reaches it any more. Whether its bytes can go now
                // depends on whether a descriptor still holds it, which is
                // the caller's question to answer — POSIX keeps an unlinked
                // file alive until the last one closes.
                self.orphaned.push(node);
            }
        }
        Ok(())
    }

    /// Takes a name out of a directory, and reports which upper node it
    /// pointed at.
    ///
    /// The link count is *not* touched here, because two callers mean
    /// different things by removing a name: `unlink` is losing one, and
    /// `rename` is moving one. Conflating them made a renamed file look
    /// like it had no names left, and its contents were freed out from
    /// under the name it had just been given.
    fn detach(&mut self, directory: u32, name: &[u8]) -> Result<Option<u32>, Errno> {
        let directory = self.copy_up(directory)?;
        let shadows_lower = {
            let Body::Directory { entries, lower } = &self.node(directory)?.body else {
                return Err(Errno::NotDir);
            };
            let below = match lower {
                None => false,
                Some(lower) => {
                    let inode = self.lower.inode(*lower).map_err(|_| Errno::Io)?;
                    self.lower
                        .lookup(&inode, name)
                        .map_err(|_| Errno::Io)?
                        .is_some()
                }
            };
            if !below && !matches!(entries.get(name), Some(Entry::Node(_))) {
                return Err(Errno::NoEntry);
            }
            below
        };
        let held = match self.node(directory)?.body {
            Body::Directory { ref entries, .. } => match entries.get(name) {
                Some(Entry::Node(node)) => Some(*node),
                _ => None,
            },
            _ => None,
        };
        if shadows_lower {
            // A name the layer below also has cannot simply go: what is
            // recorded is that it is gone.
            self.set_entry(directory, name, Entry::Whiteout)?;
        } else {
            let before = self.visible(directory, name)?;
            let Body::Directory { entries, .. } = &mut self.node_mut(directory)?.body else {
                return Err(Errno::NotDir);
            };
            entries.remove(name);
            // Removing an upper name can *uncover* one below it, in which
            // case the directory has the same number of entries as before.
            let after = self.visible(directory, name)?;
            self.adjust(directory, before, after)?;
        }
        Ok(held)
    }

    /// Nodes whose last name has gone, waiting to be checked against the
    /// descriptors still open on them.
    ///
    /// Kept as a list rather than freed on the spot because this layer does
    /// not know what the descriptor table holds — and the alternative,
    /// reference counting every open, would put the fd table and the
    /// filesystem into a cycle that neither owns.
    pub fn take_orphans(&mut self) -> Vec<u32> {
        core::mem::take(&mut self.orphaned)
    }

    /// Frees a node's contents. The node stays; see [`Body::Released`].
    pub fn release(&mut self, node: u32) -> Result<(), Errno> {
        let entry = self.node_mut(node)?;
        if entry.inode.nlink > 0 {
            // A name reappeared — a `link` between the unlink and this
            // call. Nothing to free.
            return Ok(());
        }
        entry.body = Body::Released;
        entry.inode.size = 0;
        Ok(())
    }

    /// Whether a node has no names left, which is what makes it a candidate
    /// for release once the last descriptor closes.
    pub fn is_orphaned(&self, node: u32) -> bool {
        if !is_upper(node) {
            return false;
        }
        self.node(node)
            .map(|node| node.inode.nlink == 0)
            .unwrap_or(false)
    }

    /// Whether an upper directory reads through to one below it.
    ///
    /// A directory copied up from the image keeps a window onto the image's
    /// version — that is what makes the listing a merge — and a directory
    /// created here has none. `rename` turns on the difference: what moves
    /// is a name, and a name whose contents come from the layer below would
    /// leave every path under it resolving at the old place.
    pub fn shadows_lower(&self, number: u32) -> Result<bool, Errno> {
        match &self.node(number)?.body {
            Body::Directory { lower, .. } => Ok(lower.is_some()),
            _ => Ok(false),
        }
    }

    /// Whether a directory has any entry at all, which is what `rmdir`
    /// needs to know.
    pub fn is_empty_directory(&self, number: u32) -> Result<bool, Errno> {
        if is_upper(number) {
            return Ok(self.merged_count(number)? == 0);
        }
        // A directory still only in the image: nothing has been added to or
        // removed from it, so what the image says is the answer.
        let inode = self.lower.inode(number).map_err(|_| Errno::Io)?;
        Ok(self.lower.entry_count(&inode).map_err(|_| Errno::Io)? == 0)
    }

    /// Writes into a regular file, extending it with zeros if the offset is
    /// past its end — which is how a sparse file is written, and what every
    /// `pwrite` past the end does.
    pub fn write_at(&mut self, number: u32, offset: u64, bytes: &[u8]) -> Result<u64, Errno> {
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(Errno::TooBig)?;
        let Body::Regular(contents) = &mut self.node_mut(number)?.body else {
            return Err(Errno::Invalid);
        };
        grow(contents, end)?;
        let offset = offset as usize;
        contents[offset..offset + bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len() as u64)
    }

    pub fn truncate(&mut self, number: u32, length: u64) -> Result<(), Errno> {
        let Body::Regular(contents) = &mut self.node_mut(number)?.body else {
            return Err(Errno::Invalid);
        };
        if length < contents.len() as u64 {
            contents.truncate(length as usize);
            return Ok(());
        }
        grow(contents, length)
    }

    /// Sets a file's modification time.
    ///
    /// Load-bearing, and not obviously so: CPython decides whether a `.pyc`
    /// is stale by comparing the source's `mtime` against the one recorded
    /// in the cache file. A kernel that rounded or invented timestamps would
    /// make every import either recompile or use a stale cache, and neither
    /// would look like a filesystem bug.
    pub fn set_times(&mut self, number: u32, seconds: i64, nanoseconds: u32) -> Result<(), Errno> {
        let node = self.node_mut(number)?;
        node.inode.mtime_sec = seconds;
        node.inode.mtime_nsec = nanoseconds;
        Ok(())
    }

    pub fn set_mode(&mut self, number: u32, mode: u32) -> Result<(), Errno> {
        let node = self.node_mut(number)?;
        node.inode.mode = (node.inode.mode & file_type::MASK) | (mode & 0o7777);
        Ok(())
    }

    /// Moves a name from one directory to another.
    ///
    /// The source has to be upper first: a rename is a deletion and a
    /// creation, and the deletion half needs somewhere to record itself.
    pub fn rename(
        &mut self,
        from_directory: u32,
        from_name: &[u8],
        to_directory: u32,
        to_name: &[u8],
    ) -> Result<(), Errno> {
        // By name, because that is the only way a file can be copied up —
        // it does not know its own parent. The name is in hand here, which
        // is exactly why the copy happens now rather than later.
        let node = self.copy_up_child(from_directory, from_name)?;
        let to_directory = self.copy_up(to_directory)?;
        // A name the destination already had is *unlinked* by the rename,
        // which is what makes `rename` an atomic replace — so it loses a
        // name and may become reclaimable.
        if let Body::Directory { entries, .. } = &self.node(to_directory)?.body
            && let Some(Entry::Node(replaced)) = entries.get(to_name).copied()
            && replaced != node
        {
            let remaining = {
                let inode = &mut self.node_mut(replaced)?.inode;
                inode.nlink = inode.nlink.saturating_sub(1);
                inode.nlink
            };
            if remaining == 0 {
                self.orphaned.push(replaced);
            }
        }
        // The source name goes without changing the count: a rename moves a
        // name rather than removing one.
        self.detach(from_directory, from_name)?;
        self.node_mut(node)?.inode.parent = to_directory;
        self.set_entry(to_directory, to_name, Entry::Node(node))
    }
}
