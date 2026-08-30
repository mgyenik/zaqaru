//! The guest address space: linear memory, with permissions and a record of
//! which pages hold code.
//!
//! A guest virtual address *is* an offset into linear memory, which is the
//! arrangement the transpiler already has and the reason `/proc/self/maps`
//! can be honest and `AT_PHDR` can be real. What is new here is that the
//! address space has *permissions*. Four bitmaps, one bit per four-kilobyte
//! page, decide what an access does:
//!
//! - `readable`, `writable`, `executable` — maintained by the `mmap`,
//!   `mprotect` and `munmap` rows, checked on the load, store and fetch
//!   paths. A wild access is a [`Fault`], which the loop turns into a real
//!   `SIGSEGV` delivered to the guest with a faithful `si_addr`. The
//!   transpiler documents this fidelity class as impossible: a null deref
//!   reads garbage there, guard pages cannot be enforced, and a stack
//!   overflow corrupts silently.
//! - `code` — set for every page a cached block took bytes from, and tested
//!   on every store. It is what makes self-modifying code and runtime code
//!   generation *correct* rather than forbidden.
//!
//! **This module is the choke point for guest memory, and that is a
//! correctness property, not a style rule.** The set of writers to guest
//! memory is closed — the interpreter's stores, the kernel's helpers, and
//! the mapping rows — and every one of them has to pass through here, or a
//! cached block goes on executing bytes that no longer exist. A new writer
//! that reaches around this module is a silent staleness bug, so reaching
//! around it should be structurally hard: nothing outside this module turns
//! a guest address into a pointer.

use crate::state::Width;

/// The page size everything here is denominated in. Not a tunable: it is the
/// granularity Linux's `mprotect` works at, so the bitmaps have to match it
/// or the kernel rows could not be expressed.
pub const PAGE_SIZE: u64 = 4096;
pub const PAGE_SHIFT: u32 = 12;

/// One past the highest address the machine can have.
///
/// The wasm32 ceiling, and so a property of the machine rather than of any
/// particular host: linear memory is indexed by an `i32`, so four gigabytes
/// is the whole address space. `docs/vm.md` states it as a boundary of the
/// design — "v1 of this design is a 4 GiB machine" — and the bitmaps below
/// are sized against it, so a limit beyond it is a mistake rather than an
/// expensive request.
pub const CEILING: u64 = 1 << 32;

/// What an access was trying to do, so a fault can say so.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Read,
    Write,
    /// An instruction fetch. Distinguished because Linux's page-fault error
    /// code distinguishes it, and because "jumped into a data page" is a
    /// different bug report from "read a wild pointer".
    Fetch,
}

/// An access the address space refuses.
///
/// This is the whole of what a memory error is here: an address and what was
/// attempted. The loop turns it into a signal; nothing in this module knows
/// what a signal is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fault {
    pub address: u64,
    pub access: Access,
}

/// Why a C string was not a C string.
///
/// Two answers, because the kernel gives two different errnos: a string
/// that ran off the end of what the guest may read is `EFAULT`, and one
/// that reached the caller's bound without a terminator is `ENAMETOOLONG`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unterminated {
    Fault,
    TooLong,
}

/// What a mapping allows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Protection {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl Protection {
    pub const NONE: Self = Self {
        read: false,
        write: false,
        execute: false,
    };
    pub const READ: Self = Self {
        read: true,
        write: false,
        execute: false,
    };
    pub const READ_WRITE: Self = Self {
        read: true,
        write: true,
        execute: false,
    };
    pub const READ_EXECUTE: Self = Self {
        read: true,
        write: false,
        execute: true,
    };
    pub const ALL: Self = Self {
        read: true,
        write: true,
        execute: true,
    };
}

/// One bit per page, grown on demand.
#[derive(Default, Clone)]
struct Bitmap {
    words: Vec<u64>,
}

