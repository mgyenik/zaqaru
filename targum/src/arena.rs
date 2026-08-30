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

/// The guest's linear memory, natively.
///
/// Inside the module this type has no counterpart, because the module *is*
/// the memory: `memory.grow` moves the limit and the engine reads
/// `memory.size`. Natively the same two operations are a commit and a
/// number, and this is them — deliberately with the same shape, so that
/// `Machine::grow` differs between the two worlds in one line rather than
/// in a policy.
///
/// # Why the bytes live in a file
///
/// A guest address *is* a host address, so at most one address space can be
/// at the guest's addresses at a time. That is not a limitation to work
/// around — it is the same fact that makes a process an instance in the
/// module world — but it does mean a second process's memory has to exist
/// *somewhere else* while it is not running.
///
/// So each address space is an anonymous file, and making one current is
/// one `MAP_FIXED` mapping of it over the guest's range: a page-table swap,
/// which is what a real kernel does at exactly this moment. Nothing is
/// copied on a switch. What *is* copied is a fork, which is the file, which
/// is the design's "a snapshot is a memcpy" with the kernel doing the copy.
pub struct LinearMemory {
    /// The backing file. Its length is the committed size.
    file: std::fs::File,
    limit: u64,
    /// Held while this memory is the one at the guest's addresses.
    ///
    /// A lock rather than a flag because the invariant is not "one at a time
    /// within a container" but "one at a time in this *process*": two guests
    /// running in one host process — two tests, say — would otherwise map
    /// their bytes at the same addresses and read each other's. Taking it on
    /// `activate` makes a switch a handoff, and makes a second container
    /// wait rather than corrupt.
    current: Option<std::sync::MutexGuard<'static, ()>>,
}

