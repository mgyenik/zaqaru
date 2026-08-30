//! The rows that change something.
//!
//! Everything here goes through the same three steps: resolve the name to a
//! directory and a component, copy that directory up if it is still only in
//! the image, and then make the change in the upper layer. The order matters
//! — a change needs somewhere to be recorded before it can be made — and it
//! is why the first write into a deep path is more expensive than the rest.
//!
//! What decides whether a change is allowed at all is the *mount*, not the
//! file: `EROFS` means "this filesystem does not accept changes", and it is
//! answered by asking the mount table whether the filesystem has a writable
//! layer. A synthetic `/proc` says no; the root says yes.

use crate::abi::Store;
use crate::errno::Errno;
use crate::file::{at, open_flags};
use crate::image::{Inode, file_type};
use crate::machine::Machine;
use crate::mount::Vnode;
use crate::syscall::{Arguments, Fault, Kernel, Outcome, number};
use crate::vfs::Lookup;

/// `utimensat`'s two special nanosecond values.
pub mod utime {
    /// "Now" — which in a container with no clock of its own is the same
    /// answer `clock_gettime` gives.
    pub const NOW: i64 = (1 << 30) - 1;
    /// "Leave this one alone."
    pub const OMIT: i64 = (1 << 30) - 2;
}

/// `flock`'s operations.
pub mod lock {
    pub const SHARED: i32 = 1;
    pub const EXCLUSIVE: i32 = 2;
    pub const NONBLOCK: i32 = 4;
    pub const UNLOCK: i32 = 8;
}

impl<'a, S: Store, M: Machine> Kernel<'a, S, M> {
    // ---- helpers -------------------------------------------------------

    /// The directory a name lives in, and the name — with the directory
    /// already copied up, so that something can be put in it.
    ///
    /// `EROFS` if the mount has no writable layer, which is a fact about the
    /// filesystem rather than about the file.
    /// `follow` decides whether a trailing symlink is the thing being
    /// changed or the thing being changed *through* — see
    /// [`crate::vfs::Vfs::resolve_parent`].
    fn writable_parent(
        &mut self,
        dirfd: i64,
        path: i64,
        follow: bool,
    ) -> Result<(Vnode, Vec<u8>), Errno> {
        let (directory, name) = self.parent_of(dirfd, path, follow)?;
        let upper = self.copy_up(directory)?;
        Ok((upper, name))
    }

    /// The same, *without* copying anything up.
    ///
    /// Copying a directory up changes its identity — the guest sees a new
    /// `st_ino` — so a call that is about to fail must not do it. An earlier
    /// version copied up first and validated after, which meant a failed
    /// `unlink` of a name that was never there, or an `EEXIST` from `mkdir`,
    /// silently renumbered the parent directory. `(st_dev, st_ino)` is how
    /// `find -xdev`, `du` and every cycle check identify a file.
    fn parent_of(
        &mut self,
        dirfd: i64,
        path: i64,
        follow: bool,
    ) -> Result<(Vnode, Vec<u8>), Errno> {
        let text = self.path_at(path)?;
        let start = self.start_at(dirfd, text)?;
        // `EROFS` before anything else: a filesystem that cannot change is
        // not one a failed validation should have walked into.
        let (directory, name) = self.vfs.resolve_parent(start, text, follow)?;
        if !self.is_writable(directory) {
            return Err(Errno::ReadOnlyFs);
        }
        Ok((directory, name))
    }

    /// Where a relative path starts, given a descriptor.
    fn start_at(&self, dirfd: i64, path: &[u8]) -> Result<Vnode, Errno> {
        if path.first() == Some(&b'/') {
            return Ok(self.vfs.root());
        }
        self.start_directory(dirfd)
    }

    /// Copies a *directory* into the writable layer.
    fn copy_up(&mut self, vnode: Vnode) -> Result<Vnode, Errno> {
        let overlay = self
            .vfs
            .filesystem_of_mut(vnode)?
            .writable()
            .ok_or(Errno::ReadOnlyFs)?;
        let number = overlay.copy_up(vnode.inode)?;
        Ok(Vnode::new(vnode.mount, number))
    }

    /// Copies whatever a path names into the writable layer, and returns it
    /// there. The path is what makes it possible — see
    /// [`crate::overlay::Overlay::copy_up_child`].
    pub(crate) fn copy_up_path(
        &mut self,
        dirfd: i64,
        path: i64,
        follow: bool,
    ) -> Result<Vnode, Errno> {
        let (directory, name) = self.writable_parent(dirfd, path, follow)?;
        let overlay = self.overlay_of(directory)?;
        let number = overlay.copy_up_child(directory.inode, &name)?;
        Ok(Vnode::new(directory.mount, number))
    }

