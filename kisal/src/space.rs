//! The guest's address space: the arenas, the VMA tree, and the interval
//! surgery every memory call performs on it.
//!
//! A simplifying fact first. The main binary is never "loaded": zaqaru
//! resolves every operand to symbol-plus-addend, so the program's data
//! sections are placed by `wasm-ld` and there are no ELF virtual addresses to
//! honour. Loading exists only for what a dynamic loader maps at run time,
//! and those addresses come from here like any other mapping. So this
//! allocator is not reproducing a virtual memory system — it is handing out
//! ranges of one flat linear memory and remembering what it said about each.
//!
//! Three facts about wasm shape everything below:
//!
//! - **Memory only grows, and only in 64 KiB pages.** Both are invisible to
//!   the guest, which is promised 4 KiB pages. The page fiction is kept in
//!   bookkeeping alone: lengths round to 4 KiB, `munmap` and `mprotect`
//!   reject unaligned arguments, and the real growth happens in amortised
//!   chunks when a reservation crosses the current size.
//! - **Memory never shrinks**, so `munmap` cannot return anything to the
//!   host. It returns ranges to a free pool for address reuse instead —
//!   with one obligation, below.
//! - **There are no faults.** Nothing can be lazily zeroed, because nothing
//!   traps on first touch. Anonymous memory is zeroed when it is handed out.
//!
//! The obligation: a reused range handed out as fresh anonymous memory must
//! read as zeros. Freshly grown memory already does — wasm guarantees it —
//! so a high-water mark divides the space in two, and only a range below it
//! has to be filled. That is the whole of the zeroing story, and it is why
//! `MADV_DONTNEED` is not a no-op here.

use std::vec::Vec;

use crate::errno::Errno;
use crate::mount::Vnode;

/// The page size the guest is promised. Wasm's own is sixteen times larger
/// and the guest never learns that.
pub const PAGE: u64 = 4096;

/// How much address space `brk` gets before it starts failing.
///
/// glibc allocates through `brk` until it fails and then falls back to
/// `mmap` — that fallback is a normal, tested path in every libc, and it is
/// what a ceiling here exercises. A `brk` arena that grew without limit
/// would instead be one contiguous region that can never be reused, since
/// `brk` only moves back down when a program frees from the top.
pub const BRK_CEILING: u64 = 32 * 1024 * 1024;

/// How much memory is asked for at once when a reservation crosses the
/// current size. Growth is a host `mmap` under wasmtime; doing it per page
/// would be a syscall per page.
pub const GROW_CHUNK: u64 = 1024 * 1024;

pub mod prot {
    pub const NONE: i32 = 0;
    pub const READ: i32 = 1;
    pub const WRITE: i32 = 2;
    pub const EXEC: i32 = 4;
    /// Reserved for `System V` semaphores, and a no-op on every
    /// architecture that matters. `mprotect` accepts it; nothing does
    /// anything with it.
    pub const SEM: i32 = 8;
    pub const ALL: i32 = READ | WRITE | EXEC;
}

pub mod map {
    pub const SHARED: i32 = 0x01;
    pub const PRIVATE: i32 = 0x02;
    pub const SHARED_VALIDATE: i32 = 0x03;
    pub const TYPE_MASK: i32 = 0x0f;
    pub const FIXED: i32 = 0x10;
    pub const ANONYMOUS: i32 = 0x20;
    pub const GROWSDOWN: i32 = 0x0100;
    pub const DENYWRITE: i32 = 0x0800;
    pub const EXECUTABLE: i32 = 0x1000;
    pub const LOCKED: i32 = 0x2000;
    pub const NORESERVE: i32 = 0x4000;
    pub const POPULATE: i32 = 0x8000;
    pub const NONBLOCK: i32 = 0x10000;
    pub const STACK: i32 = 0x20000;
    pub const HUGETLB: i32 = 0x40000;
    pub const SYNC: i32 = 0x80000;
    pub const FIXED_NOREPLACE: i32 = 0x100000;
}

