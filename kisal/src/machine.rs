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

    /// The kernel's own per-thread cells — the signal mask, the
    /// `clear_child_tid` word, the robust list.
    ///
    /// Reached through the machine for the same reason `%fs` is: they belong
    /// to *a thread*, and the machine is what knows which thread is running.
    /// A world with one thread keeps one of these inline; a world with a
    /// scheduler hands back the current one's.
    fn owned(&mut self) -> &mut crate::thread::Owned;

    /// This thread's identifier.
    ///
    /// The defaults below are a *world with one thread*, which is exactly
    /// what the ahead-of-time machine is: its registers are wasm globals and
    /// there is nowhere to put a second set. Every answer here is true of
    /// such a world rather than a stand-in for a better one — one thread has
    /// the first identifier, cannot create a second, cannot park (nothing
    /// could wake it), and wakes nobody.
    fn current_tid(&self) -> i32 {
        crate::thread::FIRST
    }

    /// Creates a thread, and answers its identifier — or `None` where a
    /// second thread cannot exist.
    fn spawn(&mut self, _request: &crate::thread::Spawn) -> Option<i32> {
        None
    }

    /// Parks this thread on a futex word, and reports whether it could be.
    fn park(&mut self, _word: u64, _bitset: u32) -> bool {
        false
    }

    /// Wakes up to `count` threads parked on `word`, and answers how many.
    fn wake(&mut self, _word: u64, _bitset: u32, _count: usize) -> usize {
        0
    }

    /// Parks this thread part-way through a pipe transfer.
    ///
    /// Nothing to park on a machine with one thread and no scheduler, and
    /// that is why a pipe read on the ahead-of-time path would hang rather
    /// than wait — which is a thing to know about that path, not a gap in
    /// this one.
    fn park_on_transfer(&mut self, _transfer: crate::pipe::Transfer) -> bool {
        false
    }

    /// Marks a signal pending on some thread that has not blocked it.
    ///
    /// What a *process*-directed signal means: `kill(2)` names a process,
    /// and Linux hands the signal to whichever of its threads is willing to
    /// take it. Answers false when every thread has it blocked, in which
    /// case it stays pending on the process and nothing runs — which is also
    /// what Linux does.
    fn raise_process(&mut self, signal: i32) -> bool {
        let bit = 1u64 << (signal - 1);
        let owned = self.owned();
        if owned.blocked_signals & bit != 0 {
            return false;
        }
        owned.pending_signals |= bit;
        true
    }

    /// Puts this process's address space at the guest's addresses, and
    /// takes it down again.
    ///
    /// Nothing to do by default, and that is the honest answer for the
    /// ahead-of-time machine rather than a gap in it: there the address
    /// space is the module instance's own memory and a second process is a
    /// second instance, so no machine ever holds two. It is the interpreter
    /// that runs every process in one engine and therefore has to say which
    /// one the bytes belong to.
    fn activate(&mut self, pages: &targum::space::Space) {
        let _ = pages;
    }

    fn deactivate(&mut self, pages: &targum::space::Space) {
        let _ = pages;
    }


    /// Ends this thread, and answers how many are left.
    ///
    /// Zero from the default: the one thread ending *is* the process ending,
    /// which is what the caller reads it as.
    fn exit_current(&mut self, _status: i32) -> usize {
        0
    }

    /// This thread's registers, where the world keeps them in a control
    /// block it can hand out.
    ///
    /// `None` for the ahead-of-time machine, whose registers are wasm
    /// globals: there is no block, and building a signal frame out of them
    /// is the chain surgery that world's design calls M10. The rows that
    /// need one say so loudly rather than pretending.
    fn tcb(&mut self) -> Option<&mut targum::state::Tcb> {
        None
    }

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
#[derive(Default)]
pub struct GuestMachine {
    /// See [`Machine::owned`]. One, because this world has one thread.
    owned: crate::thread::Owned,
}

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
    fn owned(&mut self) -> &mut crate::thread::Owned {
        &mut self.owned
    }

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
    fn owned(&mut self) -> &mut crate::thread::Owned {
        &mut self.owned
    }

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
    /// See [`Machine::owned`].
    pub owned: crate::thread::Owned,
}

