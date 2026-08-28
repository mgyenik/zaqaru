//! The mount table: which filesystem answers for which subtree.
//!
//! A path names a *vnode* — a filesystem and an inode within it — not an
//! inode. That distinction is not bookkeeping. `st_dev` plus `st_ino` is how
//! every program on Unix decides two paths are the same file: `find`'s
//! `-xdev`, `cp -a`'s hardlink detection, `du`'s deduplication, and every
//! "am I looking at a cycle" check. Two filesystems reuse inode numbers
//! freely, so an identity that is only an inode number makes unrelated files
//! in different mounts compare equal.
//!
//! Crossing happens in one place, on the way *into* a directory, so every
//! caller that holds a vnode holds one that is already on the right side of
//! any mount point. `..` is the other direction and is the interesting one:
//! at the root of a mounted filesystem it leaves that filesystem entirely,
//! landing on the parent of the directory the mount covers.
//!
//! Nothing here allocates: the table is a fixed array, and a mount is an
//! image plus two numbers.

use crate::errno::Errno;
use crate::image::{Image, Inode};
use crate::overlay::{Dirent, Overlay};

/// How many filesystems can be attached at once. Small on purpose — a
/// container's namespace is the image, an overlay over it, and the handful
/// of synthetic mounts (`/proc`, `/sys`, `/dev`) M4 and M5 bring.
pub const MAX_MOUNTS: usize = 8;

/// A file, named the way the kernel has to name one: which filesystem, and
/// which inode within it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Vnode {
    pub mount: u8,
    pub inode: u32,
}

impl Vnode {
    pub const fn new(mount: u8, inode: u32) -> Self {
        Self { mount, inode }
    }
}

/// What a mount is backed by.
///
/// A closed set rather than a trait: there are two kinds and the compiler
/// should be the thing that notices when a third appears. The read interface
/// is the image's, so that everything above — resolution, `stat`,
/// `getdents64` — is written once and does not know which kind it is walking.
pub enum Filesystem<'a> {
    /// A baked image, read-only. The synthetic mounts are these too: they
    /// are built at boot in the same format.
    Image(Image<'a>),
    /// The image with a writable layer over it.
    Overlay(Overlay<'a>),
}

impl<'a> Filesystem<'a> {
    pub fn root(&self) -> u32 {
        match self {
            Self::Image(image) => image.root(),
            Self::Overlay(overlay) => overlay.root(),
        }
    }

    pub fn inode(&self, number: u32) -> Result<Inode, Errno> {
        match self {
            Self::Image(image) => image.inode(number).map_err(|_| Errno::Io),
            Self::Overlay(overlay) => overlay.inode(number),
        }
    }

    pub fn lookup(
        &self,
        directory: &Inode,
        number: u32,
        name: &[u8],
    ) -> Result<Option<u32>, Errno> {
        match self {
            Self::Image(image) => Ok(image
                .lookup(directory, name)
                .map_err(|_| Errno::Io)?
                .map(|entry| entry.inode)),
            Self::Overlay(overlay) => overlay.lookup(directory, number, name),
        }
    }

    pub fn entry_count(&self, directory: &Inode, number: u32) -> Result<u32, Errno> {
        match self {
            Self::Image(image) => image.entry_count(directory).map_err(|_| Errno::Io),
            Self::Overlay(overlay) => overlay.entry_count(directory, number),
        }
    }

    pub fn entry(
        &self,
        directory: &Inode,
        number: u32,
        position: u32,
    ) -> Result<Dirent<'_>, Errno> {
        match self {
            Self::Image(image) => {
                let entry = image.entry(directory, position).map_err(|_| Errno::Io)?;
                Ok(Dirent {
                    name: image.string(entry.name_ref).map_err(|_| Errno::Io)?,
                    inode: entry.inode,
                    entry_type: entry.entry_type,
                })
            }
            Self::Overlay(overlay) => overlay.entry(directory, number, position),
        }
    }

    pub fn symlink_target(&self, inode: &Inode, number: u32) -> Result<&[u8], Errno> {
        match self {
            Self::Image(image) => image.symlink_target(inode).map_err(|_| Errno::Io),
            Self::Overlay(overlay) => overlay.symlink_target(inode, number),
        }
    }

    pub fn contents(&self, inode: &Inode, number: u32) -> Result<&[u8], Errno> {
        match self {
            Self::Image(image) => image.contents(inode).map_err(|_| Errno::Io),
            Self::Overlay(overlay) => overlay.contents(inode, number),
        }
    }

    pub fn xattr_count(&self, inode: &Inode, number: u32) -> Result<u32, Errno> {
        match self {
            Self::Image(image) => image.xattr_count(inode).map_err(|_| Errno::Io),
            Self::Overlay(overlay) => overlay.xattr_count(inode, number),
        }
    }

    pub fn xattr(
        &self,
        inode: &Inode,
        number: u32,
        position: u32,
    ) -> Result<(&[u8], &[u8]), Errno> {
        match self {
            Self::Image(image) => image.xattr(inode, position).map_err(|_| Errno::Io),
            Self::Overlay(overlay) => overlay.xattr(inode, number, position),
        }
    }

    /// A cursor over a directory's entries, in name order.
    ///
    /// One walk per listing. Asking for the entry at a position instead
    /// restarts the merge every time, which makes `getdents64` of a
    /// written-to directory quadratic — measured at 35 ms for two thousand
    /// entries, against 0.3 ms for two hundred.
    pub fn entries(&self, directory: &Inode, number: u32) -> Result<Entries<'_>, Errno> {
        match self {
            Self::Image(image) => Ok(Entries::Image {
                image: *image,
                directory: *directory,
                position: 0,
                count: image.entry_count(directory).map_err(|_| Errno::Io)?,
            }),
            Self::Overlay(overlay) => {
                if crate::overlay::is_upper(number) {
                    return Ok(Entries::Merged(overlay.merge(number)?));
                }
                // A directory that is still only in the image is read
                // straight out of it, which is the common case and stays
                // exactly as cheap as it was.
                let image = overlay.lower();
                Ok(Entries::Image {
                    image: *image,
                    directory: *directory,
                    position: 0,
                    count: image.entry_count(directory).map_err(|_| Errno::Io)?,
                })
            }
        }
    }

    /// The image underneath, whichever kind this is.
    pub fn lower(&self) -> &Image<'a> {
        match self {
            Self::Image(image) => image,
            Self::Overlay(overlay) => overlay.lower(),
        }
    }

    /// The writable layer, for the rows that change something. `None` for a
    /// read-only mount, which is what makes `EROFS` a fact about the mount
    /// rather than a guess.
    pub fn writable(&mut self) -> Option<&mut Overlay<'a>> {
        match self {
            Self::Image(_) => None,
            Self::Overlay(overlay) => Some(overlay),
        }
    }
}

