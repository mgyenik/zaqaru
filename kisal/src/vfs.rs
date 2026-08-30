//! Resolution: turning a path into an inode, component by component.
//!
//! This is where POSIX lives, and the discipline is that it lives *only*
//! here. The store below is asked one dumb question — does this directory
//! have an entry with this name — and everything else is decided in this
//! loop: `.` and `..`, symlink following and its hop limit, whether a
//! trailing slash obliges the target to be a directory, where a relative path
//! starts from. Structfs paths have no `..` and no symlinks, so asking a
//! store to emulate either would smear POSIX across every backend that will
//! ever exist.
//!
//! Nothing here allocates. Symlinks are followed by recursion rather than by
//! splicing the target into the path, which would need a buffer; the
//! remainder of the original path simply continues from wherever the target
//! resolved to.

use std::vec::Vec;

use crate::errno::Errno;
use crate::image::Inode;
use crate::mount::{Filesystem, Mounts, Vnode};

/// Linux's `SYMLOOP_MAX`. Chains longer than this are `ELOOP` whether or not
/// they actually loop — which is what Linux does, because detecting a genuine
/// cycle costs more than refusing a chain nobody meant to write.
///
/// Counted as *total traversals per resolution*, which is what Linux counts.
/// Counting nesting depth instead lets a path with a thousand links laid end
/// to end resolve here and fail on a real kernel — permissive, but a
/// difference in what the same image does, which is the entire class of bug
/// this layer exists to prevent.
const SYMLINK_LIMIT: u32 = 40;

/// Linux's `NAME_MAX`.
const NAME_MAX: usize = crate::file::MAX_NAME;

/// What a caller wants from a resolution.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lookup {
    /// Follow a symlink in the final position. False for `lstat`, for
    /// `readlink`, and for `open` with `O_NOFOLLOW`. Components before the
    /// last are always followed — that is not a choice POSIX offers.
    pub follow_final: bool,
    /// The result must be a directory: a trailing slash said so, or
    /// `O_DIRECTORY` did.
    pub require_directory: bool,
}

impl Lookup {
    /// What `stat`, and an ordinary `open`, ask for.
    pub const FOLLOW: Self = Self {
        follow_final: true,
        require_directory: false,
    };
    /// What `lstat` and `readlink` ask for.
    pub const NO_FOLLOW: Self = Self {
        follow_final: false,
        require_directory: false,
    };
}

/// The guest's view of the filesystem.
///
/// Resolution lives here and the backing stays dumb: a store is asked
/// `lookup(directory, name)` and nothing else. What a path *means* — `..`,
/// symlinks, trailing slashes, where a mount begins — is decided in this
/// loop, once, for every filesystem there will ever be.
#[derive(Clone)]
pub struct Vfs<'a> {
    mounts: Mounts<'a>,
    working_directory: Vnode,
}

impl<'a> Vfs<'a> {
    pub fn new(image: crate::image::Image<'a>) -> Self {
        let mounts = Mounts::new(image);
        let working_directory = mounts
            .root_of(0)
            .expect("the table was just built with a root mount");
        Self {
            mounts,
            working_directory,
        }
    }