impl Bitmap {
    fn reserve(&mut self, pages: usize) {
        let words = pages.div_ceil(64);
        if self.words.len() < words {
            self.words.resize(words, 0);
        }
    }

    #[inline]
    fn get(&self, page: usize) -> bool {
        match self.words.get(page / 64) {
            Some(word) => word & (1u64 << (page % 64)) != 0,
            None => false,
        }
    }

    #[inline]
    fn set(&mut self, page: usize, value: bool) {
        self.reserve(page + 1);
        let word = &mut self.words[page / 64];
        let bit = 1u64 << (page % 64);
        match value {
            true => *word |= bit,
            false => *word &= !bit,
        }
    }
}

/// The guest's linear memory, and what may be done with each page of it.
#[derive(Clone)]
pub struct Space {
    /// One past the highest addressable byte. Linear memory grows in
    /// 64 KiB pages and never shrinks, so this only ever rises.
    limit: u64,
    readable: Bitmap,
    writable: Bitmap,
    executable: Bitmap,
    /// Pages a cached block took bytes from.
    code: Bitmap,
    /// Code pages a store has landed on since the last drain. The store path
    /// pushes here and clears the page's `code` bit; the run loop drains the
    /// list into the block cache before the next instruction is fetched, so
    /// no stale byte is ever executed.
    dirty: Vec<u64>,
}

impl Default for Space {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Space {
    /// An address space of `limit` bytes with nothing mapped in it. Every
    /// access faults until a mapping row says otherwise, which is the state
    /// a fresh process starts in.
    pub fn new(limit: u64) -> Self {
        let mut space = Self {
            limit: 0,
            readable: Bitmap::default(),
            writable: Bitmap::default(),
            executable: Bitmap::default(),
            code: Bitmap::default(),
            dirty: Vec::new(),
        };
        space.set_limit(limit);
        space
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Records that linear memory now reaches `limit`.
    ///
    /// Growth is the embedder's business — `memory.grow` inside the module,
    /// an arena commit natively — because it is the same operation the
    /// kernel's `brk` and `mmap` rows already own. What this does is size the
    /// bitmaps to match, so a page that has just come into existence has an
    /// answer for every question asked about it.
    pub fn set_limit(&mut self, limit: u64) {
        assert!(limit >= self.limit, "linear memory never shrinks");
        assert!(
            limit <= CEILING,
            "a limit of {limit:#x} is past the four-gigabyte ceiling this \
             machine has; see `CEILING`"
        );
        self.limit = limit;
        let pages = usize::try_from(limit >> PAGE_SHIFT).expect("a 32-bit page count");
        self.readable.reserve(pages);
        self.writable.reserve(pages);
        self.executable.reserve(pages);
        self.code.reserve(pages);
    }

    /// Gives a page-aligned range a protection, as `mmap` and `mprotect` do.
    ///
    /// Unaligned ends are rounded outwards, because that is what Linux does:
    /// a mapping of one byte occupies a whole page and the whole page takes
    /// the protection.
    pub fn protect(&mut self, address: u64, length: u64, protection: Protection) {
        for page in Self::pages_of(address, length) {
            self.readable.set(page, protection.read);
            self.writable.set(page, protection.write);
            self.executable.set(page, protection.execute);
        }
    }

    /// Takes a range out of the address space entirely, as `munmap` does.
    ///
    /// The code bits go with it: bytes that are no longer mapped cannot be
    /// the source of a cached block, so the pages are queued for
    /// invalidation exactly as a write to them would be. Forgetting this is
    /// the unmap-then-remap staleness bug, which is the same silent class as
    /// a missed store.
    pub fn unmap(&mut self, address: u64, length: u64) {
        for page in Self::pages_of(address, length) {
            self.readable.set(page, false);
            self.writable.set(page, false);
            self.executable.set(page, false);
            if self.code.get(page) {
                self.code.set(page, false);
                self.dirty.push(page as u64);
            }
        }
    }

    fn pages_of(address: u64, length: u64) -> std::ops::Range<usize> {
        if length == 0 {
            return 0..0;
        }
        let first = (address >> PAGE_SHIFT) as usize;
        let last = ((address + length - 1) >> PAGE_SHIFT) as usize;
        first..last + 1
    }

    // ---- the code bitmap -------------------------------------------------

    /// Marks a page as holding bytes some cached block was decoded from.
    pub fn mark_code(&mut self, page: u64) {
        self.code.set(page as usize, true);
    }

    /// Forgets that a page holds code, once no block is registered to it.
    pub fn clear_code(&mut self, page: u64) {
        self.code.set(page as usize, false);
    }

    /// Whether any store has landed on a code page since the last drain.
    #[inline]
    pub fn has_dirty_code(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// Hands over the pages a store has invalidated, emptying the list.
    pub fn take_dirty_code(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.dirty)
    }

    /// What a page allows, for the kernel's own consistency check.
    /// Every page that has any protection at all, in ascending order.
    ///
    /// What a *dormant* process's bytes are, inside the module: linear
    /// memory there holds the engine's own data as well as the guest's, and
    /// `baker::layout` interleaves them — the program's segments low, the
    /// module's data above them, the arenas above that. So "the guest's
    /// bytes" cannot be a range. It is exactly the set of pages the guest's
    /// page table describes, which is what this is.
    ///
    /// A `PROT_NONE` page is deliberately not in it. Nothing can reach one
    /// until an `mprotect` gives it a protection, and a fresh anonymous page
    /// reads as zeroes when it does — so there is nothing there to carry.
    pub fn mapped_pages(&self) -> impl Iterator<Item = u64> + '_ {
        let count = (self.limit >> PAGE_SHIFT) as usize;
        (0..count)
            .filter(|&page| {
                self.readable.get(page) || self.writable.get(page) || self.executable.get(page)
            })
            .map(|page| (page as u64) << PAGE_SHIFT)
    }

