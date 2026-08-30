//! The kernel's other downward face: the emulated machine's registers.
//!
//! The store ([`crate::abi`]) is how the kernel reaches the world; this is how
//! it reaches the guest's own state. Both are traits for the same reason —
//! the kernel's logic is ordinary Rust that gets unit-tested natively, and a
//! wasm global is not something Rust can name.
//!
//! Only cells the kernel touches *individually* live here. Moving the whole
//! register file at a context switch is `x86_save_machine`/`x86_load_machine`,
//! which is a memory operation and needs no accessor at all.

/// The machine cells the kernel reads and writes one at a time.
pub trait Machine {
    /// The `%fs` base — the thread pointer, as far as every libc is
    /// concerned. `arch_prctl` is what moves it.
    fn segment_base(&self) -> i64;
    fn set_segment_base(&mut self, value: i64);

    /// `%rsp`. Boot is what writes it: every other register a process starts
    /// with is zero, and a wasm global starts at zero, so the stack pointer
    /// is the only cell between a fresh instance and the state Linux hands
    /// `_start`.
    fn stack_pointer(&self) -> i64;
    fn set_stack_pointer(&mut self, value: i64);

    /// Puts the floating-point unit back the way `execve` leaves it.
    ///
    /// A fresh image gets a fresh FPU: `FNINIT` state, empty registers, the
    /// default control word. Nothing else resets it, and the state is not in
    /// the register file — it lives inside the `x87` crate — so the one
    /// place that starts a new program has to say so.
    ///
    /// Fork and snapshot need nothing of the kind: that state is linear
    /// memory, and a snapshot already carries it.
    fn reset_floating_point(&mut self);

    /// One past the highest byte the guest can address. Every syscall that
    /// takes a pointer is bounded by this before it dereferences anything.
    fn memory_limit(&self) -> u64;

    /// Makes the address space reach at least `to`, and reports whether it
    /// does now.
    ///
    /// Wasm memory grows in 64 KiB pages and never shrinks, so this is the
    /// only way the guest's address space ever gets bigger — and the reason
    /// `munmap` returns ranges to a pool instead of giving them back.
    /// Freshly grown memory is zero, which is what lets anonymous mappings
    /// above the high-water mark skip being filled.
    fn grow(&mut self, to: u64) -> bool;
}

/// The real machine: the generated accessors the seam object defines.
///
/// Declared as plain undefined symbols rather than as host imports, because
/// they are neither — the seam defines them inside the same link, and
/// `wasm-ld` resolves them there.
pub struct GuestMachine;

#[cfg(target_arch = "wasm32")]
unsafe extern "C" {
    #[link_name = "x86_get_fs_base"]
    fn get_segment_base() -> i64;
    #[link_name = "x86_set_fs_base"]
    fn set_segment_base(value: i64);
    #[link_name = "x86_get_rsp"]
    fn get_stack_pointer() -> i64;
    #[link_name = "x86_set_rsp"]
    fn set_stack_pointer(value: i64);
    // Defined by the `x87` crate, which is in every container's link for
    // the same reason kisal is.
    #[link_name = "x87_reset"]
    fn reset_floating_point();
}

#[cfg(target_arch = "wasm32")]
impl Machine for GuestMachine {
    fn segment_base(&self) -> i64 {
        unsafe { get_segment_base() }
    }

    fn set_segment_base(&mut self, value: i64) {
        unsafe { set_segment_base(value) }
    }

    fn stack_pointer(&self) -> i64 {
        unsafe { get_stack_pointer() }
    }

    fn set_stack_pointer(&mut self, value: i64) {
        unsafe { set_stack_pointer(value) }
    }

    fn reset_floating_point(&mut self) {
        unsafe { reset_floating_point() }
    }

    fn memory_limit(&self) -> u64 {
        // Memory only ever grows, so this is read per syscall rather than
        // cached: a `memory.grow` between two syscalls must widen what the
        // second one accepts.
        core::arch::wasm32::memory_size(0) as u64 * 65536
    }

    fn grow(&mut self, to: u64) -> bool {
        const WASM_PAGE: u64 = 65536;
        let current = self.memory_limit();
        if to <= current {
            return true;
        }
        // In amortised chunks: growth is a host `mmap` under wasmtime, and
        // one per 4 KiB reservation would be one syscall per page.
        let wanted = to
            .max(current + crate::space::GROW_CHUNK)
            .next_multiple_of(WASM_PAGE);
        let pages = ((wanted - current) / WASM_PAGE) as usize;
        // `memory.grow` answers −1 when it cannot, which is `ENOMEM` at the
        // row above rather than a trap here.
        core::arch::wasm32::memory_grow(0, pages) != usize::MAX
    }
}

