//! Linear memory, natively.
//!
//! Inside a wasm module the guest address space needs no help: it *is* the
//! module's memory, and a guest address is already a host pointer. Natively
//! there is no such memory, and the whole value of running the engine
//! natively — unit tests in milliseconds, and the lockstep oracle that
//! compares the interpreter against a real process instruction by
//! instruction — depends on the two builds agreeing about what an address
//! means. So the native build maps an anonymous region *at the guest's own
//! addresses* and the identity holds on both sides. There is no second
//! address model, no base to add, and no `#[cfg]` anywhere on the memory
//! path.
//!
//! That is only possible because the guest lives below four gigabytes (the
//! wasm32 ceiling the design already accepts) and a position-independent
//! test binary does not: the low address space is free in this process, and
//! a container's own layout is exactly what gets mapped there.
//!
//! The host protection is read/write for everything. Guest protections are
//! [`crate::space::Space`]'s bitmaps, and they have to be, because a host
//! `mprotect` would fault the *interpreter* — a SIGSEGV in the engine's own
//! Rust frame — where the design needs a signal delivered to the guest.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::space::{CEILING, PAGE_SIZE};

/// Where the reservation starts.
///
/// Not zero: `vm.mmap_min_addr` forbids the first pages on every Linux
/// worth running on, and a guest wants them unmapped anyway — an
/// unmappable null page is the same null page a real process has.
pub const FLOOR: u64 = 0x1_0000;