pub mod advice {
    pub const NORMAL: i32 = 0;
    pub const RANDOM: i32 = 1;
    pub const SEQUENTIAL: i32 = 2;
    pub const WILLNEED: i32 = 3;
    pub const DONTNEED: i32 = 4;
    pub const FREE: i32 = 8;
    pub const REMOVE: i32 = 9;
    pub const DONTFORK: i32 = 10;
    pub const DOFORK: i32 = 11;
    pub const HUGEPAGE: i32 = 14;
    pub const NOHUGEPAGE: i32 = 15;
    pub const DONTDUMP: i32 = 16;
    pub const DODUMP: i32 = 17;
    pub const WIPEONFORK: i32 = 18;
    pub const KEEPONFORK: i32 = 19;
}

pub mod remap {
    pub const MAYMOVE: i32 = 1;
    pub const FIXED: i32 = 2;
    pub const DONTUNMAP: i32 = 4;
}

/// What a mapping is backed by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backing {
    /// Zero-filled, and belonging to nothing.
    Anonymous,
    /// `PROT_NONE` address space with no obligations — glibc reserves a
    /// thread stack's whole extent this way and then maps the usable part
    /// over it.
    Reservation,
    /// A copy of a file's bytes.
    ///
    /// A *copy*, eagerly, for every kind of file. POSIX leaves post-map
    /// visibility of writes unspecified for `MAP_PRIVATE`, so a copy is
    /// conformant rather than a shortcut — and the alternative, handing the
    /// guest an address inside the shared immutable image blob, cannot
    /// survive a later `mprotect(PROT_WRITE)`, which Linux permits on a
    /// private mapping however the descriptor was opened.
    File { vnode: Vnode, offset: u64 },
}

/// One mapping.
#[derive(Clone, Debug)]
pub struct Vma {
    pub start: u64,
    pub length: u64,
    /// As recorded. Nothing enforces it: wasm has no protection to set, and
    /// a kernel that pretended otherwise would be lying about the one thing
    /// a guest cannot check.
    pub prot: i32,
    pub flags: i32,
    pub backing: Backing,
}

impl Vma {
    pub fn end(&self) -> u64 {
        self.start + self.length
    }

    fn contains(&self, address: u64) -> bool {
        address >= self.start && address < self.end()
    }

    /// The same mapping, describing a sub-range of itself. A file backing's
    /// offset moves with the start, which is what makes splitting an extent
    /// mapping keep every piece pointing at the right part of the file.
    fn slice(&self, start: u64, length: u64) -> Self {
        let backing = match &self.backing {
            Backing::File { vnode, offset } => Backing::File {
                vnode: *vnode,
                offset: offset + (start - self.start),
            },
            other => other.clone(),
        };
        Self {
            start,
            length,
            prot: self.prot,
            flags: self.flags,
            backing,
        }
    }
}

/// What the caller must do to the range before the guest sees it.
///
/// Returned rather than done here so that this module stays arithmetic: it
/// decides *which* bytes are owed zeros, and the kernel — which is the thing
/// holding a handle on guest memory — writes them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Fill {
    pub start: u64,
    pub length: u64,
}

pub struct Space {
    /// The `brk` arena: contiguous, bump-allocated, with a ceiling.
    brk_start: u64,
    brk_current: u64,
    brk_ceiling: u64,
    /// The highest address ever handed out. Above it, memory is freshly
    /// grown and therefore zero; below it, anything reused has to be
    /// filled.
    high_water: u64,
    /// Ordered by start. A `Vec` rather than a tree: the trace's process
    /// holds a few hundred mappings, and a binary search over a contiguous
    /// array beats a tree with pointer chasing at that size.
    vmas: Vec<Vma>,
    /// Ranges `munmap` gave back, for address reuse. Memory never shrinks,
    /// so this is the only way an address is ever used twice.
    free: Vec<(u64, u64)>,
    /// One past the highest address the guest may be given.
    ///
    /// The guest's arenas are a *reserved block*, not whatever lies above
    /// the module. They have to be, because the kernel's own allocator takes
    /// its pages from the end of linear memory and so would the arenas —
    /// two claimants on the same bytes, interleaving, with the guest's `brk`
    /// memory and the kernel's heap silently the same. Reserving the block
    /// once at boot puts the kernel's heap above it for good.
    ///
    /// So the guest's address space is bounded, and running out of it is an
    /// ordinary `ENOMEM` rather than a collision nothing detects.
    ceiling: u64,
}