    pub fn protection(&self, address: u64) -> Protection {
        let page = (address >> PAGE_SHIFT) as usize;
        Protection {
            read: self.readable.get(page),
            write: self.writable.get(page),
            execute: self.executable.get(page),
        }
    }

    // ---- access ----------------------------------------------------------

    /// Whether `length` bytes at `address` may be accessed as `access` says.
    ///
    /// The page loop is one iteration for every access that does not straddle
    /// a page boundary, which is nearly all of them.
    fn permitted(&self, address: u64, length: u64, access: Access) -> Result<(), Fault> {
        if length == 0 {
            return Ok(());
        }
        let end = match address.checked_add(length) {
            Some(end) if end <= self.limit => end,
            // Off the end of linear memory, or wrapped. Either way the guest
            // named an address that does not exist.
            _ => return Err(Fault { address, access }),
        };
        let map = match access {
            Access::Read => &self.readable,
            Access::Write => &self.writable,
            Access::Fetch => &self.executable,
        };
        let first = (address >> PAGE_SHIFT) as usize;
        let last = ((end - 1) >> PAGE_SHIFT) as usize;
        for page in first..=last {
            if !map.get(page) {
                // The reported address is the faulting *byte*, not the start
                // of the access: a `si_addr` naming the first page of a
                // straddling access would send a fault handler to the wrong
                // page.
                let at = (page as u64) << PAGE_SHIFT;
                return Err(Fault {
                    address: at.max(address),
                    access,
                });
            }
        }
        Ok(())
    }

    /// Whether `length` bytes at `address` may be read, for the kernel rows
    /// that need the answer without the bytes.
    pub fn check(&self, address: u64, length: u64, access: Access) -> Result<(), Fault> {
        self.permitted(address, length, access)
    }

