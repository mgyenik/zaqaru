//! The memory rows: `brk`, `mmap`, `munmap`, `mprotect`, `mremap`,
//! `madvise`, `msync`.
//!
//! The arithmetic is in [`crate::space`]; this is where it meets the guest.
//! The division is deliberate — the address space decides *which* bytes are
//! owed zeros or a copy, and this decides what to put there, because it is
//! the half holding a handle on guest memory and on the filesystem.

use crate::abi::Store;
use crate::errno::Errno;
use crate::machine::Machine;
use crate::mount::Vnode;
use crate::space::{Backing, Fill, Request, map, prot};
use crate::syscall::{Arguments, Fault, Kernel, Outcome, number};

impl<S: Store, M: Machine> Kernel<'_, S, M> {
    /// `brk`: where the program break is, or where it should be.
    ///
    /// A request the arena cannot satisfy leaves the break where it was and
    /// answers *that* — which is what Linux does, and what glibc reads as
    /// "the heap is full, use `mmap` from now on". An errno here would be a
    /// divergence from a path every libc takes.
    pub(crate) fn brk(&mut self, arguments: Arguments) -> Outcome {
        let requested = arguments.get(0) as u64;
        let machine = &mut self.machine;
        let pages = &mut self.pages;
        let enforcement = self.enforcement;
        let was = self.space.brk_current();
        let (result, fill) = self.space.brk(requested, &mut |to| {
            crate::syscall::grow_memory(machine, pages, enforcement, to)
        });
        // The break is a bump pointer with no VMA behind it, so the page
        // table cannot be derived from the tree here the way it is
        // everywhere else — the heap is exactly `[brk_start, brk_current)`
        // and this is the only place that knows it moved. Both directions:
        // a shrunk break makes its pages unreachable, as Linux does, so a
        // program that frees its way down and then reads what it freed
        // faults rather than finding its own old bytes.
        match result.cmp(&was) {
            core::cmp::Ordering::Greater => self.pages.protect(
                was,
                result - was,
                targum::space::Protection::READ_WRITE,
            ),
            core::cmp::Ordering::Less => self.pages.unmap(result, was - result),
            core::cmp::Ordering::Equal => {}
        }
        if let Err(errno) = self.zero(fill) {
            return Outcome::Done(errno.as_result());
        }
        Outcome::Done(result as i64)
    }

    pub(crate) fn mmap(&mut self, arguments: Arguments) -> Outcome {
        let hint = arguments.get(0) as u64;
        let length = arguments.get(1) as u64;
        let prot_bits = arguments.get(2) as i32;
        let flags = arguments.get(3) as i32;
        let fd = arguments.get(4) as i32;
        let offset = arguments.get(5);

        // Unknown protection bits are ignored rather than refused, which is
        // what Linux does here and does *not* do in `mprotect`. Measured:
        // `mmap` accepts even `0x80000000`, while `mprotect` answers
        // `EINVAL` for `0x40`. Recording only the three that mean something
        // keeps `/proc/self/maps` honest about what was asked for.
        let prot_bits = prot_bits & prot::ALL;
        if offset < 0 || offset as u64 % crate::space::PAGE != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        // Exactly one of shared and private, which Linux requires.
        match flags & map::TYPE_MASK {
            map::SHARED | map::PRIVATE | map::SHARED_VALIDATE => {}
            _ => return Outcome::Done(Errno::Invalid.as_result()),
        }
        if flags & map::FIXED != 0 && flags & map::FIXED_NOREPLACE != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }

        let anonymous = flags & map::ANONYMOUS != 0;
        let backing = if anonymous {
            // A `PROT_NONE` anonymous mapping is a reservation and nothing
            // else: glibc takes a thread stack's whole extent this way and
            // then maps the usable part over it.
            if prot_bits == prot::NONE {
                Backing::Reservation
            } else {
                Backing::Anonymous
            }
        } else {
            match self.file_backing(fd, offset as u64) {
                Ok(backing) => backing,
                Err(errno) => return Outcome::Done(errno.as_result()),
            }
        };

        // A writable shared file mapping needs write-back, which needs
        // either dirty tracking or a flush at `msync`. Nothing in the
        // target workload creates one, and a mapping that silently dropped
        // the writes would be the worst of the three options.
        if !anonymous && flags & map::TYPE_MASK == map::SHARED && prot_bits & prot::WRITE != 0 {
            return Outcome::Fault(Fault::detailed(
                number::MMAP,
                arguments,
                "a writable shared file mapping, which needs write-back",
            ));
        }

        // Where a translated ELF goes is not the allocator's to choose.
        //
        // This is the run-time half of "prelink at bake". The bake assigned
        // this file a base and translated its code *at* that base, so every
        // internal reference in it — and every entry the exec map holds for
        // it — is an address in that region. A loader asking to map it
        // "anywhere" must be told that address and no other, or it will
        // relocate against a base nothing was translated at and the first
        // call into the library reports a miss for an address that was never
        // the question.
        //
        // The loader's own `MAP_FIXED` carving of the segments then lands
        // where the translation assumed, because it computes those addresses
        // from what this call returned.
        let (hint, flags) = match self.placement(&backing, offset as u64, flags) {
            Ok(placement) => placement.unwrap_or((hint, flags)),
            Err(errno) => return Outcome::Done(errno.as_result()),
        };

        // Executing bytes nobody translated is not something that can be
        // half-done. The design names this a loud error precisely because
        // the alternative is a container that runs until it calls into the
        // mapping and then reports a miss on an address with no story.
        //
        // **This is the ahead-of-time deal, and only that world pays it.**
        // A bake can translate only code that existed at bake time, so a
        // mapping of anything else has nowhere to run — which is why Node,
        // the JVM and every other JITted runtime cannot exist there. The
        // interpreter has no such clause: it executes whatever bytes are at
        // the program counter, including ones the guest wrote a microsecond
        // ago, so under `Mapped` the pages simply get their permission bits
        // and the first fetch decodes them.
        if self.enforcement == crate::syscall::Enforcement::Flat
            && prot_bits & prot::EXEC != 0
            && matches!(backing, Backing::File { .. })
            && !self.is_translated(&backing).unwrap_or(false)
        {
            return Outcome::Fault(Fault::detailed(
                number::MMAP,
                arguments,
                "an executable mapping of a file the bake did not translate, \
                 whose code therefore does not exist in this module",
            ));
        }

        let request = Request {
            hint,
            length,
            prot: prot_bits,
            flags,
            backing,
        };
        let machine = &mut self.machine;
        let pages = &mut self.pages;
        let enforcement = self.enforcement;
        let (address, fill) = match self.space.map(&request, &mut |to| crate::syscall::grow_memory(machine, pages, enforcement, to)) {
            Ok(answer) => answer,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        // The page table learns the mapping before anything is written into
        // it, because everything written into it goes through the table.
        self.sync_pages(address, address + length);
        // Zeroed first, then the file's bytes over the top: the tail of the
        // last page past the file's end reads as zeros on Linux, and a copy
        // alone would leave whatever was there.
        if let Err(errno) = self.zero(fill) {
            return Outcome::Done(errno.as_result());
        }
        if let Err(errno) = self.copy_file_backing(address, length, &request.backing) {
            return Outcome::Done(errno.as_result());
        }
        Outcome::Done(address as i64)
    }

    /// What a descriptor contributes to a mapping: which file, and the name
    /// `/proc/self/maps` will show.
    fn file_backing(&mut self, fd: i32, offset: u64) -> Result<Backing, Errno> {
        let file = *self.files.description(fd)?;
        let crate::fd::Backing::Image(vnode) = file.backing else {
            // A console stream is not mappable, which is what Linux says
            // about a pipe.
            return Err(Errno::NoDevice);
        };
        let inode = self.vfs.inode(vnode)?;
        if inode.is_directory() {
            return Err(Errno::NoDevice);
        }
        if !inode.is_regular() {
            return Err(Errno::NoDevice);
        }
        Ok(Backing::File { vnode, offset })
    }

    /// Whether a mapping's file is an ELF the bake translated.
    fn is_translated(&self, backing: &Backing) -> Result<bool, Errno> {
        let Backing::File { vnode, .. } = backing else {
            return Ok(false);
        };
        Ok(self.vfs.inode(*vnode)?.flags & crate::image::inode_flags::EXEC_TRANSPILED != 0)
    }

    /// Where a translated ELF must be mapped, when the caller did not say.
    ///
    /// `None` means the caller's own hint and flags stand: the file is not a
    /// translated ELF, or the caller used `MAP_FIXED` and has therefore
    /// already computed an address from an earlier answer of ours.
    ///
    /// The address is the base plus the file offset being mapped, which is
    /// the same thing for every well-formed shared object: a loader maps
    /// from the first segment's file offset, and that segment's virtual
    /// address is congruent to it — that congruence is what lets a loader
    /// map a file at all.
    fn placement(
        &self,
        backing: &Backing,
        offset: u64,
        flags: i32,
    ) -> Result<Option<(u64, i32)>, Errno> {
        if flags & map::FIXED != 0 || !self.is_translated(backing)? {
            return Ok(None);
        }
        let Backing::File { vnode, .. } = backing else {
            return Ok(None);
        };
        let Some(base) = self.vfs.filesystem_of(*vnode)?.prelink_base(vnode.inode) else {
            return Ok(None);
        };
        Ok(Some((base + offset, flags | map::FIXED)))
    }

    /// Copies a file's bytes into a mapping that was just made.
    ///
    /// Eagerly, and for every kind of file. POSIX leaves post-map visibility
    /// of writes unspecified for a private mapping, so a copy is conformant;
    /// the alternative — pointing the guest at the shared image blob — has
    /// no answer for a later `mprotect(PROT_WRITE)`, which Linux permits.
    fn copy_file_backing(
        &mut self,
        address: u64,
        length: u64,
        backing: &Backing,
    ) -> Result<(), Errno> {
        let Backing::File { vnode, offset, .. } = backing else {
            return Ok(());
        };
        let inode = self.vfs.inode(*vnode)?;
        let contents = self
            .vfs
            .filesystem_of(*vnode)?
            .contents(&inode, vnode.inode)?;
        let from = (*offset).min(contents.len() as u64);
        let to = from.saturating_add(length).min(contents.len() as u64);
        if to <= from {
            return Ok(());
        }
        let slice = &contents[from as usize..to as usize];
        // The kernel's own write: a guest may map a file read-only, and the
        // bytes still have to be there when it looks. The address space by
        // its field because `contents` still borrows the filesystem, and the
        // two are disjoint parts of the kernel.
        crate::memory::GuestMemory::new(&mut self.pages).place(address, slice)
    }

    pub(crate) fn munmap(&mut self, arguments: Arguments) -> Outcome {
        let start = arguments.get(0) as u64;
        let length = arguments.get(1) as u64;
        Outcome::Done(match self.space.unmap(start, length) {
            Ok(()) => {
                self.sync_pages(start, start + length);
                0
            }
            Err(errno) => errno.as_result(),
        })
    }

    pub(crate) fn mprotect(&mut self, arguments: Arguments) -> Outcome {
        let start = arguments.get(0) as u64;
        let length = arguments.get(1) as u64;
        let prot_bits = arguments.get(2) as i32;
        Outcome::Done(match self.space.protect(start, length, prot_bits) {
            Ok(()) => {
                self.sync_pages(start, start + length);
                0
            }
            Err(errno) => errno.as_result(),
        })
    }

    pub(crate) fn madvise(&mut self, arguments: Arguments) -> Outcome {
        let start = arguments.get(0) as u64;
        let length = arguments.get(1) as u64;
        let what = arguments.get(2) as i32;
        let fill = match self.space.advise(start, length, what) {
            Ok(fill) => fill,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        // Advice can split a mapping, and a split is a change to the tree.
        self.sync_pages(start, start + length);
        Outcome::Done(match self.zero(fill) {
            Ok(()) => 0,
            Err(errno) => errno.as_result(),
        })
    }

    pub(crate) fn mremap(&mut self, arguments: Arguments) -> Outcome {
        let start = arguments.get(0) as u64;
        let old_length = arguments.get(1) as u64;
        let new_length = arguments.get(2) as u64;
        let flags = arguments.get(3) as i32;
        let machine = &mut self.machine;
        let pages = &mut self.pages;
        let enforcement = self.enforcement;
        let moved = match self
            .space
            .remap(start, old_length, new_length, flags, &mut |to| {
                crate::syscall::grow_memory(machine, pages, enforcement, to)
            }) {
            Ok(moved) => moved,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        // Both ends: the range it left, which may now be a hole, and the
        // range it arrived at. Growing in place moves only the second.
        self.sync_pages(start, start + old_length.max(new_length));
        self.sync_pages(moved.to, moved.to + moved.length.max(new_length));
        if moved.to != moved.from && moved.length > 0 {
            // The kernel moving its own mapping, not a copy on the guest's
            // behalf — the source mapping does not exist any more, because
            // the tree has already moved it, so asking whether the guest may
            // read there would be asking about a mapping that was released a
            // few lines ago. That question used to be answerable because the
            // check only asked "is this inside linear memory"; now that it
            // asks "is this mapped", the honest answer is no and the copy
            // has to say what it actually is.
            if let Err(errno) = self.pages.relocate(moved.to, moved.from, moved.length) {
                return Outcome::Done(crate::errno::Errno::from(errno).as_result());
            }
        }
        if let Err(errno) = self.zero(moved.fill) {
            return Outcome::Done(errno.as_result());
        }
        Outcome::Done(moved.to as i64)
    }

    /// `msync`, which has nothing to flush.
    ///
    /// Every mapping is a copy, and nothing creates a writable shared one —
    /// that case is a named fault at `mmap`. So there is no write-back to
    /// perform and success is the honest answer, not a stub.
    pub(crate) fn msync(&mut self, arguments: Arguments) -> Outcome {
        let start = arguments.get(0) as u64;
        if start % crate::space::PAGE != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        Outcome::Done(0)
    }

    /// Zeroing a range the kernel is handing over, which is the kernel's
    /// own write rather than the guest's: the mapping may be read-only, and
    /// it still has to arrive zeroed.
    fn zero(&mut self, fill: Fill) -> Result<(), Errno> {
        if fill.length == 0 {
            return Ok(());
        }
        self.memory_mut().place_fill(fill.start, fill.length, 0)
    }

    /// `/proc/self/maps`, rendered from the tree on each read.
    ///
    /// Generated rather than stored because it is a *view*: glibc's
    /// `pthread_getattr_np` reads it to find a thread's stack bounds, and a
    /// snapshot taken at boot would describe an address space that no longer
    /// exists.
    ///
    /// The format is fixed by what that parser accepts —
    /// `start-end perms offset dev:dev inode path`, one line per mapping, in
    /// address order.
    /// Public because a profile is addresses and a reader needs names: the
    /// engine counts where a guest spent itself and only the kernel knows
    /// what is mapped there.
    pub fn render_maps(&self) -> String {
        let mut rendered = String::new();
        for vma in self.space.vmas() {
            hex(&mut rendered, vma.start);
            rendered.push('-');
            hex(&mut rendered, vma.end());
            rendered.push(' ');
            rendered.push(if vma.prot & prot::READ != 0 { 'r' } else { '-' });
            rendered.push(if vma.prot & prot::WRITE != 0 {
                'w'
            } else {
                '-'
            });
            rendered.push(if vma.prot & prot::EXEC != 0 { 'x' } else { '-' });
            // Every mapping is private: nothing creates a writable shared
            // one, and a read-only shared mapping is indistinguishable from
            // a private one here because both are copies.
            rendered.push_str("p ");
            match &vma.backing {
                Backing::File { vnode, offset } => {
                    padded_hex(&mut rendered, *offset, 8);
                    rendered.push(' ');
                    // The mount, so that two files from different
                    // filesystems do not look like one file.
                    padded_hex(&mut rendered, vnode.mount as u64, 2);
                    rendered.push(':');
                    padded_hex(&mut rendered, 0, 2);
                    rendered.push(' ');
                    decimal(&mut rendered, vnode.inode as u64);
                    rendered.push_str("                       ");
                    self.name_of(*vnode, &mut rendered);
                }
                Backing::Anonymous | Backing::Reservation => {
                    rendered.push_str("00000000 00:00 0 ");
                }
            }
            rendered.push('\n');
        }
        rendered
    }

    /// The path a mapped file has, found by searching for it.
    ///
    /// Searched rather than remembered, and the trade is deliberate. A file
    /// does not know its own name — the index gives a parent pointer to
    /// directories only, because a file can have several names — so the
    /// alternatives are to keep the name at `open`, which puts an
    /// allocation on a path the design promises is allocation-free, or to
    /// look it up here. This runs when something reads `/proc/self/maps`,
    /// which a process does once or twice; `open` runs thousands of times.
    fn name_of(&self, vnode: Vnode, into: &mut String) {
        let root = self.vfs.root();
        if root.mount != vnode.mount {
            // A file in another mount is reachable only through that
            // mount's own point, which this does not walk. Its inode number
            // is still in the line above.
            return;
        }
        let at = into.len();
        if !self.search(root, vnode, into) {
            into.truncate(at);
        }
    }

    /// Depth-first, appending each component as it descends and taking it
    /// back off when the branch does not contain the file.
    fn search(&self, directory: Vnode, wanted: Vnode, into: &mut String) -> bool {
        let Ok(inode) = self.vfs.inode(directory) else {
            return false;
        };
        let Ok(filesystem) = self.vfs.filesystem_of(directory) else {
            return false;
        };
        let Ok(count) = filesystem.entry_count(&inode, directory.inode) else {
            return false;
        };
        for position in 0..count {
            let Ok(entry) = filesystem.entry(&inode, directory.inode, position) else {
                return false;
            };
            let at = into.len();
            into.push('/');
            into.push_str(&String::from_utf8_lossy(entry.name));
            let child = Vnode::new(directory.mount, entry.inode);
            if child == wanted {
                return true;
            }
            if let Ok(child_inode) = self.vfs.inode(child)
                && child_inode.is_directory()
                && self.search(child, wanted, into)
            {
                return true;
            }
            into.truncate(at);
        }
        false
    }
}

fn hex(into: &mut String, value: u64) {
    if value == 0 {
        into.push('0');
        return;
    }
    let mut digits = [0u8; 16];
    let mut length = 0;
    let mut value = value;
    while value > 0 {
        digits[length] = b"0123456789abcdef"[(value & 0xf) as usize];
        value >>= 4;
        length += 1;
    }
    for index in (0..length).rev() {
        into.push(digits[index] as char);
    }
}

fn padded_hex(into: &mut String, value: u64, width: usize) {
    let mut rendered = String::new();
    hex(&mut rendered, value);
    for _ in rendered.len()..width {
        into.push('0');
    }
    into.push_str(&rendered);
}

fn decimal(into: &mut String, value: u64) {
    if value == 0 {
        into.push('0');
        return;
    }
    let mut digits = [0u8; 20];
    let mut length = 0;
    let mut value = value;
    while value > 0 {
        digits[length] = b'0' + (value % 10) as u8;
        value /= 10;
        length += 1;
    }
    for index in (0..length).rev() {
        into.push(digits[index] as char);
    }
}