impl Space {
    /// Carves the arenas out of whatever is above the module's current end.
    ///
    /// `start` is the top of memory at boot, which is above the linker's
    /// data, the shadow stack, and whatever the kernel's own allocator has
    /// taken. Everything the guest is given comes from here up.
    /// Unbounded, for the native tests: off wasm there is no shared memory
    /// to collide over, and a test that is not about exhaustion should not
    /// have to say how much space it wants.
    pub fn new(start: u64) -> Self {
        Self::with_brk_ceiling(start, BRK_CEILING)
    }

    /// Bounded to a reserved block, which is what a container gets.
    pub fn within(start: u64, ceiling: u64) -> Self {
        Self {
            ceiling,
            ..Self::with_brk_ceiling(start, BRK_CEILING)
        }
    }

    /// The same, with the `brk` arena sized explicitly.
    ///
    /// The ceiling is a configured quantity rather than a constant of
    /// nature: what it decides is when glibc gives up on `brk` and switches
    /// to `mmap`, and a test that wants to watch that happen should not have
    /// to allocate thirty-two megabytes to see it.
    pub fn with_brk_ceiling(start: u64, arena: u64) -> Self {
        // Saturating, not wrapping. A start at the very top of the address
        // space is what a test that never uses memory hands over, and the
        // arenas it produces are empty — which is the right answer, and not
        // one that should arrive as a panic in the middle of an unrelated
        // syscall.
        let brk_start = start.checked_next_multiple_of(PAGE).unwrap_or(u64::MAX);
        Self {
            brk_start,
            brk_current: brk_start,
            brk_ceiling: brk_start.saturating_add(arena),
            high_water: brk_start,
            vmas: Vec::new(),
            free: Vec::new(),
            ceiling: u64::MAX,
        }
    }

    pub fn vmas(&self) -> &[Vma] {
        &self.vmas
    }

    pub fn brk_current(&self) -> u64 {
        self.brk_current
    }

    /// `brk`: a bump pointer inside its arena.
    ///
    /// A request of zero asks where the break is, which is how every libc
    /// starts. Past the ceiling it stays where it was — Linux answers the
    /// *current* break rather than an errno, and glibc reads the unchanged
    /// value as failure and falls back to `mmap`.
    pub fn brk(&mut self, requested: u64, grow: &mut impl FnMut(u64) -> bool) -> (u64, Fill) {
        if requested == 0 || requested < self.brk_start || requested > self.brk_ceiling {
            return (self.brk_current, Fill::default());
        }
        let requested = requested.next_multiple_of(PAGE);
        if requested <= self.brk_current {
            // Shrinking. The memory stays, and the range above the new
            // break is no longer the guest's — a later grow of the break
            // has to hand it back as zeros, which the high-water mark
            // already arranges.
            self.brk_current = requested;
            return (self.brk_current, Fill::default());
        }
        if !grow(requested) {
            return (self.brk_current, Fill::default());
        }
        let fill = self.claim(self.brk_current, requested - self.brk_current);
        self.brk_current = requested;
        (self.brk_current, fill)
    }

    /// Records a range as handed out, and says how much of it must be
    /// zeroed: everything below the high-water mark, which is the part that
    /// may hold what a previous owner left.
    fn claim(&mut self, start: u64, length: u64) -> Fill {
        let end = start.saturating_add(length);
        let fill = if start < self.high_water {
            Fill {
                start,
                length: length.min(self.high_water - start),
            }
        } else {
            Fill::default()
        };
        self.high_water = self.high_water.max(end);
        fill
    }
}

