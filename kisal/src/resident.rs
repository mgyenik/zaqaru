//! Which process's bytes are where: the page ownership that makes a process
//! switch cost what actually collides, rather than everything.
//!
//! Every process in a container shares one address range — see
//! `machine::guest_base` — and the interpreter has no page table to swap, so
//! at most one process's bytes can be at any given address. The first
//! arrangement made that "at most one process's bytes are at the guest's
//! addresses *at all*": a switch copied the whole outgoing process out to
//! the heap, zeroed it, and copied the whole incoming one back. Correct, and
//! measured at thirty of a warm request's forty-one milliseconds, because a
//! 60 MB CPython worker moved twice on every round trip to a 4 MB nginx that
//! overlapped its first four megabytes and nothing above them.
//!
//! So the rule becomes per page. **Linear memory keeps every process's
//! pages in place**, and an owner table says whose bytes are resident at
//! each page. A page moves only when the running process maps an address
//! whose resident bytes belong to somebody else — the owner's bytes are
//! *displaced* into the heap, the page is zeroed for the newcomer — and it
//! moves back when the displaced process next runs. Two processes that
//! never map the same address never copy anything on each other's account.
//!
//! Three invariants, and all of this module is keeping them:
//!
//! 1. **Every page the running process maps is owned by it.** Established
//!    by [`activate`], which brings back every page it was displaced from,
//!    and kept by [`claim`], which the kernel calls whenever this process
//!    comes to map a page — so a page a process can address always holds
//!    its own bytes.
//! 2. **A live owner maps the page.** [`release`] gives ownership up when
//!    a mapping goes, and [`retire`] ends a token when a process exits or
//!    `exec`s, so an owner that is live is one whose bytes are worth
//!    saving; anything else resident is garbage to be zeroed.
//! 3. **A displaced process holds a copy of every page it maps and does
//!    not own.** A fork child starts wholly displaced — it maps everything
//!    its parent does, at the same addresses, and the parent owns them —
//!    which is the one shape that stays a copy of the whole address space,
//!    and it is paid once per fork rather than per switch.
//!
//! One instance per thread, reached without being passed around, for the
//! same reason `guest_base` is a static: a process is born in three places
//! — boot, `fork`, `execve` — and every one of them would otherwise have to
//! thread the container's table through to a machine that has no other
//! reason to know it exists. A container is one thread; the native test
//! harness runs each test on a thread of its own.
//!
//! Natively there is one further duty. The guest's addresses are a
//! reservation shared by every container in the host process, and a
//! container whose pages stay resident holds them for its whole life — so
//! the first token taken on a thread takes the arena lock, and the lock is
//! released when the thread ends. Two containers in one thread would be
//! one container's bytes under another's, and there is none.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use targum::space::{PAGE_SHIFT, PAGE_SIZE};

const PAGE: usize = PAGE_SIZE as usize;

/// How the guest block is divided between programs that are not related by
/// `fork`, so that they do not collide by default.
///
/// Every process bump-allocates from the start of its address space, so two
/// unrelated programs given the same start map their binaries, their
/// libraries and their heaps at the same addresses and every switch between
/// them moves all of it. Given different starts they share nothing, and a
/// switch moves nothing. A fresh program is placed at the start of the slot
/// with the fewest pages owned by live processes — an empty one when there
/// is one, and the least crowded otherwise, which is still correct because
/// ownership is per page: a program that outgrows its slot collides with
/// its neighbour on the pages it reaches and on no others.
///
/// A quarter of the block. nginx, a shell and a CPython worker are three
/// programs; the fourth slot is what a `python -c` under the shell gets.
pub const SLOTS: u64 = 4;

/// A process's identity in the owner table. Never reused: a token that has
/// been retired stays dead, so bytes it owned are recognised as garbage
/// for as long as they sit there.
pub type Token = u32;

/// Nobody's, or nobody's any more.
const NONE: Token = 0;

/// A page number hashed by multiplication, which is all a dense integer key
/// needs. The default hasher is SipHash, chosen to resist an adversary
/// picking keys; the keys here are the guest's own page numbers, and a
/// switch hashes thousands of them.
#[derive(Default)]
struct PageHasher(u64);