/// A walk over a directory's entries.
///
/// Two shapes because there are two: an indexed array, which is a counter,
/// and a merge of two sorted sequences, which is a cursor. Neither
/// allocates.
pub enum Entries<'f> {
    Image {
        image: Image<'f>,
        directory: Inode,
        position: u32,
        count: u32,
    },
    Merged(crate::overlay::Merge<'f>),
}

impl<'f> Entries<'f> {
    pub fn next(&mut self) -> Result<Option<Dirent<'f>>, Errno> {
        match self {
            Self::Image {
                image,
                directory,
                position,
                count,
            } => {
                if *position >= *count {
                    return Ok(None);
                }
                let entry = image.entry(directory, *position).map_err(|_| Errno::Io)?;
                *position += 1;
                Ok(Some(Dirent {
                    name: image.string(entry.name_ref).map_err(|_| Errno::Io)?,
                    inode: entry.inode,
                    entry_type: entry.entry_type,
                }))
            }
            Self::Merged(merge) => match merge.next()? {
                None => Ok(None),
                Some((name, inode)) => Ok(Some(Dirent {
                    name,
                    inode,
                    // Filled in by the caller, which has the filesystem to
                    // ask. Kept out of the cursor so that a walk that only
                    // needs names does not pay for an inode read each step.
                    entry_type: crate::image::directory_entry_type::UNKNOWN,
                })),
            },
        }
    }
}

/// One attached filesystem.
struct Mount<'a> {
    filesystem: Filesystem<'a>,
    /// The directory this filesystem covers, in the mount above it. `None`
    /// for the root, which covers nothing.
    covers: Option<Vnode>,
    /// What `st_dev` reports for every file in it. Distinct per mount,
    /// because that is the entire point of the field.
    device: u64,
}

/// The device number of the first mount. It has to be *some* non-zero value:
/// a constant zero would make every image file look like it lived on the same
/// device as everything else that answers zero.
const FIRST_DEVICE: u64 = 0x0001_0000;

pub struct Mounts<'a> {
    entries: [Option<Mount<'a>>; MAX_MOUNTS],
    count: usize,
}