/// Reserves the entire guest address space, once per process.
///
/// The reservation is `PROT_NONE` and `MAP_NORESERVE`: it commits no memory
/// and costs nothing but address space, of which a 64-bit process has more
/// than it can spend. What it buys is that **every address below four
/// gigabytes has an answer**. Without it, an engine defect that computes an
/// address past the guest's limit lands in whatever the host allocator
/// happened to put there and quietly works; with it, the same defect faults
/// at once and names itself. Inside the module that property is free — a
/// wasm load past `memory.size` traps — and this is what buys it natively,
/// which is the whole point of the native build being the same interpreter.
///
/// Arenas are then *commits* inside the reservation rather than mappings
/// beside it, and dropping one puts the range back to `PROT_NONE` so a
/// stale pointer faults instead of finding the next test's memory.
fn reserve() {
    use std::sync::OnceLock;
    static RESERVED: OnceLock<()> = OnceLock::new();
    RESERVED.get_or_init(|| {
        // SAFETY: an anonymous reservation at an address nothing else in
        // this process holds — `MAP_FIXED_NOREPLACE` is what makes that a
        // check rather than a belief.
        let mapped = unsafe {
            libc::mmap(
                FLOOR as usize as *mut libc::c_void,
                (CEILING - FLOOR) as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE
                    | libc::MAP_ANONYMOUS
                    | libc::MAP_NORESERVE
                    | libc::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        assert_ne!(
            mapped,
            libc::MAP_FAILED,
            "reserving the guest address space failed: {}. Something in this \
             process is already mapped below four gigabytes, which is where \
             the guest lives.",
            std::io::Error::last_os_error()
        );
    });
}

/// A region of the low address space, mapped for a guest to live in.
pub struct Arena {
    base: u64,
    length: u64,
}

/// One past the highest address a guest's linear memory can reach.
///
/// The address space is shared with [`Arena`], which hands out scratch
/// regions to tests, so the two are given halves rather than left to
/// collide: everything below this belongs to whichever guest is running,
/// and everything above it to the test that asked for it.
pub const GUEST_CEILING: u64 = 0x8000_0000;

/// Where [`Arena::new`] hands out regions from.
///
/// Above the guest's half, so a test's scratch memory and a guest's address
/// space cannot land on each other, and far below four gigabytes.
static NEXT: AtomicU64 = AtomicU64::new(GUEST_CEILING);

impl Arena {
    /// Maps `length` bytes at exactly `base`, or panics saying so.
    ///
    /// Exactly: a mapping that landed somewhere else would silently give the
    /// guest a different address space from the one the caller loaded a
    /// program for.
    pub fn at(base: u64, length: u64) -> Self {
        assert_eq!(base % PAGE_SIZE, 0, "an arena starts on a page boundary");
        let length = length.next_multiple_of(PAGE_SIZE);
        reserve();
        assert!(
            base >= FLOOR && base.saturating_add(length) <= CEILING,
            "an arena at {base:#x} for {length:#x} bytes is outside the guest \
             address space"
        );
        // `MAP_FIXED`, not `MAP_FIXED_NOREPLACE`: what is being replaced is
        // this process's own reservation, and replacing it is precisely the
        // commit. A collision with another *arena* is what the caller's
        // allocation discipline is for, not what this flag would catch.
        //
        // SAFETY: the range is inside the reservation, checked above.
        let mapped = unsafe {
            libc::mmap(
                base as usize as *mut libc::c_void,
                length as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        assert_ne!(
            mapped,
            libc::MAP_FAILED,
            "committing {length:#x} bytes of guest memory at {base:#x} failed: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            mapped as usize as u64, base,
            "the guest arena did not land where the guest expects it"
        );
        Self { base, length }
    }

    /// Maps `length` bytes wherever the next free region is.
    ///
    /// For everything that does not care about the address — scratch memory
    /// for a unit test, a stack, a heap — and, because the bump is atomic,
    /// safe to call from tests running in parallel in one process.
    pub fn new(length: u64) -> Self {
        let length = length.next_multiple_of(PAGE_SIZE);
        // A page of slack between regions so that a test running off the end
        // of one lands in nothing rather than in the next.
        let base = NEXT.fetch_add(length + PAGE_SIZE, Ordering::Relaxed);
        Self::at(base, length)
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    /// One past the highest address the region reaches — the `limit` a
    /// [`crate::space::Space`] over it is built with.
    pub fn limit(&self) -> u64 {
        self.base + self.length
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // Back to `PROT_NONE` rather than unmapped, so the reservation stays
        // whole and a pointer that outlived this arena faults instead of
        // finding whatever is allocated next.
        //
        // SAFETY: restoring exactly the range this arena committed.
        unsafe {
            libc::mmap(
                self.base as usize as *mut libc::c_void,
                self.length as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE
                    | libc::MAP_ANONYMOUS
                    | libc::MAP_NORESERVE
                    | libc::MAP_FIXED,
                -1,
                0,
            );
        }
    }
}

/// The guest's memory, natively: the shared reservation with a committed
/// prefix.
///
/// Inside the module this has no counterpart, because the module *is* the
/// memory: `memory.grow` moves the limit and the engine reads `memory.size`.
/// Natively the same two operations are a commit and a number, and this is
/// them — deliberately with the same shape, so that `Machine::grow` differs
/// between the two worlds in one line rather than in a policy.
///
/// One per host process, not one per guest process. Every process of a
/// container shares the one address range and keeps its pages resident
/// there, with `kisal::resident` saying whose bytes are at which page; the
/// arrangement before this one gave each process an anonymous file and made
/// a switch a `MAP_FIXED` of it, which was a page-table swap natively and a
/// whole-address-space copy inside the module, where there is no page table
/// to swap. One arrangement in both worlds is what lets the native tests
/// exercise the code the module runs.
static COMMITTED: AtomicU64 = AtomicU64::new(FLOOR);

/// The lock a container holds on the guest's addresses for as long as it
/// has pages there.
///
/// The invariant is "one container at a time in this *host* process": two
/// containers in one process — two tests, say — would otherwise keep their
/// bytes at the same addresses and read each other's. Taking it makes a
/// second container wait for the first to end, which is what happens, one
/// test thread at a time.
static ADDRESSES: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn hold_addresses() -> std::sync::MutexGuard<'static, ()> {
    reserve();
    ADDRESSES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// One past the highest committed address.
pub fn committed() -> u64 {
    COMMITTED.load(Ordering::Relaxed)
}

/// Commits memory up to `to`, readable and writable. Never shrinks, and
/// never fails for a `to` already committed.
pub fn commit(to: u64) -> bool {
    reserve();
    let current = committed();
    if to <= current {
        return true;
    }
    if to > GUEST_CEILING {
        return false;
    }
    let to = to.next_multiple_of(PAGE_SIZE);
    // SAFETY: a range inside the reservation, and `MAP_FIXED` over
    // `PROT_NONE` pages nothing has been given.
    let mapped = unsafe {
        libc::mmap(
            current as usize as *mut libc::c_void,
            (to - current) as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return false;
    }
    COMMITTED.store(to, Ordering::Relaxed);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_region_is_readable_writable_and_at_its_own_address() {
        let arena = Arena::new(0x2_0000);
        // SAFETY: the arena maps this range read/write.
        unsafe {
            let at = arena.base() as usize as *mut u64;
            at.write(0x0123_4567_89ab_cdef);
            assert_eq!(at.read(), 0x0123_4567_89ab_cdef);
        }
    }

    /// The property the reservation exists for: an address the guest could
    /// name but nothing has committed is not somebody else's memory.
    #[test]
    fn an_uncommitted_address_faults_rather_than_belonging_to_someone() {
        let arena = Arena::new(0x1_0000);
        // The page of slack this allocator leaves between regions, which is
        // the only address guaranteed to belong to nobody: one page further
        // on is exactly where the *next* region starts, and a test running
        // beside this one may well have committed it.
        let beyond = arena.limit();
        // Read it the way a fault-tolerant probe must: in a child, because
        // the whole point is that the access is fatal.
        // SAFETY: `fork` here does nothing between the fork and the read
        // that is not async-signal-safe.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork");
        if pid == 0 {
            // SAFETY: deliberately reading an address the reservation keeps
            // unreadable. The child exists to die.
            unsafe {
                // No core dump. Not tidiness: this machine's `core_pattern`
                // pipes to a crash reporter, which a zero `RLIMIT_CORE`
                // does not suppress — the kernel runs a pipe handler
                // regardless, so that crash reporters always see a crash.
                // `PR_SET_DUMPABLE` does suppress it, and the difference is
                // a second of the fast tier for a test whose whole content
                // is "the read faults".
                libc::prctl(libc::PR_SET_DUMPABLE, 0);
                let value = (beyond as usize as *const u8).read_volatile();
                libc::_exit(i32::from(value));
            }
        }
        let mut status = 0;
        // SAFETY: waiting on a child of this process.
        unsafe { libc::waitpid(pid, &raw mut status, 0) };
        assert!(
            libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSEGV,
            "reading {beyond:#x} did not fault (status {status:#x})"
        );
    }

    /// The address space is one place, so these run one at a time.
    fn exclusively<R>(body: impl FnOnce() -> R) -> R {
        use std::sync::Mutex;
        static ONE: Mutex<()> = Mutex::new(());
        let _held = ONE.lock().unwrap_or_else(|poison| poison.into_inner());
        body()
    }

    fn byte(at: u64) -> u8 {
        // SAFETY: callers only read committed, current memory.
        unsafe { (at as usize as *const u8).read_volatile() }
    }

    fn set(at: u64, value: u8) {
        // SAFETY: as above.
        unsafe { (at as usize as *mut u8).write_volatile(value) }
    }

    #[test]
    fn committed_memory_grows_and_never_shrinks() {
        let _held = hold_addresses();
        let was = committed();
        assert!(commit(was + 3 * PAGE_SIZE));
        let grown = committed();
        assert!(grown >= was + 3 * PAGE_SIZE);
        set(grown - 1, 7);
        assert_eq!(byte(grown - 1), 7);
        // Asking for less is a no-op, not a shrink.
        assert!(commit(was));
        assert_eq!(committed(), grown);
        assert_eq!(byte(grown - 1), 7);
        // And nothing past the guest's ceiling.
        assert!(!commit(GUEST_CEILING + PAGE_SIZE));
    }

    #[test]
    fn regions_do_not_overlap() {
        let first = Arena::new(0x1_0000);
        let second = Arena::new(0x1_0000);
        assert!(first.limit() <= second.base() || second.limit() <= first.base());
    }
}
