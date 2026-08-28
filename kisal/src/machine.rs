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
}

#[cfg(target_arch = "wasm32")]
impl Machine for GuestMachine {
    fn segment_base(&self) -> i64 {
        unsafe { get_segment_base() }
    }

    fn set_segment_base(&mut self, value: i64) {
        unsafe { set_segment_base(value) }
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
    /// The address space a native test is pretending to have. Unbounded by
    /// default, because a native test hands over real host pointers; a test
    /// that is *about* the bound sets one.
    pub memory_limit: u64,
    /// How far [`Machine::grow`] may raise that limit. A test that exercises
    /// the memory rows sets this to the end of the buffer it owns, so that
    /// every address the kernel hands out is one the test can actually read.
    pub ceiling: u64,
}

impl Default for Registers {
    fn default() -> Self {
        Self {
            segment_base: 0,
            memory_limit: u64::MAX,
            ceiling: u64::MAX,
        }
    }
}

impl Machine for Registers {
    fn segment_base(&self) -> i64 {
        self.segment_base
    }

    fn set_segment_base(&mut self, value: i64) {
        self.segment_base = value;
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