// ---- the VMA tree -----------------------------------------------------

/// What a caller is asking `mmap` for.
pub struct Request {
    pub hint: u64,
    pub length: u64,
    pub prot: i32,
    pub flags: i32,
    pub backing: Backing,
}

impl Space {
    /// `mmap`.
    ///
    /// Returns the address and the range that must be zeroed before the
    /// guest sees it. A file-backed request is zeroed the same way and then
    /// has its bytes copied over the top by the caller: the tail of the last
    /// page past the file's end has to read as zeros, which is what Linux
    /// guarantees and what a copy alone would not give.
    pub fn map(
        &mut self,
        request: &Request,
        grow: &mut impl FnMut(u64) -> bool,
    ) -> Result<(u64, Fill), Errno> {
        if request.length == 0 {
            return Err(Errno::Invalid);
        }
        let length = round_up(request.length)?;
        let fixed = request.flags & map::FIXED != 0;
        let no_replace = request.flags & map::FIXED_NOREPLACE != 0;

        if (fixed || no_replace) && request.hint % PAGE != 0 {
            return Err(Errno::Invalid);
        }

        let start = if fixed || no_replace {
            let start = request.hint;
            if start == 0 {
                return Err(Errno::Invalid);
            }
            // `MAP_FIXED_NOREPLACE` is the whole reason a caller can ask for
            // an address *safely*: it fails rather than destroying what is
            // there. Plain `MAP_FIXED` destroys it, which is how a loader
            // carves segments out of an extent it just mapped.
            if no_replace && self.overlaps(start, length) {
                return Err(Errno::Exists);
            }
            let end = start.checked_add(length).ok_or(Errno::NoMemory)?;
            // Past the reserved block is not the guest's to have: above it
            // is where the kernel's own allocator lives.
            if end > self.ceiling || !grow(end) {
                return Err(Errno::NoMemory);
            }
            self.punch(start, length);
            start
        } else {
            // A hint is honoured when it is free, and silently ignored when
            // it is not — which is exactly what Linux does, and why a caller
            // that needs the address must say `MAP_FIXED_NOREPLACE`.
            let hinted = if request.hint != 0 && request.hint % PAGE == 0 {
                let hint = request.hint;
                (!self.overlaps(hint, length) && hint >= self.brk_ceiling).then_some(hint)
            } else {
                None
            };
            match hinted {
                Some(hint) => {
                    let end = hint.checked_add(length).ok_or(Errno::NoMemory)?;
                    if !grow(end) {
                        return Err(Errno::NoMemory);
                    }
                    hint
                }
                None => self.allocate(length, grow)?,
            }
        };

        let fill = self.claim(start, length);
        self.insert(Vma {
            start,
            length,
            prot: request.prot,
            flags: request.flags,
            backing: request.backing.clone(),
        });
        Ok((start, fill))
    }

    /// Finds somewhere to put `length` bytes: a returned range if one fits,
    /// otherwise fresh address space above everything.
    ///
    /// Reuse first, because memory never shrinks — a process that maps and
    /// unmaps in a loop would otherwise walk the address space until it ran
    /// out, which glibc's arena handling does exactly.
    fn allocate(&mut self, length: u64, grow: &mut impl FnMut(u64) -> bool) -> Result<u64, Errno> {
        if let Some(index) = self.free.iter().position(|(_, size)| *size >= length) {
            let (start, size) = self.free[index];
            if size == length {
                self.free.remove(index);
            } else {
                self.free[index] = (start + length, size - length);
            }
            return Ok(start);
        }
        let start = self.top();
        let end = start.checked_add(length).ok_or(Errno::NoMemory)?;
        if end > self.ceiling || !grow(end) {
            return Err(Errno::NoMemory);
        }
        Ok(start)
    }