/// See [`LinearMemory::current`].
static ADDRESSES: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Default for LinearMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl LinearMemory {
    /// An address space with nothing committed, and not current.
    ///
    /// Page zero is never committed and never will be: a null dereference is
    /// a fault here for the same reason it is one in a real process.
    pub fn new() -> Self {
        reserve();
        // SAFETY: creating an anonymous file, which touches nothing else.
        let raw = unsafe {
            libc::memfd_create(c"targum-guest".as_ptr(), libc::MFD_CLOEXEC)
        };
        assert!(
            raw >= 0,
            "creating an address space failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: a descriptor this call just produced and nothing else
        // holds.
        let file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        Self {
            file,
            limit: FLOOR,
            current: None,
        }
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn is_current(&self) -> bool {
        self.current.is_some()
    }

    /// Puts this address space at the guest's addresses.
    ///
    /// The caller must have taken the previous one down first, which the
    /// assertion below turns from a rule into a check.
    pub fn activate(&mut self) {
        if self.current.is_some() {
            return;
        }
        let held = ADDRESSES
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.current = Some(held);
        self.map();
    }

    /// Takes it down, leaving the range unreadable.
    ///
    /// Not merely tidy: a pointer into a process that is not running must
    /// fault rather than read the process that is.
    pub fn deactivate(&mut self) {
        if self.current.is_none() {
            return;
        }
        if self.limit > FLOOR {
            // SAFETY: restoring exactly the range this memory occupied.
            unsafe {
                libc::mmap(
                    FLOOR as usize as *mut libc::c_void,
                    (self.limit - FLOOR) as usize,
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
        self.current = None;
    }

    /// Commits up to `to`, and answers whether the memory now reaches it.
    ///
    /// Never shrinks, and never past [`GUEST_CEILING`] — a request beyond
    /// that is `ENOMEM` at the row above, which is what a guest asking for
    /// more than the machine has should get.
    pub fn grow(&mut self, to: u64) -> bool {
        if to <= self.limit {
            return true;
        }
        if to > GUEST_CEILING {
            return false;
        }
        let to = to.next_multiple_of(PAGE_SIZE);
        if self.file.set_len(to - FLOOR).is_err() {
            return false;
        }
        self.limit = to;
        if self.current.is_some() {
            self.map();
        }
        true
    }

    /// A copy of this address space, not current — which is a fork.
    ///
    /// The copy is the kernel's, through the page cache, rather than a loop
    /// here: the two files are the same size and the bytes go straight
    /// across.
    pub fn duplicate(&self) -> Option<Self> {
        let mut child = Self::new();
        let length = self.limit - FLOOR;
        if child.file.set_len(length).is_err() {
            return None;
        }
        child.limit = self.limit;
        let mut copied = 0i64;
        while (copied as u64) < length {
            let mut from = copied;
            let mut into = copied;
            // SAFETY: two descriptors this process owns, with offsets and a
            // length inside both files.
            let moved = unsafe {
                libc::copy_file_range(
                    std::os::fd::AsRawFd::as_raw_fd(&self.file),
                    &raw mut from,
                    std::os::fd::AsRawFd::as_raw_fd(&child.file),
                    &raw mut into,
                    (length - copied as u64) as usize,
                    0,
                )
            };
            if moved <= 0 {
                return None;
            }
            copied += moved as i64;
        }
        Some(child)
    }

    /// Maps the file over the guest's range.
    ///
    /// Nothing to do for an address space with nothing committed: an
    /// address space that has not grown yet is a process that has not
    /// started, and a zero-length mapping is an error rather than a no-op.
    fn map(&self) {
        if self.limit <= FLOOR {
            return;
        }
        // `MAP_SHARED`, so that what the guest writes lands in the file and
        // survives being taken down and put back.
        //
        // SAFETY: the range is inside this process's own reservation, and
        // `MAP_FIXED` over it is the switch.
        let mapped = unsafe {
            libc::mmap(
                FLOOR as usize as *mut libc::c_void,
                (self.limit - FLOOR) as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                std::os::fd::AsRawFd::as_raw_fd(&self.file),
                0,
            )
        };
        assert_ne!(
            mapped,
            libc::MAP_FAILED,
            "mapping an address space at the guest's addresses failed: {}",
            std::io::Error::last_os_error()
        );
    }

}

impl Drop for LinearMemory {
    fn drop(&mut self) {
        self.deactivate();
    }
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
    fn linear_memory_grows_and_never_shrinks() {
        exclusively(|| {
            let mut memory = LinearMemory::new();
            memory.activate();
            assert_eq!(memory.limit(), FLOOR);
            assert!(memory.grow(FLOOR + 0x1000));
            assert_eq!(memory.limit(), FLOOR + 0x1000);
            set(FLOOR, 0x5a);
            assert_eq!(byte(FLOOR), 0x5a);
            assert!(memory.grow(FLOOR), "a smaller request is already satisfied");
            assert_eq!(memory.limit(), FLOOR + 0x1000);
            assert!(!memory.grow(GUEST_CEILING + 1), "past the guest's half");
        });
    }

    /// Two address spaces, one set of addresses: the switch is what makes a
    /// second process possible at all, and it must carry the bytes.
    #[test]
    fn switching_address_spaces_swaps_what_is_there() {
        exclusively(|| {
            let mut first = LinearMemory::new();
            first.activate();
            assert!(first.grow(FLOOR + 0x1000));
            set(FLOOR, 1);

            let mut second = LinearMemory::new();
            first.deactivate();
            second.activate();
            assert!(second.grow(FLOOR + 0x1000));
            assert_eq!(byte(FLOOR), 0, "a fresh address space is zeros");
            set(FLOOR, 2);

            second.deactivate();
            first.activate();
            assert_eq!(byte(FLOOR), 1, "and the first one kept its own");
        });
    }

    /// A fork: the child starts with the parent's bytes and then diverges.
    #[test]
    fn a_duplicate_carries_the_bytes_and_then_diverges() {
        exclusively(|| {
            let mut parent = LinearMemory::new();
            parent.activate();
            assert!(parent.grow(FLOOR + 0x2000));
            set(FLOOR, 0x11);
            set(FLOOR + 0x1000, 0x22);

            let mut child = parent.duplicate().expect("duplicate");
            assert_eq!(child.limit(), parent.limit());
            parent.deactivate();
            child.activate();
            assert_eq!(byte(FLOOR), 0x11, "the child inherited");
            assert_eq!(byte(FLOOR + 0x1000), 0x22);
            set(FLOOR, 0x33);

            child.deactivate();
            parent.activate();
            assert_eq!(byte(FLOOR), 0x11, "and the parent is untouched");
        });
    }

    /// An address space that is not current must not be readable through the
    /// guest's addresses — otherwise a stale pointer in one process reads
    /// another's memory, which is the whole failure this arrangement exists
    /// to prevent.
    #[test]
    fn a_dormant_address_space_is_not_readable() {
        exclusively(|| {
            let mut memory = LinearMemory::new();
            memory.activate();
            assert!(memory.grow(FLOOR + 0x1000));
            set(FLOOR, 0x7e);
            memory.deactivate();

            // SAFETY: `fork` here does nothing between the fork and the read
            // that is not async-signal-safe. The child exists to die.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork");
            if pid == 0 {
                unsafe {
                    let none = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    libc::setrlimit(libc::RLIMIT_CORE, &none);
                    libc::prctl(libc::PR_SET_DUMPABLE, 0);
                    let value = (FLOOR as usize as *const u8).read_volatile();
                    libc::_exit(i32::from(value));
                }
            }
            let mut status = 0;
            // SAFETY: waiting on a child of this process.
            unsafe { libc::waitpid(pid, &raw mut status, 0) };
            assert!(
                libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSEGV,
                "reading a dormant address space did not fault (status {status:#x})"
            );
        });
    }

    #[test]
    fn regions_do_not_overlap() {
        let first = Arena::new(0x1_0000);
        let second = Arena::new(0x1_0000);
        assert!(first.limit() <= second.base() || second.limit() <= first.base());
    }
}