    pub fn mounts(&self) -> &Mounts<'a> {
        &self.mounts
    }

    pub fn mounts_mut(&mut self) -> &mut Mounts<'a> {
        &mut self.mounts
    }

    /// The filesystem a vnode lives in.
    pub fn filesystem_of(&self, vnode: Vnode) -> Result<&Filesystem<'a>, Errno> {
        self.mounts.filesystem(vnode.mount)
    }

    /// The same, to write through.
    pub fn filesystem_of_mut(&mut self, vnode: Vnode) -> Result<&mut Filesystem<'a>, Errno> {
        self.mounts.filesystem_mut(vnode.mount)
    }

    pub fn root(&self) -> Vnode {
        self.mounts
            .root_of(0)
            .expect("the root mount is always attached")
    }

    pub fn working_directory(&self) -> Vnode {
        self.working_directory
    }


    pub fn inode(&self, vnode: Vnode) -> Result<Inode, Errno> {
        self.mounts.filesystem(vnode.mount)?.inode(vnode.inode)
    }

    /// `st_dev` for a vnode: which filesystem it is in.
    pub fn device(&self, vnode: Vnode) -> Result<u64, Errno> {
        self.mounts.device(vnode.mount)
    }

    /// Steps into whatever is mounted over a directory. Called on the way in
    /// to every component, so no caller ever holds the covered directory.
    fn cross_down(&self, mut vnode: Vnode) -> Result<Vnode, Errno> {
        // A loop rather than an `if`: stacking is refused at `attach` time,
        // but a filesystem mounted on the root of another one is not
        // stacking and does chain.
        for _ in 0..crate::mount::MAX_MOUNTS {
            match self.mounts.mounted_on(vnode) {
                Some(mount) => vnode = self.mounts.root_of(mount)?,
                None => return Ok(vnode),
            }
        }
        Err(Errno::Loop)
    }

    /// Where `..` goes. At the root of a mounted filesystem it leaves that
    /// filesystem: the answer is the parent of the directory the mount
    /// covers, in the mount above. The real root is its own parent, so a
    /// path cannot escape upward — which is also what a chroot means.
    fn parent_of(&self, vnode: Vnode) -> Result<Vnode, Errno> {
        let mut vnode = vnode;
        for _ in 0..crate::mount::MAX_MOUNTS {
            if !self.mounts.is_mount_root(vnode)? {
                break;
            }
            match self.mounts.covers(vnode.mount)? {
                Some(covered) => vnode = covered,
                None => return Ok(vnode),
            }
        }
        let parent = Vnode::new(vnode.mount, self.inode(vnode)?.parent);
        // The covered directory's parent can itself be a mount point.
        self.cross_down(parent)
    }

    /// Resolves `path`, starting from `start` when it is relative.
    ///
    /// `start` is the working directory for a bare path and the directory a
    /// descriptor names for the `…at` family. An absolute path ignores it,
    /// which is what makes `openat(fd, "/etc/passwd")` mean what it says.
    pub fn resolve(&self, start: Vnode, path: &[u8], lookup: Lookup) -> Result<Vnode, Errno> {
        // Linux distinguishes these: an empty path is `ENOENT`, not the
        // directory it was relative to. `AT_EMPTY_PATH` is the opt-in that
        // changes it, and callers pass that in themselves.
        if path.is_empty() {
            return Err(Errno::NoEntry);
        }
        // A trailing slash does two things, and only one of them was being
        // done. It requires the target to be a directory — and it also makes
        // the final component *not* final, so its symlink is followed even
        // under `O_NOFOLLOW` or `AT_SYMLINK_NOFOLLOW`. `/lib`, `/bin`, `/sbin`
        // and `/lib64` are symlinks in every modern base image, so
        // `stat("/lib/")` is an ordinary thing to do and it must work.
        let trailing_slash = path.last() == Some(&b'/');
        let lookup = Lookup {
            require_directory: lookup.require_directory || trailing_slash,
            follow_final: lookup.follow_final || trailing_slash,
        };
        let mut traversals = 0;
        self.walk(start, path, lookup, &mut traversals)
    }

    /// Resolves everything but the last component, for the rows that create
    /// or remove a name rather than open one.
    ///
    /// Returns the directory the name lives in and the name itself. The
    /// distinction matters: `unlink("/a/b")` has to find `/a` even when `b`
    /// does not exist, and `open("/a/b", O_CREAT)` has to find `/a` in order
    /// to put `b` in it.
    ///
    /// `.` and `..` as the final component are refused here. Linux answers
    /// `EINVAL` for `rmdir(".")` and `ENOTEMPTY` for `rmdir("..")`; both are
    /// names no caller can create or remove, and letting them through would
    /// mean a row deleting the directory it was told to look inside.
    /// `follow_final` decides whether a trailing symlink is the thing being
    /// changed or the thing being changed *through*. `unlink` and `rename`
    /// act on the link; `open` for writing, `truncate`, `chmod` and
    /// `utimensat` act on what it points at, which is what every one of
    /// them does on Linux.
    /// The name comes back owned rather than borrowed. It has to: following
    /// a trailing symlink means the answer is a name from the *filesystem*
    /// rather than from the caller's path, and the two have different
    /// lifetimes. One small allocation, on a path that is about to change
    /// something and allocates anyway.
    pub fn resolve_parent(
        &self,
        start: Vnode,
        path: &[u8],
        follow_final: bool,
    ) -> Result<(Vnode, Vec<u8>), Errno> {
        let mut traversals = 0;
        self.walk_parent(start, path, follow_final, &mut traversals)
    }

    fn walk_parent(
        &self,
        start: Vnode,
        path: &[u8],
        follow_final: bool,
        traversals: &mut u32,
    ) -> Result<(Vnode, Vec<u8>), Errno> {
        if path.is_empty() {
            return Err(Errno::NoEntry);
        }
        // A trailing slash does not change which name is last — `a/b/` names
        // `b` — but it does oblige `b` to be a directory, which the caller
        // decides what to do about.
        let trimmed = {
            let mut end = path.len();
            while end > 1 && path[end - 1] == b'/' {
                end -= 1;
            }
            &path[..end]
        };
        let split = trimmed.iter().rposition(|byte| *byte == b'/');
        let (directory, name) = match split {
            Some(0) => (&b"/"[..], &trimmed[1..]),
            Some(at) => (&trimmed[..at], &trimmed[at + 1..]),
            None => (&b"."[..], trimmed),
        };
        if name.is_empty() || name == b"." || name == b".." {
            return Err(if name == b".." {
                Errno::NotEmpty
            } else {
                Errno::Invalid
            });
        }
        if name.len() > NAME_MAX {
            return Err(Errno::NameTooLong);
        }
        let directory = self.resolve(
            start,
            directory,
            Lookup {
                follow_final: true,
                require_directory: true,
            },
        )?;
        if !follow_final {
            return Ok((directory, name.to_vec()));
        }
        // The name is a symlink and the caller is changing what it points
        // at, so the answer is the *target's* parent and the target's own
        // name. Without this, `truncate` through a link empties the link,
        // `chmod` changes the link's mode, and `open` for writing hands
        // back a descriptor on a symlink — three plausible successes that
        // leave the file the caller meant untouched.
        let inode = self.inode(directory)?;
        let Some(child) = self
            .filesystem_of(directory)?
            .lookup(&inode, directory.inode, name)?
        else {
            return Ok((directory, name.to_vec()));
        };
        let child = Vnode::new(directory.mount, child);
        if !self.inode(child)?.is_symlink() {
            return Ok((directory, name.to_vec()));
        }
        *traversals += 1;
        if *traversals > SYMLINK_LIMIT {
            return Err(Errno::Loop);
        }
        let target = self
            .filesystem_of(child)?
            .symlink_target(&self.inode(child)?, child.inode)?;
        if target.is_empty() {
            return Err(Errno::NoEntry);
        }
        // Resolved against the directory the *link* is in, as every symlink
        // is.
        let target = target.to_vec();
        self.walk_parent(directory, &target, true, traversals)
    }

    fn walk(
        &self,
        start: Vnode,
        path: &[u8],
        lookup: Lookup,
        traversals: &mut u32,
    ) -> Result<Vnode, Errno> {
        let mut current = if path.first() == Some(&b'/') {
            self.cross_down(self.root())?
        } else {
            start
        };

        let mut components = path
            .split(|byte| *byte == b'/')
            .filter(|component| !component.is_empty())
            .peekable();

        // A path of nothing but slashes is the root, and it is a directory.
        while let Some(component) = components.next() {
            let is_final = components.peek().is_none();

            // The directory the component is looked up in, kept by number
            // because a symlink found here resolves against *it* rather than
            // against the working directory.
            let directory = current;
            let inode = self.inode(current)?;
            // Every component before the last must be a directory to be
            // walked *through*, and `ENOTDIR` is what says so.
            //
            // Checked *before* the component's own length, because that is
            // Linux's precedence: `stat("regular-file/<300 chars>")` is
            // `ENOTDIR`, not `ENAMETOOLONG`. The parent being unwalkable is
            // decided before anything about the name is looked at.
            if !inode.is_directory() {
                return Err(Errno::NotDir);
            }
            if component.len() > NAME_MAX {
                return Err(Errno::NameTooLong);
            }

            if component == b"." {
                continue;
            }
            if component == b".." {
                current = self.parent_of(current)?;
                continue;
            }

            let child = self
                .filesystem_of(directory)?
                .lookup(&inode, directory.inode, component)?
                .ok_or(Errno::NoEntry)?;
            // Whatever is mounted over the entry is what the name reaches,
            // which is what makes a mount point mean anything at all.
            current = self.cross_down(Vnode::new(directory.mount, child))?;

            let resolved = self.inode(current)?;
            if resolved.is_symlink() && (!is_final || lookup.follow_final) {
                *traversals += 1;
                if *traversals > SYMLINK_LIMIT {
                    return Err(Errno::Loop);
                }
                let target = self
                    .filesystem_of(current)?
                    .symlink_target(&resolved, current.inode)?;
                // A symlink with an empty target is not a path; Linux calls
                // it `ENOENT` rather than resolving to the directory.
                if target.is_empty() {
                    return Err(Errno::NoEntry);
                }
                // Resolved against the directory the *link* is in, not
                // against the working directory. The traversal count is
                // threaded through rather than reset, so a chain laid end to
                // end costs what a nested one does.
                current = self.walk(directory, target, Lookup::FOLLOW, traversals)?;
            }
        }

        if lookup.require_directory && !self.inode(current)?.is_directory() {
            return Err(Errno::NotDir);
        }
        Ok(current)
    }

    /// The absolute path of a directory, written into `into`.
    ///
    /// Walks up through the parent pointers and asks each parent which of its
    /// entries the child is. That is `O(depth × entries)` and it is the right
    /// trade: `getcwd` is called once at startup and the alternative is
    /// carrying a path string on every `chdir` and keeping it correct.
    pub fn absolute_path(&self, directory: Vnode, into: &mut [u8]) -> Result<usize, Errno> {
        let root = self.root();
        if directory == root {
            return write_at(into, 0, b"/");
        }

        // Collect the chain upward, then emit it downward. The chain is
        // bounded by the tree's depth, and a tree deeper than this is one
        // nobody can name anyway.
        const MAX_DEPTH: usize = 128;
        let mut chain = [Vnode::new(0, 0); MAX_DEPTH];
        let mut length = 0;
        let mut current = directory;
        while current != root {
            if length == MAX_DEPTH {
                return Err(Errno::NameTooLong);
            }
            chain[length] = current;
            length += 1;
            current = self.parent_of(current)?;
        }

        let mut written = 0;
        for index in (0..length).rev() {
            let name = self.name_of(chain[index])?;
            written = write_at(into, written, b"/")?;
            written = write_at(into, written, name)?;
        }
        Ok(written)
    }

    /// A directory's own name in its parent. For the root of a mounted
    /// filesystem that is the name of the directory it covers, which is what
    /// makes `getcwd` inside a mount report the path a caller can hand back.
    fn name_of(&self, vnode: Vnode) -> Result<&[u8], Errno> {
        let mut vnode = vnode;
        for _ in 0..crate::mount::MAX_MOUNTS {
            if !self.mounts.is_mount_root(vnode)? {
                break;
            }
            match self.mounts.covers(vnode.mount)? {
                Some(covered) => vnode = covered,
                // The root of everything has no name.
                None => return Err(Errno::NoEntry),
            }
        }
        let parent = Vnode::new(vnode.mount, self.inode(vnode)?.parent);
        self.name_in(parent, vnode)
    }

    /// Which of a directory's entries names a given inode.
    fn name_in(&self, directory: Vnode, child: Vnode) -> Result<&[u8], Errno> {
        let inode = self.inode(directory)?;
        let filesystem = self.filesystem_of(directory)?;
        let count = filesystem.entry_count(&inode, directory.inode)?;
        for position in 0..count {
            let entry = filesystem.entry(&inode, directory.inode, position)?;
            if entry.inode == child.inode {
                return Ok(entry.name);
            }
        }
        // The child's parent pointer and the parent's entries disagree, which
        // is a corrupt index rather than anything the guest did.
        Err(Errno::NoEntry)
    }

    pub fn set_working_directory(&mut self, directory: Vnode) -> Result<(), Errno> {
        if !self.inode(directory)?.is_directory() {
            return Err(Errno::NotDir);
        }
        self.working_directory = directory;
        Ok(())
    }
}

fn write_at(into: &mut [u8], at: usize, bytes: &[u8]) -> Result<usize, Errno> {
    let end = at + bytes.len();
    if end > into.len() {
        return Err(Errno::Range);
    }
    into[at..end].copy_from_slice(bytes);
    Ok(end)
}