impl Default for Registers {
    fn default() -> Self {
        Self {
            segment_base: 0,
            stack_pointer: 0,
            memory_limit: targum::space::CEILING,
            ceiling: targum::space::CEILING,
            floating_point_resets: 0,
            owned: crate::thread::Owned::default(),
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
    /// Every thread this process has, and which one is running. A context
    /// switch is choosing a different index.
    pub threads: crate::thread::Threads,
    /// Linear memory, natively: a reservation with a committed prefix,
    /// backed by a file so that a process that is not running still has its
    /// bytes somewhere. Making one current is one `MAP_FIXED` mapping of it
    /// over the guest's range — a page-table swap, and nothing is copied.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) memory: targum::arena::LinearMemory,
    /// The same thing inside the module, where there is no page table to
    /// swap: a dormant process's pages, held in the kernel's own heap.
    ///
    /// `Some` exactly when this process is *not* the one at the guest's
    /// addresses, which is the invariant `activate` and `deactivate`
    /// maintain between them.
    #[cfg(target_arch = "wasm32")]
    pub(crate) dormant: Option<Dormant>,
}

/// A process's bytes while some other process is the one running.
///
/// Inside the module linear memory is shared with the engine — the
/// program's segments low, the module's own data above them, the guest's
/// arenas above that, all in one memory — so this cannot be a range. It is
/// the pages the guest's own page table describes, and nothing else, which
/// is what keeps a switch from writing over the engine that is performing
/// it.
///
/// The bill, stated: a switch is a copy, not a mapping, and it costs a
/// memcpy of everything the process has mapped. Natively the same operation
/// is free. What keeps it affordable is that a switch happens only when the
/// running process cannot continue or has used a whole process quantum —
/// and the shape that matters, a `fork` whose parent immediately waits,
/// costs two.
#[cfg(target_arch = "wasm32")]
pub struct Dormant {
    /// Page address and its contents, ascending.
    pages: Vec<(u64, [u8; PAGE_BYTES])>,
}

#[cfg(target_arch = "wasm32")]
const PAGE_BYTES: usize = targum::space::PAGE_SIZE as usize;

#[cfg(target_arch = "wasm32")]
impl Dormant {
    /// Takes a copy of everything the page table says is mapped.
    ///
    /// # Safety
    /// A guest address is a linear-memory offset, so a mapped page is
    /// `PAGE_BYTES` readable bytes at that offset — which is the identity
    /// the whole design rests on, and the same one every load and store in
    /// [`targum::space`] uses.
    pub(crate) fn taken(pages: &targum::space::Space) -> Self {
        let mut held = Vec::new();
        for address in pages.mapped_pages() {
            let mut bytes = [0u8; PAGE_BYTES];
            // SAFETY: the page is inside the limit and mapped, which is what
            // `mapped_pages` answered.
            let from = unsafe {
                core::slice::from_raw_parts(address as usize as *const u8, PAGE_BYTES)
            };
            bytes.copy_from_slice(from);
            held.push((address, bytes));
        }
        Self { pages: held }
    }

    /// Writes them back where they came from.
    pub(crate) fn restore(&self) {
        for (address, bytes) in &self.pages {
            // SAFETY: the page was inside the limit when it was taken, and
            // linear memory never shrinks.
            let into = unsafe {
                core::slice::from_raw_parts_mut(*address as usize as *mut u8, PAGE_BYTES)
            };
            into.copy_from_slice(bytes);
        }
    }
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
    /// The thread the loop is advancing.
    pub fn thread(&self) -> &targum::state::Tcb {
        &self.threads.current().tcb
    }

    pub fn thread_mut(&mut self) -> &mut targum::state::Tcb {
        &mut self.threads.current_mut().tcb
    }

    pub fn new() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let memory = {
            let mut memory = targum::arena::LinearMemory::new();
            // A process that is being built is the process that is running:
            // `exec` writes the program's segments into it, and there is
            // nowhere for those bytes to go until the address space is at
            // the guest's addresses.
            memory.activate();
            assert!(
                memory.grow(PROGRAM_REGION),
                "reserving the program's region failed"
            );
            memory
        };
        Self {
            threads: crate::thread::Threads::new(),
            #[cfg(not(target_arch = "wasm32"))]
            memory,
            // A process that is being built is the process that is running.
            #[cfg(target_arch = "wasm32")]
            dormant: None,
        }
    }
}

impl Machine for Interpreted {
    fn owned(&mut self) -> &mut crate::thread::Owned {
        &mut self.threads.current_mut().owned
    }