    /// Turns a checked guest address into a host pointer.
    ///
    /// Inside the module this is the identity, because linear memory is the
    /// address space; natively it is the identity too, because the arena is
    /// mapped at the addresses the guest uses. That is not a coincidence to
    /// be relied on quietly — it is what lets the same interpreter run under
    /// wasmtime and under a native lockstep harness with no second address
    /// model, so it is stated here once.
    ///
    /// # Safety
    /// The caller must have established that the range is inside the address
    /// space and carries the permission the access needs.
    #[inline]
    unsafe fn pointer(address: u64) -> *mut u8 {
        address as usize as *mut u8
    }

    /// An integer load of one, two, four or eight bytes, zero-extended.
    #[inline]
    pub fn load(&self, address: u64, width: Width) -> Result<u64, Fault> {
        self.permitted(address, u64::from(width.bytes()), Access::Read)?;
        // SAFETY: checked immediately above.
        unsafe {
            let at = Self::pointer(address);
            Ok(match width {
                Width::Byte => u64::from(at.read()),
                Width::Word => u64::from(at.cast::<u16>().read_unaligned().to_le()),
                Width::Dword => u64::from(at.cast::<u32>().read_unaligned().to_le()),
                Width::Qword => at.cast::<u64>().read_unaligned().to_le(),
            })
        }
    }

    /// An integer store. Every store in the engine reaches memory through
    /// here, which is what makes the code-page test unavoidable.
    #[inline]
    pub fn store(&mut self, address: u64, width: Width, value: u64) -> Result<(), Fault> {
        self.permitted(address, u64::from(width.bytes()), Access::Write)?;
        self.note_code_write(address, u64::from(width.bytes()));
        // SAFETY: checked immediately above.
        unsafe {
            let at = Self::pointer(address);
            match width {
                Width::Byte => at.write(value as u8),
                Width::Word => at.cast::<u16>().write_unaligned((value as u16).to_le()),
                Width::Dword => at.cast::<u32>().write_unaligned((value as u32).to_le()),
                Width::Qword => at.cast::<u64>().write_unaligned(value.to_le()),
            }
        }
        Ok(())
    }

    /// Reads a run of bytes — a vector move, a string operation, a kernel
    /// row copying out of the guest.
    pub fn read(&self, address: u64, into: &mut [u8]) -> Result<(), Fault> {
        self.permitted(address, into.len() as u64, Access::Read)?;
        // SAFETY: checked immediately above.
        unsafe {
            Self::pointer(address).copy_to(into.as_mut_ptr(), into.len());
        }
        Ok(())
    }

    /// Writes a run of bytes.
    ///
    /// `copy_from`, not the non-overlapping form: a kernel row's source can
    /// be the image blob, which lives in the same linear memory as the
    /// destination, and a guest is free to hand `read(2)` a buffer that
    /// overlaps the file it is reading.
    pub fn write(&mut self, address: u64, from: &[u8]) -> Result<(), Fault> {
        self.permitted(address, from.len() as u64, Access::Write)?;
        self.note_code_write(address, from.len() as u64);
        // SAFETY: checked immediately above.
        unsafe {
            Self::pointer(address).copy_from(from.as_ptr(), from.len());
        }
        Ok(())
    }

    /// Writes bytes the *kernel* owns, without asking the guest's
    /// permissions.
    ///
    /// The distinction Linux makes and this has to make too: a write on the
    /// guest's behalf — `read(2)` filling a user buffer, a signal frame — is
    /// a user access and answers to the guest's protections, so writing a
    /// read-only page is `EFAULT`. *Populating* memory the kernel is in the
    /// middle of handing over is not a user access at all: zero-filling a
    /// fresh mapping, copying a file's bytes into one the guest asked to be
    /// read-only, loading a program's text. A real kernel does those through
    /// the direct map, where the process's page table has no say.
    ///
    /// What it does **not** skip is the invalidation hook. That is the whole
    /// reason this lives here rather than being a raw pointer at the call
    /// site: the closed set of writers stays closed, and a mapping populated
    /// over a page some block was decoded from still drops that block.
    pub fn place(&mut self, address: u64, from: &[u8]) -> Result<(), Fault> {
        self.within(address, from.len() as u64)?;
        self.note_code_write(address, from.len() as u64);
        // SAFETY: checked to be inside linear memory immediately above.
        unsafe {
            Self::pointer(address).copy_from(from.as_ptr(), from.len());
        }
        Ok(())
    }