impl Hasher for PageHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.write_u64(u64::from(*byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0 ^ value).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

type Copies = HashMap<u64, Box<[u8; PAGE]>, BuildHasherDefault<PageHasher>>;

struct Residency {
    /// The resident owner of each page, indexed by page number. Grown on
    /// demand; the guest's whole four gigabytes is a million entries.
    owners: Vec<Token>,
    /// Whether each token still names a process, indexed by token.
    live: Vec<bool>,
    /// Each displaced process's copies of the pages it maps and does not
    /// own, indexed by token.
    displaced: Vec<Copies>,
    /// Page buffers a restore has emptied, for the next displacement to
    /// fill: a switch that moves a thousand pages should not visit the
    /// allocator a thousand times each way.
    pool: Vec<Box<[u8; PAGE]>>,
    /// The arena, held for the thread's life — see the module comment.
    #[cfg(not(target_arch = "wasm32"))]
    _arena: Option<std::sync::MutexGuard<'static, ()>>,
}

thread_local! {
    static RESIDENCY: RefCell<Residency> = RefCell::new(Residency {
        owners: Vec::new(),
        live: vec![false],
        displaced: vec![Copies::default()],
        pool: Vec::new(),
        #[cfg(not(target_arch = "wasm32"))]
        _arena: None,
    });
}

/// A fresh identity for a process that is about to have pages.
pub(crate) fn new_token() -> Token {
    RESIDENCY.with_borrow_mut(|held| {
        #[cfg(not(target_arch = "wasm32"))]
        if held._arena.is_none() {
            held._arena = Some(targum::arena::hold_addresses());
        }
        held.live.push(true);
        held.displaced.push(Copies::default());
        (held.live.len() - 1) as Token
    })
}

/// Where a fresh program should start, inside `[base, ceiling)`.
///
/// The start of the slot with the fewest pages owned by live processes;
/// see [`SLOTS`]. The block itself is never moved: the ceiling every
/// process is bounded by stays where boot put it, because the kernel's own
/// heap lives above it.
pub(crate) fn choose_start(base: u64, ceiling: u64) -> u64 {
    let span = (ceiling - base) / SLOTS;
    if span < PAGE_SIZE {
        return base;
    }
    RESIDENCY.with_borrow(|held| {
        let mut best = (u64::MAX, base);
        for slot in 0..SLOTS {
            let start = base + slot * span;
            let crowd = pages(start, start + span)
                .filter(|page| held.is_live(held.owner(*page)))
                .count() as u64;
            if crowd < best.0 {
                best = (crowd, start);
            }
        }
        best.1
    })
}

/// The running process is about to map `[start, end)`.
///
/// Every page in it becomes the caller's: a page somebody else owns has
/// that owner's bytes saved first, and every page that changes hands is
/// zeroed, because the mapping rows promise fresh memory reads as zeros and
/// whatever was resident is another process's, or a dead one's, or a
/// previous container's in the same native test process.
pub(crate) fn claim(token: Token, start: u64, end: u64) {
    RESIDENCY.with_borrow_mut(|held| {
        for page in pages(start, end) {
            let owner = held.owner(page);
            if owner == token {
                continue;
            }
            if held.is_live(owner) {
                held.displace(owner, page);
            }
            // A copy this process held from before it was displaced here is
            // stale the moment it maps the page afresh.
            held.discard(token, page);
            // SAFETY: a page the caller is mapping, which the mapping rows
            // have already grown memory to cover.
            unsafe { resident(page).fill(0) };
            held.set_owner(page, token);
        }
    });
}

/// The running process no longer maps `[start, end)`.
///
/// Ownership goes so that a later claimant does not save bytes nobody can
/// read any more, and any copy this process was holding for the range is
/// dropped for the same reason.
pub(crate) fn release(token: Token, start: u64, end: u64) {
    RESIDENCY.with_borrow_mut(|held| {
        for page in pages(start, end) {
            if held.owner(page) == token {
                held.set_owner(page, NONE);
            }
            held.discard(token, page);
        }
    });
}

/// The process is about to run: every page it was displaced from comes
/// back, displacing whoever is there now.
pub(crate) fn activate(token: Token) {
    RESIDENCY.with_borrow_mut(|held| {
        let Some(slot) = held.displaced.get_mut(token as usize) else {
            return;
        };
        if slot.is_empty() {
            return;
        }
        let mine = std::mem::take(slot);
        for (page, bytes) in mine {
            let owner = held.owner(page);
            if owner != token && held.is_live(owner) {
                held.displace(owner, page);
            }
            // SAFETY: a page this process maps, inside memory that was
            // grown to hold it when the mapping was made.
            unsafe { resident(page).copy_from_slice(&bytes[..]) };
            held.set_owner(page, token);
            held.pool.push(bytes);
        }
    });
}

/// A child is born with a copy of every page in `ranges` — its parent's
/// mappings, which the parent owns because it is the one running — and
/// starts wholly displaced.
pub(crate) fn fork(parent: Token, child: Token, ranges: &[(u64, u64)]) {
    RESIDENCY.with_borrow_mut(|held| {
        let mut copies = Copies::default();
        for &(start, end) in ranges {
            for page in pages(start, end) {
                debug_assert_eq!(
                    held.owner(page),
                    parent,
                    "a forking process maps a page it does not own"
                );
                let mut copy = held.buffer();
                // SAFETY: a page the parent maps, and the parent is running.
                copy.copy_from_slice(unsafe { resident(page) });
                copies.insert(page, copy);
            }
        }
        held.displaced[child as usize] = copies;
    });
}

/// The process is gone, or has become another program: its copies are
/// dropped and its token is dead, so the bytes it owns are garbage.
pub(crate) fn retire(token: Token) {
    RESIDENCY.with_borrow_mut(|held| {
        if let Some(slot) = held.live.get_mut(token as usize) {
            *slot = false;
        }
        if let Some(copies) = held.displaced.get_mut(token as usize) {
            let dropped = std::mem::take(copies);
            held.pool.extend(dropped.into_values());
        }
    });
}

/// How many pages a process holds copies of, for the tests.
#[cfg(test)]
pub(crate) fn displaced_pages(token: Token) -> usize {
    RESIDENCY.with_borrow(|held| held.displaced.get(token as usize).map_or(0, HashMap::len))
}

impl Residency {
    fn owner(&self, page: u64) -> Token {
        self.owners.get(page as usize).copied().unwrap_or(NONE)
    }

    fn set_owner(&mut self, page: u64, token: Token) {
        let index = page as usize;
        if self.owners.len() <= index {
            self.owners.resize(index + 1, NONE);
        }
        self.owners[index] = token;
    }

    fn is_live(&self, token: Token) -> bool {
        token != NONE && self.live.get(token as usize).copied().unwrap_or(false)
    }

    /// Saves the resident bytes of `page` for `owner`, who is about to lose
    /// it.
    fn displace(&mut self, owner: Token, page: u64) {
        let mut copy = self.buffer();
        // SAFETY: a page a live process maps, inside grown memory.
        copy.copy_from_slice(unsafe { resident(page) });
        if let Some(old) = self.displaced[owner as usize].insert(page, copy) {
            self.pool.push(old);
        }
    }

    /// Drops the copy `token` holds of `page`, if any.
    fn discard(&mut self, token: Token, page: u64) {
        if let Some(copies) = self.displaced.get_mut(token as usize)
            && let Some(old) = copies.remove(&page)
        {
            self.pool.push(old);
        }
    }

    /// A page buffer, from the pool when it has one.
    fn buffer(&mut self) -> Box<[u8; PAGE]> {
        self.pool.pop().unwrap_or_else(|| Box::new([0u8; PAGE]))
    }
}

/// The page numbers covering `[start, end)`.
fn pages(start: u64, end: u64) -> impl Iterator<Item = u64> {
    let first = start >> PAGE_SHIFT;
    let last = end.saturating_sub(1) >> PAGE_SHIFT;
    match end > start {
        true => first..=last,
        #[allow(clippy::reversed_empty_ranges)]
        false => 1..=0,
    }
}

/// The bytes resident at a page.
///
/// # Safety
/// A guest address is a linear-memory offset, so a mapped page is `PAGE`
/// bytes at that offset — the identity the whole design rests on, and the
/// same one every load and store in `targum::space` uses. The caller must
/// know the page is inside grown memory.
unsafe fn resident<'a>(page: u64) -> &'a mut [u8] {
    let address = (page << PAGE_SHIFT) as usize;
    // SAFETY: the caller's.
    unsafe { core::slice::from_raw_parts_mut(address as *mut u8, PAGE) }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// A committed page of the shared guest memory, for the tests to own.
    fn committed(pages_wanted: u64) -> u64 {
        let base = targum::arena::FLOOR + (64 << 20);
        assert!(targum::arena::commit(base + pages_wanted * PAGE_SIZE));
        base
    }

    fn byte(at: u64) -> u8 {
        // SAFETY: a committed address.
        unsafe { *(at as usize as *const u8) }
    }

    fn set(at: u64, value: u8) {
        // SAFETY: a committed address.
        unsafe { *(at as usize as *mut u8) = value };
    }

    /// Two processes at the same address, and neither sees the other's
    /// bytes across any number of switches.
    #[test]
    fn a_page_two_processes_map_holds_each_ones_bytes_in_turn() {
        let base = committed(4);
        let a = new_token();
        let b = new_token();
        claim(a, base, base + PAGE_SIZE);
        set(base, 0xaa);
        // b runs, and maps the same page.
        claim(b, base, base + PAGE_SIZE);
        assert_eq!(byte(base), 0, "a newcomer's page is zeroed");
        set(base, 0xbb);
        assert_eq!(displaced_pages(a), 1, "a's bytes were saved");
        activate(a);
        assert_eq!(byte(base), 0xaa);
        assert_eq!(displaced_pages(b), 1);
        activate(b);
        assert_eq!(byte(base), 0xbb);
        activate(a);
        assert_eq!(byte(base), 0xaa);
    }

    /// A page only one process maps is never copied on a switch.
    #[test]
    fn a_page_nobody_else_maps_stays_where_it_is() {
        let base = committed(8);
        let a = new_token();
        let b = new_token();
        claim(a, base + 4 * PAGE_SIZE, base + 5 * PAGE_SIZE);
        set(base + 4 * PAGE_SIZE, 0x11);
        claim(b, base + 6 * PAGE_SIZE, base + 7 * PAGE_SIZE);
        activate(a);
        activate(b);
        assert_eq!(displaced_pages(a), 0);
        assert_eq!(displaced_pages(b), 0);
        assert_eq!(byte(base + 4 * PAGE_SIZE), 0x11);
    }

    /// A fork child has the parent's bytes and then diverges from them.
    #[test]
    fn a_fork_child_carries_the_bytes_and_then_diverges() {
        let base = committed(12);
        let at = base + 9 * PAGE_SIZE;
        let parent = new_token();
        claim(parent, at, at + PAGE_SIZE);
        set(at, 0x42);
        let child = new_token();
        fork(parent, child, &[(at, at + PAGE_SIZE)]);
        assert_eq!(displaced_pages(child), 1);
        activate(child);
        assert_eq!(byte(at), 0x42, "the child sees what the parent wrote");
        set(at, 0x43);
        activate(parent);
        assert_eq!(byte(at), 0x42, "the parent does not see the child's write");
        activate(child);
        assert_eq!(byte(at), 0x43);
    }

    /// A dead process's bytes are not saved, and are zeroed for whoever
    /// comes next.
    #[test]
    fn a_retired_processs_page_is_garbage_to_the_next_claimant() {
        let base = committed(16);
        let at = base + 13 * PAGE_SIZE;
        let gone = new_token();
        claim(gone, at, at + PAGE_SIZE);
        set(at, 0x99);
        retire(gone);
        let next = new_token();
        claim(next, at, at + PAGE_SIZE);
        assert_eq!(byte(at), 0);
        assert_eq!(displaced_pages(gone), 0);
    }

    /// Unmapping gives the page up: the next claimant saves nothing for a
    /// process that could not read it anyway.
    #[test]
    fn an_unmapped_page_is_not_saved_for_its_former_owner() {
        let base = committed(20);
        let at = base + 17 * PAGE_SIZE;
        let a = new_token();
        let b = new_token();
        claim(a, at, at + PAGE_SIZE);
        set(at, 0x55);
        release(a, at, at + PAGE_SIZE);
        claim(b, at, at + PAGE_SIZE);
        assert_eq!(byte(at), 0);
        assert_eq!(displaced_pages(a), 0);
    }
}