    fn current_tid(&self) -> i32 {
        self.threads.current().tid
    }

    fn tcb(&mut self) -> Option<&mut targum::state::Tcb> {
        Some(self.thread_mut())
    }

    /// A thread is a control block with `%rsp` and `%rip` set.
    ///
    /// That sentence is the design's whole claim about threads, and this is
    /// it: the child is a copy of the caller's registers with a new stack, a
    /// return value of zero, and — because `syscall` already advanced it —
    /// the same program counter, so both sides return from the same call to
    /// different answers exactly as they do on Linux.
    fn spawn(&mut self, request: &crate::thread::Spawn) -> Option<i32> {
        let mut tcb = self.thread().clone();
        tcb.set_stack_pointer(request.stack);
        // The child's `clone` returns zero; the parent's returns the tid.
        tcb.registers[0] = 0;
        if let Some(tls) = request.tls {
            tcb.fs_base = tls;
        }
        // A fresh unit, not the caller's: the x87 stack is per-thread, and
        // handing a new thread the parent's is the cross-thread corruption
        // the thread design was written to avoid.
        tcb.x87.reset();
        let tid = self.threads.spawn(tcb);
        if let Some(thread) = self.threads.find_mut(tid) {
            thread.owned.clear_child_tid = request.clear_child_tid;
        }
        Some(tid)
    }

    fn park(&mut self, word: u64, bitset: u32) -> bool {
        self.threads.current_mut().state = crate::thread::State::Waiting { word, bitset };
        true
    }

    fn park_on_transfer(&mut self, transfer: crate::pipe::Transfer) -> bool {
        self.threads.current_mut().state = crate::thread::State::Transferring(transfer);
        true
    }

    fn wake(&mut self, word: u64, bitset: u32, count: usize) -> usize {
        self.threads.wake(word, bitset, count)
    }

    fn exit_current(&mut self, status: i32) -> usize {
        self.threads.current_mut().state = crate::thread::State::Exited { status };
        self.threads.live()
    }

    /// The current thread first, so a program that raises a signal at itself
    /// sees it on the thread that raised it — which is what every `raise(3)`
    /// expects, and what a `SIGSEGV` handler debugging its own thread needs.
    fn activate(&mut self, pages: &targum::space::Space) {
        Interpreted::activate(self, pages);
    }

    fn deactivate(&mut self, pages: &targum::space::Space) {
        Interpreted::deactivate(self, pages);
    }

    fn raise_process(&mut self, signal: i32) -> bool {
        let bit = 1u64 << (signal - 1);
        if self.threads.current().owned.blocked_signals & bit == 0 {
            self.threads.current_mut().owned.pending_signals |= bit;
            return true;
        }
        for thread in self.threads.all_mut() {
            if thread.owned.blocked_signals & bit == 0 {
                thread.owned.pending_signals |= bit;
                return true;
            }
        }
        false
    }

    fn segment_base(&self) -> i64 {
        self.thread().fs_base as i64
    }

    fn set_segment_base(&mut self, value: i64) {
        self.thread_mut().fs_base = value as u64;
    }

    fn stack_pointer(&self) -> i64 {
        self.thread().stack_pointer() as i64
    }

    fn set_stack_pointer(&mut self, value: i64) {
        self.thread_mut().set_stack_pointer(value as u64);
    }

    /// A fresh process gets a fresh unit — and *erased*, not merely emptied.
    /// `fninit` marks the stack unreachable and leaves the bytes; `execve`
    /// must not hand one program another's.
    fn reset_floating_point(&mut self) {
        self.thread_mut().x87.reset();
    }

    #[cfg(target_arch = "wasm32")]
    fn memory_limit(&self) -> u64 {
        core::arch::wasm32::memory_size(0) as u64 * 65536
    }

    #[cfg(target_arch = "wasm32")]
    fn grow(&mut self, to: u64) -> bool {
        GuestMachine::default().grow(to)
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
    fn owned(&mut self) -> &mut crate::thread::Owned {
        &mut self.owned
    }

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