impl<'a> Mounts<'a> {
    /// A table with one filesystem: the baked image with a writable layer
    /// over it, at `/`.
    pub fn new(image: Image<'a>) -> Self {
        let mut entries = [const { None }; MAX_MOUNTS];
        entries[0] = Some(Mount {
            filesystem: Filesystem::Overlay(Overlay::new(image)),
            covers: None,
            device: FIRST_DEVICE,
        });
        Self { entries, count: 1 }
    }

    /// Attaches a filesystem over a directory, and returns its mount number.
    ///
    /// Refuses a target that is not a directory, and refuses to stack a
    /// second filesystem on a directory that already has one — Linux allows
    /// that stacking and this does not, because nothing in the design wants
    /// it and silently shadowing a mount is a thing nobody can debug.
    pub fn attach(&mut self, at: Vnode, image: Image<'a>) -> Result<u8, Errno> {
        if self.mounted_on(at).is_some() {
            return Err(Errno::Busy);
        }
        let inode = self.filesystem(at.mount)?.inode(at.inode)?;
        if !inode.is_directory() {
            return Err(Errno::NotDir);
        }
        if self.count == MAX_MOUNTS {
            return Err(Errno::NoMemory);
        }
        let number = self.count as u8;
        self.entries[self.count] = Some(Mount {
            filesystem: Filesystem::Image(image),
            covers: Some(at),
            device: FIRST_DEVICE + self.count as u64,
        });
        self.count += 1;
        Ok(number)
    }

    /// Attaches a filesystem, replacing whatever was mounted there.
    ///
    /// The one caller is the boot path rebuilding a synthetic mount whose
    /// contents changed — `/proc` when the executable path becomes known.
    /// `attach` refuses to stack deliberately; this is the other thing a
    /// caller might mean, and it is spelled differently so that neither can
    /// happen by accident.
    pub fn replace(&mut self, at: Vnode, image: Image<'a>) -> Result<(), Errno> {
        if let Some(existing) = self.mounted_on(at)
            && let Some(entry) = self.entries.get_mut(existing as usize).and_then(Option::as_mut)
        {
            entry.filesystem = Filesystem::Image(image);
            return Ok(());
        }
        self.attach(at, image).map(|_| ())
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// The filesystem a mount number names.
    ///
    /// `Errno::NoDevice` rather than a panic: a vnode carrying a mount that
    /// is not attached is a bug in this kernel, and the one thing it must not
    /// do is read some other mount's inode by that number.
    pub fn filesystem(&self, mount: u8) -> Result<&Filesystem<'a>, Errno> {
        self.entry(mount).map(|entry| &entry.filesystem)
    }

    pub fn filesystem_mut(&mut self, mount: u8) -> Result<&mut Filesystem<'a>, Errno> {
        self.entries
            .get_mut(mount as usize)
            .and_then(Option::as_mut)
            .map(|entry| &mut entry.filesystem)
            .ok_or(Errno::NoDevice)
    }

    pub fn device(&self, mount: u8) -> Result<u64, Errno> {
        self.entry(mount).map(|entry| entry.device)
    }

    /// The root vnode of a filesystem.
    pub fn root_of(&self, mount: u8) -> Result<Vnode, Errno> {
        Ok(Vnode::new(mount, self.entry(mount)?.filesystem.root()))
    }

    /// The directory a filesystem covers, or `None` for the root mount.
    pub fn covers(&self, mount: u8) -> Result<Option<Vnode>, Errno> {
        Ok(self.entry(mount)?.covers)
    }

    /// Which filesystem, if any, is mounted over this directory.
    pub fn mounted_on(&self, vnode: Vnode) -> Option<u8> {
        for (number, entry) in self.entries.iter().enumerate() {
            if let Some(entry) = entry {
                if entry.covers == Some(vnode) {
                    return Some(number as u8);
                }
            }
        }
        None
    }

    /// Whether a vnode is the root of its own filesystem — where `..` stops
    /// meaning "the parent inode" and starts meaning "leave this mount".
    pub fn is_mount_root(&self, vnode: Vnode) -> Result<bool, Errno> {
        Ok(vnode.inode == self.entry(vnode.mount)?.filesystem.root())
    }

    fn entry(&self, mount: u8) -> Result<&Mount<'a>, Errno> {
        self.entries
            .get(mount as usize)
            .and_then(Option::as_ref)
            .ok_or(Errno::NoDevice)
    }
}