    /// Moves a run of bytes the kernel owns, from one of its mappings to
    /// another.
    ///
    /// `mremap`, and nothing else. Like [`Space::place`] it does not consult
    /// the guest's protections, and for a sharper reason than population
    /// does: by the time the bytes are copied the *source* mapping no longer
    /// exists — the tree has already moved it — so asking whether the guest
    /// may read there would be asking about a mapping that has been
    /// released. The bytes are still in linear memory, which never shrinks,
    /// and that is the only thing worth checking.
    ///
    /// Overlapping-safe: a grown mapping may land on a range the old one
    /// touched.
    pub fn relocate(&mut self, to: u64, from: u64, length: u64) -> Result<(), Fault> {
        self.within(from, length)?;
        self.within(to, length)?;
        self.note_code_write(to, length);
        // SAFETY: both ranges are inside linear memory, checked above, and
        // `copy` is defined for overlapping ranges.
        unsafe {
            Self::pointer(to).copy_from(Self::pointer(from), length as usize);
        }
        Ok(())
    }

    /// As [`Space::place`], for a run of one byte.
    pub fn place_fill(&mut self, address: u64, length: u64, byte: u8) -> Result<(), Fault> {
        self.within(address, length)?;
        self.note_code_write(address, length);
        // SAFETY: as above.
        unsafe {
            Self::pointer(address).write_bytes(byte, length as usize);
        }
        Ok(())
    }

    /// Whether a range is inside linear memory at all, which is the only
    /// question a kernel-owned write asks.
    fn within(&self, address: u64, length: u64) -> Result<(), Fault> {
        if length == 0 {
            return Ok(());
        }
        match address.checked_add(length) {
            Some(end) if end <= self.limit && address != 0 => Ok(()),
            _ => Err(Fault {
                address,
                access: Access::Write,
            }),
        }
    }

    /// Fills a range with a byte — `MADV_DONTNEED`'s zeroing, an anonymous
    /// mapping's, a `memset` the kernel does on the guest's behalf.
    pub fn fill(&mut self, address: u64, length: u64, byte: u8) -> Result<(), Fault> {
        self.permitted(address, length, Access::Write)?;
        self.note_code_write(address, length);
        // SAFETY: checked immediately above.
        unsafe {
            Self::pointer(address).write_bytes(byte, length as usize);
        }
        Ok(())
    }

    /// A borrowed view of a readable range.
    ///
    /// The one place this module hands out a reference rather than copying,
    /// because the kernel's rows need one: a path, an `iovec` array, a
    /// buffer being written out. The permission check is the same check
    /// every other access makes, so what the caller gets is a range the
    /// guest was allowed to read.
    ///
    /// # Safety
    /// The lifetime is the caller's to choose, and linear memory outlives
    /// any of them — but the guest can *write* the range while the
    /// reference is live, so a caller must not hold one across anything
    /// that lets the guest run.
    pub unsafe fn slice<'a>(&self, address: u64, length: u64) -> Result<&'a [u8], Fault> {
        self.permitted(address, length, Access::Read)?;
        if length == 0 {
            return Ok(&[]);
        }
        // SAFETY: the range is inside the address space and readable.
        unsafe {
            Ok(std::slice::from_raw_parts(
                Self::pointer(address),
                length as usize,
            ))
        }
    }