/// Off wasm there are no globals to reach. The type exists so that the rest
/// of the kernel compiles unchanged for its native tests, which supply a
/// double of their own.
#[cfg(not(target_arch = "wasm32"))]
impl Machine for GuestMachine {
    fn segment_base(&self) -> i64 {
        unreachable!("the guest machine exists only inside the wasm module")
    }

    fn set_segment_base(&mut self, _value: i64) {
        unreachable!("the guest machine exists only inside the wasm module")
    }

    fn stack_pointer(&self) -> i64 {
        unreachable!("the guest machine exists only inside the wasm module")
    }

    fn set_stack_pointer(&mut self, _value: i64) {
        unreachable!("the guest machine exists only inside the wasm module")
    }

    fn reset_floating_point(&mut self) {
        unreachable!("the guest machine exists only inside the wasm module")
    }

    fn memory_limit(&self) -> u64 {
        unreachable!("the guest machine exists only inside the wasm module")
    }

    fn grow(&mut self, _to: u64) -> bool {
        unreachable!("the guest machine exists only inside the wasm module")
    }
}

/// A machine that is only its registers: the native tests' stand-in, and the
/// shape a thread control block's saved register file will have.
pub struct Registers {
    pub segment_base: i64,
    pub stack_pointer: i64,
    /// The address space a native test is pretending to have. The whole of
    /// it by default, because a native test hands over real host pointers
    /// and is not usually about the bound; a test that *is* about the bound
    /// sets a smaller one.
    ///
    /// Not `u64::MAX`, which it used to be: the page table is sized against
    /// the limit, and an address space larger than the machine has is not a
    /// generous default but a bitmap covering half a petabyte.
    pub memory_limit: u64,
    /// How far [`Machine::grow`] may raise that limit. A test that exercises
    /// the memory rows sets this to the end of the buffer it owns, so that
    /// every address the kernel hands out is one the test can actually read.
    pub ceiling: u64,
    /// How many times the floating-point unit has been reset. There is no
    /// unit here to reset, so the count is the whole of what a native test
    /// can observe — and it is what makes "`execve` resets the FPU" a
    /// statement a test can check rather than a comment.
    pub floating_point_resets: usize,
}

impl Default for Registers {
    fn default() -> Self {
        Self {
            segment_base: 0,
            stack_pointer: 0,
            memory_limit: targum::space::CEILING,
            ceiling: targum::space::CEILING,
            floating_point_resets: 0,
        }
    }
}

/// The machine the interpreter runs.
///
/// **The thread control block lives here, and here is inside the kernel** —
/// `Kernel` owns its `machine`, so owning the control block through it is
/// the ownership `docs/vm.md` section 3 asks for ("the TCB, owned by kisal
/// as M7 always intended") with no second path to the same state. The six
/// places the kernel reaches for machine state do not change shape at all;
/// they simply land on a control block instead of on a wasm global.
///
/// What is *not* here is the address space, which is the kernel's own field
/// because the mapping rows write it, and the block cache, which is the
/// scheduler's. See [`crate::run::Process`].
pub struct Interpreted {
    pub thread: targum::state::Tcb,
    /// Linear memory. Inside the module there is nothing to hold — the
    /// module *is* the memory — and natively it is a reservation with a
    /// committed prefix. One line of difference, which is what the design
    /// asked for.
    #[cfg(not(target_arch = "wasm32"))]
    memory: targum::arena::LinearMemory,
}

impl Default for Interpreted {
    fn default() -> Self {
        Self::new()
    }
}

/// How much of the address space a program is left, natively.
///
/// Inside the module this is `baker::layout`'s job: a container carrying a
/// program has the module's own data placed above everything that program
/// will occupy, so linear memory already reaches past the program's top
/// before the kernel exists and `Kernel::new` carves the guest's arenas from
/// there up. Natively there is no bake and no linker to do it, so the same
/// reservation is made here — grow past the program's region first, and the
/// arenas land above it.
///
/// Sixty-four megabytes because a static glibc program tops out a few
/// megabytes in and a static CPython an order of magnitude further, and
/// because an untouched reservation costs nothing but address space. A
/// program that exceeds it does not corrupt anything: its `MAP_FIXED`
/// segment collides with the arena and `exec` refuses to load it, by name.
#[cfg(not(target_arch = "wasm32"))]
pub const PROGRAM_REGION: u64 = 64 << 20;

impl Interpreted {
    pub fn new() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let memory = {
            let mut memory = targum::arena::LinearMemory::new();
            assert!(
                memory.grow(PROGRAM_REGION),
                "reserving the program's region failed"
            );
            memory
        };
        Self {
            thread: targum::state::Tcb::new(),
            #[cfg(not(target_arch = "wasm32"))]
            memory,
        }
    }
}

impl Machine for Interpreted {
    fn segment_base(&self) -> i64 {
        self.thread.fs_base as i64
    }