    /// The mount's writable layer, or `EROFS`.
    fn overlay_of(&mut self, vnode: Vnode) -> Result<&mut crate::overlay::Overlay<'a>, Errno> {
        self.vfs
            .filesystem_of_mut(vnode)?
            .writable()
            .ok_or(Errno::ReadOnlyFs)
    }

    /// A fresh inode with the mode and ownership a new file gets.
    fn new_inode(&mut self, kind: u32, mode: u32) -> Inode {
        Inode {
            mode: kind | (mode & 0o7777),
            // One user. `setuid` and the rest of the identity story arrive
            // with M6; until then everything the container creates belongs
            // to whoever the image says is running it.
            uid: 0,
            gid: 0,
            nlink: 1,
            size: 0,
            mtime_sec: self.now(),
            mtime_nsec: 0,
            xattr_ref: 0,
            payload: 0,
            uname_ref: 0,
            gname_ref: 0,
            flags: 0,
            parent: 0,
        }
    }

    /// Whether the mount a file is in accepts changes at all.
    pub(crate) fn is_writable(&self, vnode: Vnode) -> bool {
        matches!(
            self.vfs.filesystem_of(vnode),
            Ok(crate::mount::Filesystem::Overlay(_))
        )
    }

    /// `O_TRUNC` at open time, which happens before the descriptor exists.
    pub(crate) fn truncate_open(&mut self, vnode: Vnode) -> Result<(), Errno> {
        self.truncate_vnode(vnode, 0)
    }

    // ---- creating and removing names -----------------------------------

    /// Creates a file for `open(…, O_CREAT)`.
    pub(crate) fn create_file(&mut self, dirfd: i64, path: i64, mode: u32) -> Result<Vnode, Errno> {
        // Creating: the name is the thing, and it does not exist yet.
        let (directory, name) = self.writable_parent(dirfd, path, false)?;
        let inode = self.new_inode(file_type::REGULAR, mode);
        let overlay = self.overlay_of(directory)?;
        let created = overlay.create(directory.inode, &name, inode, Some(Vec::new()))?;
        Ok(Vnode::new(directory.mount, created))
    }

    pub(crate) fn mkdir(&mut self, arguments: Arguments) -> Outcome {
        self.mkdirat(Arguments::new([
            at::FDCWD,
            arguments.get(0),
            arguments.get(1),
            0,
            0,
            0,
        ]))
    }

    pub(crate) fn mkdirat(&mut self, arguments: Arguments) -> Outcome {
        let mode = arguments.get(2) as u32;
        Outcome::Done(
            match self.make_directory(arguments.get(0), arguments.get(1), mode) {
                Ok(()) => 0,
                Err(errno) => errno.as_result(),
            },
        )
    }

    fn make_directory(&mut self, dirfd: i64, path: i64, mode: u32) -> Result<(), Errno> {
        let (directory, name) = self.parent_of(dirfd, path, false)?;
        // A name that is already there is `EEXIST`, whatever kind of thing
        // it is — asked before anything is copied up, so that a refusal
        // leaves the tree exactly as it was.
        if self.exists(directory, &name)? {
            return Err(Errno::Exists);
        }
        let directory = self.copy_up(directory)?;
        let inode = self.new_inode(file_type::DIRECTORY, mode);
        let overlay = self.overlay_of(directory)?;
        overlay.create(directory.inode, &name, inode, None)?;
        Ok(())
    }

    fn exists(&self, directory: Vnode, name: &[u8]) -> Result<bool, Errno> {
        let inode = self.vfs.inode(directory)?;
        Ok(self
            .vfs
            .filesystem_of(directory)?
            .lookup(&inode, directory.inode, &name)?
            .is_some())
    }

    pub(crate) fn rmdir(&mut self, arguments: Arguments) -> Outcome {
        Outcome::Done(
            match self.remove(at::FDCWD, arguments.get(0), at::REMOVEDIR) {
                Ok(()) => 0,
                Err(errno) => errno.as_result(),
            },
        )
    }

    pub(crate) fn unlink(&mut self, arguments: Arguments) -> Outcome {
        Outcome::Done(match self.remove(at::FDCWD, arguments.get(0), 0) {
            Ok(()) => 0,
            Err(errno) => errno.as_result(),
        })
    }

    pub(crate) fn unlinkat(&mut self, arguments: Arguments) -> Outcome {
        let flags = arguments.get(2) as i32;
        if flags & !at::REMOVEDIR != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        Outcome::Done(
            match self.remove(arguments.get(0), arguments.get(1), flags) {
                Ok(()) => 0,
                Err(errno) => errno.as_result(),
            },
        )
    }

    /// `unlink` and `rmdir`, which differ only in what they refuse.
    fn remove(&mut self, dirfd: i64, path: i64, flags: i32) -> Result<(), Errno> {
        // `.` names a directory, and which errno says so depends on the row:
        // `rmdir(".")` is `EINVAL` — the name is unusable — and
        // `unlink(".")` is `EISDIR`, because what it found is a directory.
        // Measured; the two answers are how a caller tells the two failures
        // apart.
        if flags & at::REMOVEDIR == 0 {
            let text = self.path_at(path)?;
            let last = text
                .rsplit(|byte| *byte == b'/')
                .find(|part| !part.is_empty());
            if last == Some(b".".as_slice()) {
                return Err(Errno::IsDir);
            }
        }
        let (directory, name) = self.parent_of(dirfd, path, false)?;
        let inode = self.vfs.inode(directory)?;
        let target = self
            .vfs
            .filesystem_of(directory)?
            .lookup(&inode, directory.inode, &name)?
            .ok_or(Errno::NoEntry)?;
        let target = Vnode::new(directory.mount, target);
        let is_directory = self.vfs.inode(target)?.is_directory();

        // The two rows exist to refuse each other's argument: `unlink` on a
        // directory is `EISDIR`, `rmdir` on anything else is `ENOTDIR`.
        // Answering the same for both would let `rm -r` delete a directory
        // it meant to descend into.
        // Something is mounted here: the name belongs to the mount, not to
        // this filesystem. Linux answers `EBUSY`, and removing it would
        // leave the mount attached to a vnode nothing can reach.
        if self.vfs.mounts().mounted_on(target).is_some() {
            return Err(Errno::Busy);
        }
        if flags & at::REMOVEDIR != 0 {
            if !is_directory {
                return Err(Errno::NotDir);
            }
            let overlay = self.overlay_of(target)?;
            if !overlay.is_empty_directory(target.inode)? {
                return Err(Errno::NotEmpty);
            }
        } else if is_directory {
            return Err(Errno::IsDir);
        }

        // Everything that could refuse has refused, so the copy-up that
        // records the deletion is the first change made.
        let directory = self.copy_up(directory)?;
        let overlay = self.overlay_of(directory)?;
        overlay.unlink(directory.inode, &name)?;
        self.reclaim();
        Ok(())
    }

    pub(crate) fn symlink(&mut self, arguments: Arguments) -> Outcome {
        self.symlinkat(Arguments::new([
            arguments.get(0),
            at::FDCWD,
            arguments.get(1),
            0,
            0,
            0,
        ]))
    }

    pub(crate) fn symlinkat(&mut self, arguments: Arguments) -> Outcome {
        let target = match self.path_at(arguments.get(0)) {
            Ok(target) => target.to_vec(),
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        if target.is_empty() {
            return Outcome::Done(Errno::NoEntry.as_result());
        }
        Outcome::Done(
            match self.make_symlink(arguments.get(1), arguments.get(2), target) {
                Ok(()) => 0,
                Err(errno) => errno.as_result(),
            },
        )
    }

    fn make_symlink(&mut self, dirfd: i64, path: i64, target: Vec<u8>) -> Result<(), Errno> {
        let (directory, name) = self.parent_of(dirfd, path, false)?;
        if self.exists(directory, &name)? {
            return Err(Errno::Exists);
        }
        let directory = self.copy_up(directory)?;
        let mut inode = self.new_inode(file_type::SYMLINK, 0o777);
        inode.size = target.len() as u64;
        let overlay = self.overlay_of(directory)?;
        overlay.create(directory.inode, &name, inode, Some(target))?;
        Ok(())
    }

    pub(crate) fn link(&mut self, arguments: Arguments) -> Outcome {
        self.linkat(Arguments::new([
            at::FDCWD,
            arguments.get(0),
            at::FDCWD,
            arguments.get(1),
            0,
            0,
        ]))
    }

    /// `linkat`: a second name for a file that already exists.
    ///
    /// A real hard link in the upper layer — two entries pointing at one
    /// node — which is what makes `nlink` mean something and what the
    /// reclamation below counts.
    ///
    /// A file that is still only in the image is copied up first, and then
    /// both new names are names for the *copy*. Every other name the image
    /// had still resolves to the original below. That is what kernel
    /// overlayfs does with its index feature off, which is the default, and
    /// it is the same rule that makes writing through one name of a
    /// hardlinked image file break the link.
    pub(crate) fn linkat(&mut self, arguments: Arguments) -> Outcome {
        let flags = arguments.get(4) as i32;
        const SUPPORTED: i32 = at::SYMLINK_FOLLOW | at::EMPTY_PATH;
        if flags & !SUPPORTED != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        if flags & at::EMPTY_PATH != 0 {
            // Linking a descriptor rather than a path needs
            // `CAP_DAC_READ_SEARCH` on Linux, and needs a name this kernel
            // did not keep. Refused by name rather than answered.
            return Outcome::Fault(Fault::detailed(
                number::LINKAT,
                arguments,
                "AT_EMPTY_PATH, which links a descriptor whose name this \
                 kernel did not keep",
            ));
        }
        Outcome::Done(
            match self.add_link(
                arguments.get(0),
                arguments.get(1),
                arguments.get(2),
                arguments.get(3),
                flags,
            ) {
                Ok(()) => 0,
                Err(errno) => errno.as_result(),
            },
        )
    }

    fn add_link(
        &mut self,
        from_dirfd: i64,
        from: i64,
        to_dirfd: i64,
        to: i64,
        flags: i32,
    ) -> Result<(), Errno> {
        // `link` does *not* follow a trailing symlink unless asked: the
        // link itself is what gets a second name, which is the opposite of
        // every other row that changes something.
        let follow = flags & at::SYMLINK_FOLLOW != 0;
        let text = self.path_at(from)?;
        let start = self.start_at(from_dirfd, text)?;
        let lookup = Lookup {
            follow_final: follow,
            require_directory: false,
        };
        let source = self.vfs.resolve(start, text, lookup)?;
        if self.vfs.inode(source)?.is_directory() {
            // POSIX forbids a hard link to a directory, and a filesystem
            // that allowed one would have a cycle nothing could walk.
            return Err(Errno::Perm);
        }

        let (directory, name) = self.parent_of(to_dirfd, to, false)?;
        if directory.mount != source.mount {
            return Err(Errno::CrossDevice);
        }
        if self.exists(directory, &name)? {
            return Err(Errno::Exists);
        }

        // The file has to be in the writable layer to gain a name there.
        // Copying it up first is what makes both new names names for one
        // node; the image's other names for it still resolve below.
        let source = self.copy_up_path(from_dirfd, from, follow)?;
        let directory = self.copy_up(directory)?;
        let overlay = self.overlay_of(directory)?;
        overlay.link(directory.inode, &name, source.inode)
    }

    /// Frees what an unlinked file was holding, once nothing can reach it.
    ///
    /// POSIX keeps an unlinked file alive until its last descriptor closes,
    /// so a name going away is not enough. What decides is whether any open
    /// description still names the node — answered by looking, because the
    /// table has at most a thousand entries and this runs on `unlink` and
    /// `close` rather than on anything hot.
    ///
    /// Without this the upper layer never gives anything back: a container
    /// that writes and deletes temporary files holds every byte of them for
    /// as long as it runs.
    pub(crate) fn reclaim(&mut self) {
        let orphans = {
            let Ok(overlay) = self.vfs.filesystem_of_mut(self.vfs.root()) else {
                return;
            };
            let Some(overlay) = overlay.writable() else {
                return;
            };
            overlay.take_orphans()
        };
        for node in orphans {
            if self.files.holds(node) {
                // A descriptor still has it. The name is gone and the bytes
                // are not, which is exactly what an unlinked-but-open file
                // is; `close` runs this again.
                continue;
            }
            let root = self.vfs.root();
            if let Ok(filesystem) = self.vfs.filesystem_of_mut(root)
                && let Some(overlay) = filesystem.writable()
            {
                let _ = overlay.release(node);
            }
        }
    }

    /// The same sweep, for the nodes a closing descriptor may have been the
    /// last holder of.
    pub(crate) fn reclaim_after_close(&mut self, node: Option<crate::mount::Vnode>) {
        let Some(node) = node else {
            return;
        };
        if !crate::overlay::is_upper(node.inode) || self.files.holds(node.inode) {
            return;
        }
        let root = self.vfs.root();
        if let Ok(filesystem) = self.vfs.filesystem_of_mut(root)
            && let Some(overlay) = filesystem.writable()
            && overlay.is_orphaned(node.inode)
        {
            let _ = overlay.release(node.inode);
        }
    }

    // ---- changing contents ---------------------------------------------

    /// The bytes a `write` is being handed, checked against the guest's
    /// memory before anything is changed.
    fn writable_bytes(&self, buffer: i64, count: u64) -> Result<&'static [u8], Errno> {
        // SAFETY: bounds-checked against the guest's current memory size.
        unsafe { self.memory().slice(buffer as u64, count) }
    }

    /// Writes bytes the kernel is holding to a descriptor.
    ///
    /// The ordinary `write` row moves bytes *from guest memory*, which is
    /// what a guest asking to write always has. `sendfile` is the one call
    /// that does not: its source is a file, so the bytes exist here and the
    /// destination has to be reached without pretending they came from a
    /// buffer the guest named.
    pub(crate) fn send_bytes(&mut self, descriptor: i32, bytes: &[u8]) -> Result<u64, Errno> {
        let file = *self.files.description(descriptor)?;
        if file.flags & open_flags::ACCESS_MODE == open_flags::READ_ONLY {
            return Err(Errno::BadFile);
        }
        let path = match file.backing {
            crate::fd::Backing::Console(crate::fd::Console::Output) => {
                crate::paths::CONSOLE_STDOUT
            }
            crate::fd::Backing::Console(crate::fd::Console::Error) => crate::paths::CONSOLE_STDERR,
            crate::fd::Backing::Console(crate::fd::Console::Input) => return Err(Errno::BadFile),
            // `write` never arrives here — its row answers a pipe first,
            // because a pipe write can park and this signature cannot say
            // so. `pwrite` does arrive, and a pipe has no position.
            crate::fd::Backing::Pipe { .. } => return Err(Errno::NotSeekable),
            crate::fd::Backing::Epoll(_) => return Err(Errno::Invalid),
            crate::fd::Backing::Socket(_) => return Err(Errno::NotSeekable),
            crate::fd::Backing::Image(vnode) => {
                let inode = self.vfs.inode(vnode)?;
                return match inode.file_type() {
                    crate::image::file_type::CHARACTER => {
                        self.device_write_bytes(inode.payload, bytes.len() as u64)
                    }
                    crate::image::file_type::REGULAR => {
                        if !crate::overlay::is_upper(vnode.inode) {
                            return Err(Errno::BadFile);
                        }
                        let offset = match file.flags & open_flags::APPEND != 0 {
                            true => self.vfs.inode(vnode)?.size,
                            false => file.offset,
                        };
                        let now = self.now();
                        let overlay = self.overlay_of(vnode)?;
                        let written = overlay.write_at(vnode.inode, offset, bytes)?;
                        overlay.set_times(vnode.inode, now, 0)?;
                        if let Ok(description) = self.files.description_mut(descriptor) {
                            description.offset = offset + written;
                        }
                        Ok(written)
                    }
                    crate::image::file_type::DIRECTORY => Err(Errno::BadFile),
                    _ => Err(Errno::NoDevice),
                };
            }
        };
        match self.store.write(path, bytes) {
            crate::abi::StoreOutcome::Failed => Err(Errno::Io),
            _ => Ok(bytes.len() as u64),
        }
    }

    /// `write` to a regular file in the writable layer.
    pub(crate) fn write_regular(
        &mut self,
        vnode: Vnode,
        flags: i32,
        offset: u64,
        buffer: i64,
        count: u64,
    ) -> Result<u64, Errno> {
        let bytes = self.writable_bytes(buffer, count)?;
        // The descriptor was copied up when it was opened for writing, so
        // there is nothing to copy now — and if it was not, this is a
        // descriptor that should never have been writable.
        if !crate::overlay::is_upper(vnode.inode) {
            return Err(Errno::BadFile);
        }
        let upper = vnode;
        // `O_APPEND` means the offset is the end *at the moment of the
        // write*, not when the descriptor was opened. That atomicity is the
        // whole reason the flag exists — two processes appending to one log
        // must not overwrite each other — and it holds here for free,
        // because a syscall is never interrupted.
        let offset = if flags & open_flags::APPEND != 0 {
            self.vfs.inode(upper)?.size
        } else {
            offset
        };
        let now = self.now();
        let overlay = self.overlay_of(upper)?;
        let written = overlay.write_at(upper.inode, offset, bytes)?;
        overlay.set_times(upper.inode, now, 0)?;
        Ok(written)
    }

    pub(crate) fn pwrite(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        let count = arguments.get(2);
        let offset = arguments.get(3);
        if offset < 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        Outcome::Done(
            match self.write_positioned(fd, arguments.get(1), count, offset) {
                Ok(written) => written as i64,
                Err(errno) => errno.as_result(),
            },
        )
    }

    fn write_positioned(
        &mut self,
        fd: i32,
        buffer: i64,
        count: i64,
        offset: i64,
    ) -> Result<u64, Errno> {
        let file = *self.files.description(fd)?;
        if file.flags & open_flags::PATH != 0 {
            return Err(Errno::BadFile);
        }
        if file.flags & open_flags::ACCESS_MODE == open_flags::READ_ONLY {
            return Err(Errno::BadFile);
        }
        let crate::fd::Backing::Image(vnode) = file.backing else {
            // A console stream has no position, so `pwrite` on one is
            // `ESPIPE` — which is what Linux answers for a pipe.
            return Err(Errno::NotSeekable);
        };
        // SAFETY: checked before anything is written.
        self.memory().check(buffer as u64, count as u64)?;
        let inode = self.vfs.inode(vnode)?;
        if inode.file_type() == file_type::CHARACTER {
            return self.device_write_bytes(inode.payload, count as u64);
        }
        if !inode.is_regular() {
            return Err(Errno::Invalid);
        }
        // `pwrite` ignores `O_APPEND`, which is the difference Linux
        // documents and the reason a caller reaches for it.
        self.write_regular(vnode, 0, offset as u64, buffer, count as u64)
    }

    pub(crate) fn truncate(&mut self, arguments: Arguments) -> Outcome {
        let length = arguments.get(1);
        let path = arguments.get(0);
        Outcome::Done(match self.truncate_path(path, length) {
            Ok(()) => 0,
            Err(errno) => errno.as_result(),
        })
    }

    fn truncate_path(&mut self, path: i64, length: i64) -> Result<(), Errno> {
        if length < 0 {
            return Err(Errno::Invalid);
        }
        let text = self.path_at(path)?;
        let start = self.start_at(at::FDCWD, text)?;
        let vnode = self.vfs.resolve(start, text, Lookup::FOLLOW)?;
        let inode = self.vfs.inode(vnode)?;
        if inode.is_directory() {
            return Err(Errno::IsDir);
        }
        if !inode.is_regular() {
            return Ok(());
        }
        let upper = self.copy_up_path(at::FDCWD, path, true)?;
        self.truncate_vnode(upper, length as u64)
    }

    pub(crate) fn ftruncate(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        let length = arguments.get(1);
        Outcome::Done(match self.truncate_descriptor(fd, length) {
            Ok(()) => 0,
            Err(errno) => errno.as_result(),
        })
    }

    fn truncate_descriptor(&mut self, fd: i32, length: i64) -> Result<(), Errno> {
        if length < 0 {
            return Err(Errno::Invalid);
        }
        let file = *self.files.description(fd)?;
        // An `O_PATH` descriptor is a reference to a file rather than a
        // handle on it: every operation that would touch the file is
        // `EBADF`, which is what it is for.
        if file.flags & open_flags::PATH != 0 {
            return Err(Errno::BadFile);
        }
        if file.flags & open_flags::ACCESS_MODE == open_flags::READ_ONLY {
            // `ftruncate` needs the descriptor to be writable; `truncate`
            // needs the *file* to be, which is a different question and one
            // this kernel does not ask, having no permission model yet.
            return Err(Errno::Invalid);
        }
        let crate::fd::Backing::Image(vnode) = file.backing else {
            return Err(Errno::Invalid);
        };
        self.truncate_vnode(vnode, length as u64)
    }

    fn truncate_vnode(&mut self, upper: Vnode, length: u64) -> Result<(), Errno> {
        let inode = self.vfs.inode(upper)?;
        if inode.is_directory() {
            return Err(Errno::IsDir);
        }
        if !inode.is_regular() {
            // Truncating a device is a no-op that succeeds on Linux.
            return Ok(());
        }
        if !crate::overlay::is_upper(upper.inode) {
            return Err(Errno::ReadOnlyFs);
        }
        let now = self.now();
        let overlay = self.overlay_of(upper)?;
        overlay.truncate(upper.inode, length)?;
        overlay.set_times(upper.inode, now, 0)
    }

    // ---- metadata -------------------------------------------------------

    /// `utimensat`, which is how a `.pyc` gets the timestamp that decides
    /// whether it is stale.
    pub(crate) fn utimensat(&mut self, arguments: Arguments) -> Outcome {
        let flags = arguments.get(3) as i32;
        if flags & !at::SYMLINK_NOFOLLOW != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let result = self.set_times(arguments.get(0), arguments.get(1), arguments.get(2), flags);
        self.finish_change(arguments, result)
    }

    fn set_times(&mut self, dirfd: i64, path: i64, times: i64, flags: i32) -> Result<(), Errno> {
        let vnode = if path == 0 {
            // A null path means the descriptor itself, which is how
            // `futimens` is spelled. There is no final component to decline
            // to follow, so the flag is meaningless and Linux answers
            // `EINVAL` rather than ignoring it.
            if flags & at::SYMLINK_NOFOLLOW != 0 {
                return Err(Errno::Invalid);
            }
            let fd = i32::try_from(dirfd).map_err(|_| Errno::BadFile)?;
            match self.files.description(fd)?.backing {
                crate::fd::Backing::Image(vnode) => vnode,
                crate::fd::Backing::Console(_)
                | crate::fd::Backing::Pipe { .. }
                | crate::fd::Backing::Epoll(_)
                | crate::fd::Backing::Socket(_) => return Ok(()),
            }
        } else {
            let text = self.path_at(path)?;
            let start = self.start_at(dirfd, text)?;
            let lookup = Lookup {
                follow_final: flags & at::SYMLINK_NOFOLLOW == 0,
                require_directory: false,
            };
            self.vfs.resolve(start, text, lookup)?
        };

        // `times` is two `timespec`s: access time then modification time.
        // The image stores one timestamp, so the access half is read and
        // validated and then has nowhere to go — which is the same answer
        // `stat` gives, and it is written down rather than left implicit.
        let (seconds, nanoseconds) = if times == 0 {
            (self.now(), 0)
        } else {
            // SAFETY: bounds-checked against the guest's memory.
            let bytes = unsafe { self.memory().slice(times as u64, 32)? };
            let word =
                |at: usize| i64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"));
            let (access_nanoseconds, seconds, nanoseconds) = (word(8), word(16), word(24));
            for nanoseconds in [access_nanoseconds, nanoseconds] {
                if nanoseconds != utime::NOW
                    && nanoseconds != utime::OMIT
                    && !(0..1_000_000_000).contains(&nanoseconds)
                {
                    return Err(Errno::Invalid);
                }
            }
            match nanoseconds {
                // Nothing to set — but a read-only filesystem still says so,
                // and answering success first would tell a caller the change
                // it did not ask for had happened on a mount that accepts
                // none. The atime half has nowhere to go either way: the
                // image stores one timestamp, which is what `stat` reports
                // for all three.
                utime::OMIT => {
                    if !self.is_writable(vnode) {
                        return Err(Errno::ReadOnlyFs);
                    }
                    return Ok(());
                }
                utime::NOW => (self.now(), 0),
                _ => (seconds, nanoseconds as u32),
            }
        };
        let upper =
            self.copy_up_for_change(dirfd, path, vnode, flags & at::SYMLINK_NOFOLLOW == 0)?;
        let overlay = self.overlay_of(upper)?;
        overlay.set_times(upper.inode, seconds, nanoseconds)
    }

    /// Gets a resolved file into the writable layer so it can be changed.
    ///
    /// A path is enough, because copying a file up needs the name it was
    /// reached through — see [`crate::overlay::Overlay::copy_up_child`]. A
    /// bare descriptor is not, and the two cases are separated here rather
    /// than papered over:
    ///
    /// - a descriptor opened for writing was already copied up at `open`,
    ///   which is when the path was still in hand, so it is upper already;
    /// - a descriptor opened read-only, whose file is still in the image,
    ///   has no way back to a name. Linux allows `fchmod` and `futimens`
    ///   there and this cannot, so it says so by name. Storing the name in
    ///   every open file description would make it work and would put an
    ///   allocation on every `open`; the milestone that needs it can make
    ///   that trade with a caller to justify it.
    fn copy_up_for_change(
        &mut self,
        dirfd: i64,
        path: i64,
        vnode: Vnode,
        follow: bool,
    ) -> Result<Vnode, Errno> {
        if crate::overlay::is_upper(vnode.inode) {
            return Ok(vnode);
        }
        if !self.is_writable(vnode) {
            return Err(Errno::ReadOnlyFs);
        }
        if self.vfs.inode(vnode)?.is_directory() {
            return self.copy_up(vnode);
        }
        if path == 0 {
            return Err(Errno::NameNeeded);
        }
        // These rows change what a trailing symlink points at, unless the
        // caller said not to follow it.
        self.copy_up_path(dirfd, path, follow)
    }

    pub(crate) fn chmod(&mut self, arguments: Arguments) -> Outcome {
        self.fchmodat(Arguments::new([
            at::FDCWD,
            arguments.get(0),
            arguments.get(1),
            0,
            0,
            0,
        ]))
    }

    pub(crate) fn fchmod(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        let mode = arguments.get(1) as u32;
        let file = match self.files.description(fd) {
            Ok(file) => *file,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        if file.flags & open_flags::PATH != 0 {
            return Outcome::Done(Errno::BadFile.as_result());
        }
        let vnode = match file.backing {
            crate::fd::Backing::Image(vnode) => vnode,
            crate::fd::Backing::Console(_)
            | crate::fd::Backing::Pipe { .. }
            | crate::fd::Backing::Epoll(_)
            | crate::fd::Backing::Socket(_) => return Outcome::Done(0),
        };
        let outcome = self.change_mode_at(at::FDCWD, 0, vnode, mode);
        self.finish_change(arguments, outcome)
    }

    pub(crate) fn fchmodat(&mut self, arguments: Arguments) -> Outcome {
        let flags = arguments.get(3) as i32;
        if flags & !at::SYMLINK_NOFOLLOW != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let mode = arguments.get(2) as u32;
        let vnode = {
            let text = match self.path_at(arguments.get(1)) {
                Ok(text) => text,
                Err(errno) => return Outcome::Done(errno.as_result()),
            };
            let start = match self.start_at(arguments.get(0), text) {
                Ok(start) => start,
                Err(errno) => return Outcome::Done(errno.as_result()),
            };
            match self.vfs.resolve(start, text, Lookup::FOLLOW) {
                Ok(vnode) => vnode,
                Err(errno) => return Outcome::Done(errno.as_result()),
            }
        };
        let outcome = self.change_mode_at(arguments.get(0), arguments.get(1), vnode, mode);
        self.finish_change(arguments, outcome)
    }

    /// Turns a change's result into an outcome, and the one error that is
    /// not an errno into the named fault it was always meant to be.
    ///
    /// [`Errno::NameNeeded`] is this kernel saying it cannot reach a file by
    /// descriptor alone. It is not a Linux errno and must never reach the
    /// guest as one — an earlier version returned it as `-1000`, which lands
    /// inside the band a libc reads as an errno and would have become
    /// `errno = 1000`, a value nothing defines.
    fn finish_change(&mut self, arguments: Arguments, result: Result<(), Errno>) -> Outcome {
        match result {
            Ok(()) => Outcome::Done(0),
            Err(Errno::NameNeeded) => Outcome::Fault(Fault::detailed(
                number::FCHMOD,
                arguments,
                "changing a file reached only by a descriptor, whose name this \
                 kernel did not keep",
            )),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    fn change_mode_at(
        &mut self,
        dirfd: i64,
        path: i64,
        vnode: Vnode,
        mode: u32,
    ) -> Result<(), Errno> {
        let upper = self.copy_up_for_change(dirfd, path, vnode, true)?;
        let overlay = self.overlay_of(upper)?;
        overlay.set_mode(upper.inode, mode)
    }

    // ---- moving ----------------------------------------------------------

    pub(crate) fn rename(&mut self, arguments: Arguments) -> Outcome {
        self.renameat2(Arguments::new([
            at::FDCWD,
            arguments.get(0),
            at::FDCWD,
            arguments.get(1),
            0,
            0,
        ]))
    }

    pub(crate) fn renameat(&mut self, arguments: Arguments) -> Outcome {
        self.renameat2(Arguments::new([
            arguments.get(0),
            arguments.get(1),
            arguments.get(2),
            arguments.get(3),
            0,
            0,
        ]))
    }

    pub(crate) fn renameat2(&mut self, arguments: Arguments) -> Outcome {
        let flags = arguments.get(4) as i32;
        if flags != 0 {
            // `RENAME_NOREPLACE`, `RENAME_EXCHANGE` and `RENAME_WHITEOUT`
            // are each a different atomicity promise, and a kernel that
            // accepted the flag without keeping the promise would be worse
            // than one that says it cannot.
            return Outcome::Fault(Fault::detailed(
                number::RENAMEAT2,
                arguments,
                "rename flags, which each promise an atomicity this has not built",
            ));
        }
        Outcome::Done(
            match self.move_name(
                arguments.get(0),
                arguments.get(1),
                arguments.get(2),
                arguments.get(3),
            ) {
                Ok(()) => 0,
                Err(errno) => errno.as_result(),
            },
        )
    }

    fn move_name(
        &mut self,
        from_dirfd: i64,
        from: i64,
        to_dirfd: i64,
        to: i64,
    ) -> Result<(), Errno> {
        // Renaming moves the *name*, symlink included, which is why neither
        // side follows a trailing link.
        let (from_directory, from_name) = self.parent_of(from_dirfd, from, false)?;
        let inode = self.vfs.inode(from_directory)?;
        let source = self
            .vfs
            .filesystem_of(from_directory)?
            .lookup(&inode, from_directory.inode, &from_name)?
            .ok_or(Errno::NoEntry)?;
        let source = Vnode::new(from_directory.mount, source);
        let source_is_directory = self.vfs.inode(source)?.is_directory();

        let (to_directory, to_name) = self.parent_of(to_dirfd, to, false)?;
        if to_directory.mount != from_directory.mount {
            return Err(Errno::CrossDevice);
        }

        // Something else is mounted here, so the name is not this
        // filesystem's to move. Linux answers `EBUSY`, and the alternative
        // is a mount whose covering directory has been renamed out from
        // under it — reachable by nothing, detached forever.
        if self.vfs.mounts().mounted_on(source).is_some() {
            return Err(Errno::Busy);
        }

        // A directory that shows anything from the lower layer cannot move:
        // what would move is the *name*, and every path below it would
        // still resolve through the image at its old place. Kernel
        // overlayfs answers `EXDEV` here too, with `redirect_dir` off —
        // including for a directory that has already been copied up,
        // measured in a container: `mkdir /usr/share/zz` then
        // `rename("/usr/share", …)` is still `EXDEV`. An earlier version
        // tested `is_upper`, which becomes true the moment anything is
        // created inside a lower directory, and so allowed exactly the case
        // overlayfs refuses.
        if source_is_directory && self.shadows_lower(source)? {
            return Err(Errno::CrossDevice);
        }

        // Moving a directory inside itself would splice it into its own
        // subtree: the node ends up its own ancestor, and everything under
        // it becomes unreachable from the root. Linux answers `EINVAL`.
        if source_is_directory && self.is_ancestor(source, to_directory)? {
            return Err(Errno::Invalid);
        }

        // Replacing an existing name is allowed, and the kinds have to
        // match: a directory cannot replace a file, nor a file a directory.
        let destination_inode = self.vfs.inode(to_directory)?;
        if let Some(existing) = self.vfs.filesystem_of(to_directory)?.lookup(
            &destination_inode,
            to_directory.inode,
            &to_name,
        )? {
            let existing = Vnode::new(to_directory.mount, existing);
            if existing == source {
                // Renaming a file to itself changes nothing and succeeds,
                // which is what Linux does.
                return Ok(());
            }
            let existing_is_directory = self.vfs.inode(existing)?.is_directory();
            match (source_is_directory, existing_is_directory) {
                (true, false) => return Err(Errno::NotDir),
                (false, true) => return Err(Errno::IsDir),
                (true, true) => {
                    let overlay = self.overlay_of(existing)?;
                    if !overlay.is_empty_directory(existing.inode)? {
                        return Err(Errno::NotEmpty);
                    }
                }
                (false, false) => {}
            }
        }

        // Both sides have passed every check, so this is the first change.
        let from_directory = self.copy_up(from_directory)?;
        let to_directory = self.copy_up(to_directory)?;
        let overlay = self.overlay_of(from_directory)?;
        overlay.rename(
            from_directory.inode,
            &from_name,
            to_directory.inode,
            &to_name,
        )?;
        // A rename that replaced an existing name removed one, and what it
        // removed may have been the last name a file had.
        self.reclaim();
        Ok(())
    }

    /// Whether a directory still shows anything from the layer below —
    /// either because it has not been copied up, or because it was copied
    /// up as a *merged* directory that reads through to the image.
    fn shadows_lower(&mut self, vnode: Vnode) -> Result<bool, Errno> {
        if !crate::overlay::is_upper(vnode.inode) {
            return Ok(true);
        }
        let overlay = self.overlay_of(vnode)?;
        overlay.shadows_lower(vnode.inode)
    }

    /// Whether `ancestor` is `vnode` or one of the directories above it.
    ///
    /// Walked upward rather than downward: a directory knows its parent, and
    /// the chain is bounded by the depth of the tree.
    fn is_ancestor(&self, ancestor: Vnode, vnode: Vnode) -> Result<bool, Errno> {
        let mut current = vnode;
        for _ in 0..crate::file::PATH_MAX {
            if current == ancestor {
                return Ok(true);
            }
            let parent = Vnode::new(current.mount, self.vfs.inode(current)?.parent);
            if parent == current {
                return Ok(false);
            }
            current = parent;
        }
        // A chain this long is a corrupt tree rather than a deep one.
        Err(Errno::Loop)
    }

    // ---- locking ---------------------------------------------------------

    /// `flock`, as an in-guest lock table.
    ///
    /// One process holds every lock, so every request is uncontended and
    /// granting it is the truth rather than a guess — but the table is real,
    /// because the *other* thing `flock` promises is that a second attempt
    /// on the same open file description replaces the first, and that a
    /// close releases. Both are observable from one process.
    pub(crate) fn flock(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        let operation = arguments.get(1) as i32;
        let kind = operation & !lock::NONBLOCK;
        if !matches!(kind, lock::SHARED | lock::EXCLUSIVE | lock::UNLOCK) {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        match self.files.description(fd) {
            Ok(file) if file.flags & open_flags::PATH != 0 => {
                return Outcome::Done(Errno::BadFile.as_result());
            }
            Ok(_) => {}
            Err(errno) => return Outcome::Done(errno.as_result()),
        }
        Outcome::Done(match self.files.set_lock(fd, kind) {
            Ok(()) => 0,
            Err(errno) => errno.as_result(),
        })
    }

    /// How many bytes the writable layer holds, and how many nodes it has
    /// released. Both exist so a test can assert the layer gives memory
    /// back rather than taking it on trust.
    pub fn held_bytes(&self) -> usize {
        match self.vfs.filesystem_of(self.vfs.root()) {
            Ok(crate::mount::Filesystem::Overlay(overlay)) => overlay.held_bytes(),
            _ => 0,
        }
    }

    pub fn released_bytes(&self) -> usize {
        match self.vfs.filesystem_of(self.vfs.root()) {
            Ok(crate::mount::Filesystem::Overlay(overlay)) => overlay.released(),
            _ => 0,
        }
    }

    /// The clock a new timestamp comes from.
    ///
    /// One second past the epoch is not a real time, and it is not pretending
    /// to be: a container with no clock mount has no clock, `clock_gettime`
    /// is what will answer that question when M6 asks it, and a timestamp
    /// that claimed to be now would be a plausible wrong answer. What
    /// matters for the one caller that depends on timestamps — a `.pyc`
    /// against its source — is that a file written after another compares
    /// later, and a counter gives that.
    fn now(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }
}