    /// One past the highest address any arena or mapping has reached.
    fn top(&self) -> u64 {
        let mapped = self.vmas.last().map(Vma::end).unwrap_or(0);
        let freed = self
            .free
            .iter()
            .map(|(start, length)| start.saturating_add(*length))
            .max()
            .unwrap_or(0);
        self.brk_ceiling.max(mapped).max(freed).max(self.high_water)
    }

    fn overlaps(&self, start: u64, length: u64) -> bool {
        let end = start.saturating_add(length);
        self.vmas
            .iter()
            .any(|vma| vma.start < end && start < vma.end())
    }

    fn insert(&mut self, vma: Vma) {
        let at = self
            .vmas
            .partition_point(|existing| existing.start < vma.start);
        self.vmas.insert(at, vma);
    }

    /// Removes a range from the tree, splitting whatever it partly covers.
    ///
    /// This is the whole of interval surgery, and every operation is a use
    /// of it: `munmap` punches and frees, `MAP_FIXED` punches and installs,
    /// `mprotect` punches and reinstalls with a different `prot`.
    fn punch(&mut self, start: u64, length: u64) {
        let end = start.saturating_add(length);
        let mut index = 0;
        while index < self.vmas.len() {
            let vma = self.vmas[index].clone();
            if vma.end() <= start || vma.start >= end {
                index += 1;
                continue;
            }
            self.vmas.remove(index);
            // The part below the hole survives, and so does the part above.
            // Both, for a hole strictly inside a mapping — which is a
            // partial `munmap`, and the case a naive implementation drops.
            let mut reinserted = 0;
            if vma.start < start {
                self.vmas
                    .insert(index + reinserted, vma.slice(vma.start, start - vma.start));
                reinserted += 1;
            }
            if vma.end() > end {
                self.vmas
                    .insert(index + reinserted, vma.slice(end, vma.end() - end));
                reinserted += 1;
            }
            index += reinserted;
        }
    }

    /// `munmap`: punch the hole and return the range for reuse.
    pub fn unmap(&mut self, start: u64, length: u64) -> Result<(), Errno> {
        if start % PAGE != 0 || length == 0 {
            return Err(Errno::Invalid);
        }
        let length = round_up(length)?;
        // Unmapping what was never mapped is not an error on Linux: it is
        // how a program cleans up a range it is unsure about.
        self.punch(start, length);
        self.release(start, length);
        Ok(())
    }

    /// Gives a range back to the free pool, joined to any neighbour.
    fn release(&mut self, start: u64, length: u64) {
        let mut start = start;
        let mut end = start.saturating_add(length);
        let mut index = 0;
        while index < self.free.len() {
            let (other_start, other_length) = self.free[index];
            let other_end = other_start + other_length;
            if other_end < start || other_start > end {
                index += 1;
                continue;
            }
            // Touching or overlapping: absorb it. Coalescing matters
            // because the alternative is a pool of thousands of page-sized
            // holes that no larger request can ever use.
            start = start.min(other_start);
            end = end.max(other_end);
            self.free.remove(index);
        }
        self.free.push((start, end - start));
    }