    fn set_segment_base(&mut self, value: i64) {
        self.thread.fs_base = value as u64;
    }

    fn stack_pointer(&self) -> i64 {
        self.thread.stack_pointer() as i64
    }

    fn set_stack_pointer(&mut self, value: i64) {
        self.thread.set_stack_pointer(value as u64);
    }

    /// A fresh process gets a fresh unit — and *erased*, not merely emptied.
    /// `fninit` marks the stack unreachable and leaves the bytes; `execve`
    /// must not hand one program another's.
    fn reset_floating_point(&mut self) {
        self.thread.x87.reset();
    }

    #[cfg(target_arch = "wasm32")]
    fn memory_limit(&self) -> u64 {
        core::arch::wasm32::memory_size(0) as u64 * 65536
    }

    #[cfg(target_arch = "wasm32")]
    fn grow(&mut self, to: u64) -> bool {
        GuestMachine.grow(to)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn memory_limit(&self) -> u64 {
        self.memory.limit()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn grow(&mut self, to: u64) -> bool {
        self.memory.grow(to)
    }
}

/// A buffer at a *guest* address, for the native tests that hand the kernel
/// a pointer.
///
/// A test used to declare a stack array and pass its host address as a guest
/// address, which worked while the kernel dereferenced whatever it was
/// given. The page table forbids exactly that: the guest's address space is
/// four gigabytes and a host stack pointer is nowhere near it, so the kernel
/// now answers `EFAULT` — correctly. So a test that hands over a pointer
/// allocates the bytes where a guest's memory really is, low and inside the
/// address space, and this is the allocation.
///
/// It dereferences to `[u8; N]`, so a test reads its result exactly as it
/// read the array it replaced.
#[cfg(not(target_arch = "wasm32"))]
pub struct GuestBytes<const N: usize> {
    arena: targum::arena::Arena,
}

#[cfg(not(target_arch = "wasm32"))]
impl<const N: usize> Default for GuestBytes<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<const N: usize> GuestBytes<N> {
    pub fn new() -> Self {
        let arena = targum::arena::Arena::new(N.max(1) as u64);
        Self { arena }
    }

    /// The guest address the bytes live at, as a syscall argument.
    pub fn address(&self) -> i64 {
        self.arena.base() as i64
    }
}

/// The same, with a length known only at run time — for the tests that hand
/// the kernel a string or a buffer rather than a fixed-size record.
#[cfg(not(target_arch = "wasm32"))]
pub struct GuestBuffer {
    arena: targum::arena::Arena,
    length: usize,
}

#[cfg(not(target_arch = "wasm32"))]
impl GuestBuffer {
    /// A copy of `bytes`, at a guest address.
    pub fn of(bytes: &[u8]) -> Self {
        let arena = targum::arena::Arena::new(bytes.len().max(1) as u64);
        // SAFETY: the arena committed at least this many bytes here.
        unsafe {
            (arena.base() as usize as *mut u8).copy_from(bytes.as_ptr(), bytes.len());
        }
        Self {
            arena,
            length: bytes.len(),
        }
    }

    pub fn address(&self) -> i64 {
        self.arena.base() as i64
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl core::ops::Deref for GuestBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: the arena committed `length` bytes at this address.
        unsafe { core::slice::from_raw_parts(self.arena.base() as usize as *const u8, self.length) }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<const N: usize> core::ops::Deref for GuestBytes<N> {
    type Target = [u8; N];

    fn deref(&self) -> &Self::Target {
        // SAFETY: the arena committed at least `N` bytes at this address.
        unsafe { &*(self.arena.base() as usize as *const [u8; N]) }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<const N: usize> core::ops::DerefMut for GuestBytes<N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: as above.
        unsafe { &mut *(self.arena.base() as usize as *mut [u8; N]) }
    }
}

impl Machine for Registers {
    fn segment_base(&self) -> i64 {
        self.segment_base
    }

    fn set_segment_base(&mut self, value: i64) {
        self.segment_base = value;
    }

    fn stack_pointer(&self) -> i64 {
        self.stack_pointer
    }

    fn set_stack_pointer(&mut self, value: i64) {
        self.stack_pointer = value;
    }

    fn reset_floating_point(&mut self) {
        self.floating_point_resets += 1;
    }

    fn memory_limit(&self) -> u64 {
        self.memory_limit
    }

    /// A native test's address space is a buffer it allocated, so this can
    /// only say whether the request fits inside it. `ceiling` is how far a
    /// test lets the kernel grow; the default is the limit itself, which
    /// makes growth impossible and is right for every test that is not
    /// about memory.
    fn grow(&mut self, to: u64) -> bool {
        if to <= self.memory_limit {
            return true;
        }
        if to > self.ceiling {
            return false;
        }
        self.memory_limit = to;
        true
    }
}