    /// A NUL-terminated string at a guest address — a path, an attribute
    /// name, anything the guest hands over as a C string.
    ///
    /// Bounded twice: by what the guest may read, and by `limit`, which
    /// callers set to whatever POSIX maximum applies. A string with no
    /// terminator inside the bound is a length error, which is emphatically
    /// not the same as reading until something happens to be zero.
    ///
    /// The scan stops at the end of the readable run rather than at the end
    /// of the address space, which is what makes an unterminated string at
    /// the edge of a mapping a fault instead of a walk into whatever is
    /// mapped next.
    ///
    /// # Safety
    /// As [`Space::slice`].
    pub unsafe fn c_string<'a>(
        &self,
        address: u64,
        limit: usize,
    ) -> Result<Result<&'a [u8], Unterminated>, Fault> {
        let available = self.readable_run(address, limit as u64);
        if available == 0 {
            self.permitted(address, 1, Access::Read)?;
            return Ok(Err(Unterminated::TooLong));
        }
        // SAFETY: the run is readable by construction.
        let bytes = unsafe { self.slice(address, available)? };
        Ok(match bytes.iter().position(|byte| *byte == 0) {
            Some(end) => Ok(&bytes[..end]),
            // The run ended before the caller's bound did, so the string
            // ran off the end of what the guest may read.
            None if available < limit as u64 => Err(Unterminated::Fault),
            None => Err(Unterminated::TooLong),
        })
    }

    /// How many bytes from `address` are readable, up to `limit`.
    fn readable_run(&self, address: u64, limit: u64) -> u64 {
        if address == 0 || limit == 0 || address >= self.limit {
            return 0;
        }
        let ceiling = address.saturating_add(limit).min(self.limit);
        let mut end = address;
        while end < ceiling && self.readable.get((end >> PAGE_SHIFT) as usize) {
            end = ((end >> PAGE_SHIFT) + 1) << PAGE_SHIFT;
        }
        end.min(ceiling).saturating_sub(address)
    }

    /// Fetches instruction bytes, which is a read with the *execute*
    /// permission — the check no ahead-of-time design can make, because it
    /// has no fetch to hang it on.
    ///
    /// The slice runs to the end of the contiguous executable run containing
    /// `address`, capped, so a decoder can work through it without asking
    /// again per instruction.
    ///
    /// # Safety
    /// The returned slice borrows linear memory, which the guest can write.
    /// It is used only for decoding, and only before anything can run.
    pub fn fetch(&self, address: u64, cap: u64) -> Result<&[u8], Fault> {
        self.permitted(address, 1, Access::Fetch)?;
        let mut end = ((address >> PAGE_SHIFT) + 1) << PAGE_SHIFT;
        let ceiling = address.saturating_add(cap).min(self.limit);
        while end < ceiling && self.executable.get((end >> PAGE_SHIFT) as usize) {
            end += PAGE_SIZE;
        }
        let end = end.min(ceiling);
        // SAFETY: every page from `address` to `end` is mapped executable,
        // which is stronger than the readability the slice needs.
        unsafe {
            Ok(std::slice::from_raw_parts(
                Self::pointer(address),
                (end - address) as usize,
            ))
        }
    }

    /// Queues the pages a write landed on, if any of them hold code.
    #[inline]
    fn note_code_write(&mut self, address: u64, length: u64) {
        if length == 0 {
            return;
        }
        let first = (address >> PAGE_SHIFT) as usize;
        let last = ((address + length - 1) >> PAGE_SHIFT) as usize;
        for page in first..=last {
            if self.code.get(page) {
                // Cleared here rather than at the drain: the page is queued,
                // so a second store to it before the cache catches up has
                // nothing new to say.
                self.code.set(page, false);
                self.dirty.push(page as u64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::Arena;

    fn space(arena: &Arena) -> Space {
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), arena.length(), Protection::ALL);
        space
    }

    #[test]
    fn an_unmapped_page_faults_rather_than_reading_garbage() {
        let arena = Arena::new(0x2_0000);
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), PAGE_SIZE, Protection::READ);
        assert_eq!(space.load(arena.base(), Width::Dword), Ok(0));
        let beyond = arena.base() + PAGE_SIZE;
        assert_eq!(
            space.load(beyond, Width::Dword),
            Err(Fault {
                address: beyond,
                access: Access::Read
            })
        );
    }

    #[test]
    fn a_read_only_page_refuses_a_store() {
        let arena = Arena::new(0x2_0000);
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), PAGE_SIZE, Protection::READ);
        assert_eq!(
            space.store(arena.base(), Width::Byte, 1),
            Err(Fault {
                address: arena.base(),
                access: Access::Write
            })
        );
    }

    #[test]
    fn a_straddling_access_faults_on_the_page_that_refused_it() {
        let arena = Arena::new(0x2_0000);
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), PAGE_SIZE, Protection::READ_WRITE);
        let at = arena.base() + PAGE_SIZE - 4;
        assert_eq!(
            space.load(at, Width::Qword),
            Err(Fault {
                address: arena.base() + PAGE_SIZE,
                access: Access::Read
            })
        );
    }

    #[test]
    fn values_round_trip_at_every_width_including_unaligned() {
        let arena = Arena::new(0x2_0000);
        let mut space = space(&arena);
        for offset in 0..9u64 {
            let at = arena.base() + 0x100 + offset;
            space.store(at, Width::Qword, 0x0123_4567_89ab_cdef).unwrap();
            assert_eq!(space.load(at, Width::Qword), Ok(0x0123_4567_89ab_cdef));
            assert_eq!(space.load(at, Width::Dword), Ok(0x89ab_cdef));
            assert_eq!(space.load(at, Width::Word), Ok(0xcdef));
            assert_eq!(space.load(at, Width::Byte), Ok(0xef));
        }
    }

    /// The two kinds of write, told apart. A guest-behalf write to a
    /// read-only page is refused, because that is what the guest asked for;
    /// the kernel populating the same page is not, because a program's text
    /// is read-execute and still has to be loaded into.
    #[test]
    fn the_kernel_may_populate_a_page_the_guest_may_not_write() {
        let arena = Arena::new(0x2_0000);
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), PAGE_SIZE, Protection::READ_EXECUTE);
        assert_eq!(
            space.write(arena.base(), b"guest"),
            Err(Fault {
                address: arena.base(),
                access: Access::Write
            })
        );
        assert_eq!(space.place(arena.base(), b"kernel"), Ok(()));
        // SAFETY: read before anything can write it.
        assert_eq!(unsafe { space.slice(arena.base(), 6) }, Ok(&b"kernel"[..]));
    }

    /// What the kernel's write does *not* skip.
    #[test]
    fn a_kernel_write_still_invalidates_a_code_page() {
        let arena = Arena::new(0x2_0000);
        let mut space = space(&arena);
        let page = arena.base() >> PAGE_SHIFT;
        space.mark_code(page);
        space.place(arena.base(), b"new bytes").unwrap();
        assert_eq!(space.take_dirty_code(), vec![page]);
    }

    #[test]
    fn a_kernel_write_outside_linear_memory_is_still_refused() {
        let arena = Arena::new(0x2_0000);
        let mut space = space(&arena);
        assert!(space.place(space.limit(), b"past the end").is_err());
        assert!(space.place(0, b"the null page").is_err());
    }

    #[test]
    fn a_store_to_a_code_page_is_queued_and_a_store_elsewhere_is_not() {
        let arena = Arena::new(0x2_0000);
        let mut space = space(&arena);
        let page = arena.base() >> PAGE_SHIFT;
        space.mark_code(page);
        space.store(arena.base() + PAGE_SIZE, Width::Byte, 1).unwrap();
        assert!(!space.has_dirty_code(), "a store off the code page");
        space.store(arena.base() + 8, Width::Byte, 1).unwrap();
        assert_eq!(space.take_dirty_code(), vec![page]);
        assert!(!space.has_dirty_code());
    }

    #[test]
    fn a_write_straddling_into_a_code_page_is_queued() {
        let arena = Arena::new(0x2_0000);
        let mut space = space(&arena);
        let page = (arena.base() >> PAGE_SHIFT) + 1;
        space.mark_code(page);
        space
            .store(arena.base() + PAGE_SIZE - 4, Width::Qword, 0)
            .unwrap();
        assert_eq!(space.take_dirty_code(), vec![page]);
    }

    #[test]
    fn unmapping_a_code_page_queues_it_too() {
        let arena = Arena::new(0x2_0000);
        let mut space = space(&arena);
        let page = arena.base() >> PAGE_SHIFT;
        space.mark_code(page);
        space.unmap(arena.base(), PAGE_SIZE);
        assert_eq!(space.take_dirty_code(), vec![page]);
        assert!(space.load(arena.base(), Width::Byte).is_err());
    }

    #[test]
    fn a_string_stops_at_its_terminator() {
        let arena = Arena::new(0x2_0000);
        let mut space = space(&arena);
        let at = arena.base() + 0x40;
        space.write(at, b"/lib/libc.so.6\0trailing").unwrap();
        // SAFETY: the slice is read before anything can write it.
        let answer = unsafe { space.c_string(at, 256) }.unwrap();
        assert_eq!(answer, Ok(&b"/lib/libc.so.6"[..]));
    }

    #[test]
    fn a_string_that_never_terminates_inside_the_bound_is_too_long() {
        let arena = Arena::new(0x2_0000);
        let mut space = space(&arena);
        let at = arena.base() + 0x40;
        space.write(at, &[b'a'; 64]).unwrap();
        // SAFETY: as above.
        let answer = unsafe { space.c_string(at, 16) }.unwrap();
        assert_eq!(answer, Err(Unterminated::TooLong));
    }

    /// The case a scan bounded only by the caller's limit gets wrong: a
    /// string that runs off the end of what the guest may read. Walking on
    /// would read whatever is mapped next and call it a path.
    #[test]
    fn a_string_running_off_a_mapping_is_a_fault_not_a_walk() {
        let arena = Arena::new(0x4_0000);
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), PAGE_SIZE, Protection::READ_WRITE);
        // The page after is mapped, but not readable by the guest.
        space.protect(arena.base() + PAGE_SIZE, PAGE_SIZE, Protection::NONE);
        let at = arena.base() + PAGE_SIZE - 8;
        space.write(at, &[b'a'; 8]).unwrap();
        // SAFETY: as above.
        let answer = unsafe { space.c_string(at, 256) }.unwrap();
        assert_eq!(answer, Err(Unterminated::Fault));
    }

    #[test]
    fn a_string_at_an_unreadable_address_faults() {
        let arena = Arena::new(0x2_0000);
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), PAGE_SIZE, Protection::NONE);
        // SAFETY: as above.
        assert!(unsafe { space.c_string(arena.base(), 256) }.is_err());
    }

    #[test]
    fn a_fetch_slice_stops_at_the_end_of_the_executable_run() {
        let arena = Arena::new(0x4_0000);
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), 2 * PAGE_SIZE, Protection::READ_EXECUTE);
        space.protect(arena.base() + 2 * PAGE_SIZE, PAGE_SIZE, Protection::READ);
        let bytes = space.fetch(arena.base() + 16, 1 << 20).unwrap();
        assert_eq!(bytes.len() as u64, 2 * PAGE_SIZE - 16);
        assert_eq!(
            space.fetch(arena.base() + 2 * PAGE_SIZE, 16),
            Err(Fault {
                address: arena.base() + 2 * PAGE_SIZE,
                access: Access::Fetch
            })
        );
    }
}