    /// `mprotect`: split and record, enforce nothing.
    ///
    /// Nothing can be enforced — wasm has no page protection and no faults —
    /// and the threat model says so out loud rather than pretending. What
    /// the record is *for* is `/proc/self/maps`, which glibc reads.
    pub fn protect(&mut self, start: u64, length: u64, prot: i32) -> Result<(), Errno> {
        if start % PAGE != 0 || length == 0 {
            return Err(Errno::Invalid);
        }
        // `mprotect` validates its protection bits where `mmap` ignores
        // them — an asymmetry, and a real one: measured, `mmap` accepts
        // even `0x80000000` and `mprotect` refuses `0x40`. `PROT_SEM` is
        // accepted and does nothing, on Linux as here.
        if prot & !(prot::ALL | prot::SEM) != 0 {
            return Err(Errno::Invalid);
        }
        let prot = prot & prot::ALL;
        let length = round_up(length)?;
        let end = start.checked_add(length).ok_or(Errno::NoMemory)?;

        // Every page in the range has to be mapped, and Linux checks that
        // before it changes anything: a `mprotect` spanning a hole is
        // `ENOMEM` with nothing modified.
        let mut covered = start;
        for vma in &self.vmas {
            if vma.end() <= covered {
                continue;
            }
            if vma.start > covered {
                break;
            }
            covered = vma.end();
            if covered >= end {
                break;
            }
        }
        if covered < end {
            return Err(Errno::NoMemory);
        }

        let pieces: Vec<Vma> = self
            .vmas
            .iter()
            .filter(|vma| vma.start < end && start < vma.end())
            .map(|vma| {
                let from = vma.start.max(start);
                let to = vma.end().min(end);
                let mut piece = vma.slice(from, to - from);
                piece.prot = prot;
                piece
            })
            .collect();
        self.punch(start, length);
        for piece in pieces {
            self.insert(piece);
        }
        Ok(())
    }

    /// `madvise`.
    ///
    /// `MADV_DONTNEED` is not a no-op, which is a correction that came from
    /// reading a real trace rather than assuming: glibc's arena-free path
    /// uses it, and on Linux a subsequent read of anonymous memory sees
    /// zeros. A kernel that recorded it and moved on would hand a program
    /// its own freed heap back. `MADV_FREE` is the lazy cousin, and zeroing
    /// eagerly is a conformant implementation of it.
    pub fn advise(&mut self, start: u64, length: u64, advice: i32) -> Result<Fill, Errno> {
        if start % PAGE != 0 {
            return Err(Errno::Invalid);
        }
        if length == 0 {
            return Ok(Fill::default());
        }
        let length = round_up(length)?;
        match advice {
            advice::DONTNEED | advice::FREE => {
                // Only anonymous memory. Advising away a file mapping
                // restores the file's contents on Linux, and this has no
                // faulting layer to restore them with — so a range that is
                // not anonymous is left alone rather than silently zeroed,
                // which would destroy data the guest can still read.
                let anonymous = self.vmas.iter().any(|vma| {
                    vma.start < start + length
                        && start < vma.end()
                        && matches!(vma.backing, Backing::Anonymous)
                });
                if !anonymous {
                    return Ok(Fill::default());
                }
                Ok(Fill { start, length })
            }
            advice::NORMAL
            | advice::RANDOM
            | advice::SEQUENTIAL
            | advice::WILLNEED
            | advice::DONTFORK
            | advice::DOFORK
            | advice::HUGEPAGE
            | advice::NOHUGEPAGE
            | advice::DONTDUMP
            | advice::DODUMP
            | advice::WIPEONFORK
            | advice::KEEPONFORK => Ok(Fill::default()),
            _ => Err(Errno::Invalid),
        }
    }

    /// The mapping containing an address, if any.
    pub fn find(&self, address: u64) -> Option<&Vma> {
        let at = self.vmas.partition_point(|vma| vma.end() <= address);
        self.vmas.get(at).filter(|vma| vma.contains(address))
    }
}

/// Rounds a length up to the page the guest believes in, refusing one that
/// would overflow rather than wrapping it to something small.
fn round_up(length: u64) -> Result<u64, Errno> {
    length.checked_next_multiple_of(PAGE).ok_or(Errno::NoMemory)
}

// ---- mremap and the rendering -----------------------------------------

/// What `mremap` decided, so the caller can move the bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Move {
    pub from: u64,
    pub to: u64,
    /// How many bytes to carry across. The old length, or the new one if
    /// the mapping shrank.
    pub length: u64,
    /// Zeros owed on the tail of the new mapping, past what was copied.
    pub fill: Fill,
}

