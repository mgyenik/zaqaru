//! The kernel's other downward face: the emulated machine's registers.
//!
//! The store ([`crate::abi`]) is how the kernel reaches the world; this is how
//! it reaches the guest's own state. Both are traits for the same reason —
//! the kernel's logic is ordinary Rust that gets unit-tested natively, on a
//! machine that is nothing but a register file ([`Registers`]), and runs
//! for real on the interpreter's thread control blocks ([`Interpreted`]).
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
    /// The defaults below are a *world with one thread*, which is what the
    /// native tests' register file is: one set of registers and nowhere to
    /// put a second. Every answer here is true of such a world rather than
    /// a stand-in for a better one — one thread has the first identifier,
    /// cannot create a second, cannot park (nothing could wake it), and
    /// wakes nobody.
    fn current_tid(&self) -> i32 {
        crate::thread::FIRST
    }

    /// Creates a thread, and answers its identifier — or `None` where a
    /// second thread cannot exist.
    fn spawn(&mut self, _request: &crate::thread::Spawn) -> Option<i32> {
        None
    }

    /// Parks this thread on a futex word, and reports whether it could be.
    fn park(&mut self, _word: u64, _bitset: u32, _deadline: Option<u64>) -> bool {
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
    fn park_on_transfer(&mut self, _transfer: crate::ring::Transfer) -> bool {
        false
    }

    /// Parks this thread in a `poll` or an `epoll_wait`.
    fn park_on_watch(&mut self, _watching: crate::thread::Watching) -> bool {
        false
    }

    /// Parks this thread in `pause`, waiting for a signal.
    fn park_on_signal(&mut self) -> bool {
        false
    }

    /// Parks this thread in `accept`, waiting for a connection.
    fn park_on_accept(&mut self, _waiting: crate::thread::Accepting) -> bool {
        false
    }

    /// Parks this thread until a deadline, which is `nanosleep`.
    fn park_on_deadline(&mut self, _deadline: u64) -> bool {
        false
    }

    /// Parks this thread on an `eventfd` counter.
    fn park_on_event(&mut self, _waiting: crate::thread::Eventing) -> bool {
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

    /// Where a new process's address space starts.
    ///
    /// The top of memory by default, which is right whenever a process's
    /// bytes live somewhere of their own — natively they are a file per
    /// process, and on the ahead-of-time path there is one process. It is
    /// wrong inside the module, where they share the one linear memory, and
    /// [`Interpreted`] overrides it there. See `GUEST_BASE`.
    fn guest_base(&mut self) -> u64 {
        self.memory_limit()
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

    /// This process is about to map `[start, end)`, or has stopped mapping
    /// it. Nothing to do by default, for the reason `activate` gives; the
    /// interpreter's machine keeps the owner table — see `crate::resident`.
    fn claim_pages(&mut self, start: u64, end: u64) {
        let _ = (start, end);
    }

    fn release_pages(&mut self, start: u64, end: u64) {
        let _ = (start, end);
    }

    /// Where a fresh program's address space starts, inside the block
    /// `[base, ceiling)`. The base by default — one program, one place —
    /// and the interpreter's machine chooses a slot nobody live is using,
    /// so that unrelated programs do not share addresses; see
    /// `crate::resident::SLOTS`.
    fn guest_start(&mut self, base: u64, ceiling: u64) -> u64 {
        let _ = ceiling;
        base
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
    /// Who this process is to the owner table — see [`crate::resident`].
    ///
    /// The address space itself is nowhere in here, and that is the design:
    /// every process's pages stay in linear memory at the guest's addresses,
    /// and the table says whose bytes are at which page. A switch moves only
    /// the pages two processes both map. The arrangement before this one
    /// copied a dormant process's whole address space out to the heap and
    /// back, which cost thirty of a warm request's forty-one milliseconds
    /// for a worker and a proxy that overlapped on four megabytes.
    pub(crate) token: crate::resident::Token,
}

/// Where the guest's address space starts — decided once, at the first
/// process, and the same for every process after it.
///
/// **Every process uses the same range**, which the owner table makes safe:
/// a page holds the bytes of whichever process is running and maps it, and
/// everyone else's copy of that page lives in the heap until they run. A
/// fork already depends on the sharing, since a child needs the parent's
/// addresses.
///
/// Carving a *fresh* region off the top of memory for each one instead —
/// which is what the boot path does, correctly, when the bytes live
/// somewhere of their own — spends half a gigabyte of the module's four per
/// `execve`. It works, and then a container that runs its eighth program
/// gets `ENOEXEC` from a load that had nowhere to go. Which is exactly how
/// this was found: `python`, a captured subprocess, a shell pipeline,
/// `uname`, `ls | wc`, and then nothing.
static GUEST_BASE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn guest_base() -> u64 {
    use core::sync::atomic::Ordering;
    let recorded = GUEST_BASE.load(Ordering::Relaxed);
    if recorded != 0 {
        return recorded;
    }
    // The top of whatever the module already occupies: the linker's data,
    // the shadow stack, and anything the kernel's own allocator has taken
    // before the first process exists. Natively, the top of what the first
    // process committed for its program.
    let base = top_of_memory();
    GUEST_BASE.store(base, Ordering::Relaxed);
    base
}

/// One past the highest address the guest's memory reaches right now.
#[cfg(target_arch = "wasm32")]
fn top_of_memory() -> u64 {
    core::arch::wasm32::memory_size(0) as u64 * 65536
}

#[cfg(not(target_arch = "wasm32"))]
fn top_of_memory() -> u64 {
    targum::arena::committed()
}

/// Grows the guest's memory to reach `to`, and answers whether it did.
#[cfg(target_arch = "wasm32")]
fn grow_memory(to: u64) -> bool {
    const WASM_PAGE: u64 = 65536;
    let current = top_of_memory();
    if to <= current {
        return true;
    }
    // In amortised chunks: growth is a host `mmap` under wasmtime, and one
    // per 4 KiB reservation would be one syscall per page.
    let wanted = to
        .max(current + crate::space::GROW_CHUNK)
        .next_multiple_of(WASM_PAGE);
    let pages = ((wanted - current) / WASM_PAGE) as usize;
    // `memory.grow` answers −1 when it cannot, which is `ENOMEM` at the row
    // above rather than a trap here.
    core::arch::wasm32::memory_grow(0, pages) != usize::MAX
}

#[cfg(not(target_arch = "wasm32"))]
fn grow_memory(to: u64) -> bool {
    targum::arena::commit(to)
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
        // A process that is being built is the process that is running:
        // `exec` writes the program's segments into memory, and natively
        // the program's region has to be committed before it can.
        let token = crate::resident::new_token();
        #[cfg(not(target_arch = "wasm32"))]
        assert!(
            targum::arena::commit(targum::arena::FLOOR + PROGRAM_REGION),
            "reserving the program's region failed"
        );
        Self {
            threads: crate::thread::Threads::new(),
            token,
        }
    }
}

impl Drop for Interpreted {
    /// A machine that is gone is a process that is gone, or one that has
    /// become another program: either way its bytes are nobody's.
    fn drop(&mut self) {
        crate::resident::retire(self.token);
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

    fn park(&mut self, word: u64, bitset: u32, deadline: Option<u64>) -> bool {
        self.threads.current_mut().state = crate::thread::State::Waiting {
            word,
            bitset,
            deadline,
        };
        true
    }

    fn park_on_transfer(&mut self, transfer: crate::ring::Transfer) -> bool {
        self.threads.current_mut().state = crate::thread::State::Transferring(transfer);
        true
    }

    fn park_on_watch(&mut self, watching: crate::thread::Watching) -> bool {
        self.threads.current_mut().state = crate::thread::State::Watching(watching);
        true
    }

    fn park_on_signal(&mut self) -> bool {
        self.threads.current_mut().state = crate::thread::State::Paused;
        true
    }

    fn park_on_accept(&mut self, waiting: crate::thread::Accepting) -> bool {
        self.threads.current_mut().state = crate::thread::State::Accepting(waiting);
        true
    }

    fn park_on_deadline(&mut self, deadline: u64) -> bool {
        self.threads.current_mut().state = crate::thread::State::Sleeping { deadline };
        true
    }

    fn park_on_event(&mut self, waiting: crate::thread::Eventing) -> bool {
        self.threads.current_mut().state = crate::thread::State::Eventing(waiting);
        true
    }

    fn guest_base(&mut self) -> u64 {
        guest_base()
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

    fn claim_pages(&mut self, start: u64, end: u64) {
        crate::resident::claim(self.token, start, end);
    }

    fn guest_start(&mut self, base: u64, ceiling: u64) -> u64 {
        crate::resident::choose_start(base, ceiling)
    }

    fn release_pages(&mut self, start: u64, end: u64) {
        crate::resident::release(self.token, start, end);
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

    fn memory_limit(&self) -> u64 {
        // Memory only ever grows, so this is read per syscall rather than
        // cached: a growth between two syscalls must widen what the second
        // one accepts.
        top_of_memory()
    }

    fn grow(&mut self, to: u64) -> bool {
        grow_memory(to)
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