impl Space {
    /// `mremap`: grow, shrink, or move a mapping.
    ///
    /// Absent from the trace, but glibc's `realloc` reaches for it, and in a
    /// flat address space it is nearly free: extend in place when the space
    /// above is unclaimed, and otherwise — with `MREMAP_MAYMOVE` — take a
    /// new range, copy, and release the old one.
    pub fn remap(
        &mut self,
        start: u64,
        old_length: u64,
        new_length: u64,
        flags: i32,
        grow: &mut impl FnMut(u64) -> bool,
    ) -> Result<Move, Errno> {
        if start % PAGE != 0 || new_length == 0 {
            return Err(Errno::Invalid);
        }
        if flags & !(remap::MAYMOVE | remap::FIXED | remap::DONTUNMAP) != 0 {
            return Err(Errno::Invalid);
        }
        if flags & (remap::FIXED | remap::DONTUNMAP) != 0 {
            // Both promise something specific about *where* the result
            // lands, and neither is what a `realloc` asks for. Refused
            // rather than approximated.
            return Err(Errno::Invalid);
        }
        let old_length = round_up(old_length)?;
        let new_length = round_up(new_length)?;

        let index = self
            .vmas
            .iter()
            .position(|vma| vma.start == start)
            .ok_or(Errno::Fault)?;
        if self.vmas[index].length != old_length {
            // Linux allows remapping part of a mapping; this does not, and
            // says so rather than guessing which part was meant.
            return Err(Errno::Invalid);
        }

        if new_length <= old_length {
            // Shrinking is a truncation, and the tail goes back to the pool.
            self.vmas[index].length = new_length;
            self.release(start + new_length, old_length - new_length);
            return Ok(Move {
                from: start,
                to: start,
                length: new_length,
                fill: Fill::default(),
            });
        }

        let extra = new_length - old_length;
        if !self.overlaps(start + old_length, extra)
            && self.reserve_above(start + old_length, extra, grow)
        {
            let fill = self.claim(start + old_length, extra);
            self.vmas[index].length = new_length;
            return Ok(Move {
                from: start,
                to: start,
                length: old_length,
                fill,
            });
        }

        if flags & remap::MAYMOVE == 0 {
            return Err(Errno::NoMemory);
        }
        let vma = self.vmas[index].clone();
        let destination = self.allocate(new_length, grow)?;
        let fill = self.claim(destination, new_length);
        self.punch(start, old_length);
        self.release(start, old_length);
        let mut moved = vma.slice(vma.start, new_length);
        moved.start = destination;
        self.insert(moved);
        Ok(Move {
            from: start,
            to: destination,
            length: old_length,
            // The copy covers the old length; everything past it is owed
            // zeros, and the caller has been told which part of the whole
            // new range needs them.
            fill: Fill {
                start: fill.start.max(destination + old_length),
                length: fill.length.saturating_sub(
                    destination + old_length - fill.start.min(destination + old_length),
                ),
            },
        })
    }

    /// Whether a range immediately above a mapping can be taken: it must be
    /// free of other mappings, not inside the `brk` arena, and reachable.
    fn reserve_above(
        &mut self,
        start: u64,
        length: u64,
        grow: &mut impl FnMut(u64) -> bool,
    ) -> bool {
        if start < self.brk_ceiling {
            return false;
        }
        // If the pool holds it, take it out; otherwise it has to be fresh
        // space above everything.
        if let Some(index) = self.free.iter().position(|(free_start, free_length)| {
            *free_start <= start && free_start + free_length >= start + length
        }) {
            let (free_start, free_length) = self.free[index];
            self.free.remove(index);
            if free_start < start {
                self.free.push((free_start, start - free_start));
            }
            let end = free_start + free_length;
            if end > start + length {
                self.free.push((start + length, end - (start + length)));
            }
            return true;
        }
        if start + length <= self.high_water {
            // Inside space that has been handed out before and is not in
            // the pool: something else owns it.
            return false;
        }
        grow(start + length)
    }
}
