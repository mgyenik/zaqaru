//! The ll-store boundary, in core wasm.
//!
//! The whole host interface is two functions. Everything the kernel cannot
//! answer from its own memory — time, entropy, external I/O, the console,
//! diagnostics — is a path under `/iso` read or written through this pair,
//! and adding a capability later is adding a mount, never adding an import.
//!
//! The shapes here are the canonical ABI's lowering of featherweight's WIT,
//! not a convention of ours:
//!
//! ```wit
//! read:  func(path: list<list<u8>>)                 -> result<option<list<u8>>, string>;
//! write: func(path: list<list<u8>>, data: list<u8>) -> result<list<list<u8>>, string>;
//! ```
//!
//! A `list<T>` lowers to a `(pointer, element count)` pair; results too wide
//! to flatten travel through a caller-supplied return area; and the host
//! allocates the bytes it hands back by calling the guest's own
//! `cabi_realloc`. Following the canonical lowering exactly is what keeps
//! wrapping this core module as a featherweight Block mechanical later, and
//! it costs nothing today.

/// The canonical ABI numbers a variant's cases in declaration order, and
/// `option` is declared `none | some`.
pub const OPTION_NONE: u32 = 0;
pub const OPTION_SOME: u32 = 1;

/// One `list<u8>`: a pointer into linear memory and a length in bytes.
///
/// The layout is the wire format, so the representation is pinned rather
/// than left to the compiler.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Slice {
    pub pointer: u32,
    pub length: u32,
}

impl Slice {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            pointer: bytes.as_ptr() as usize as u32,
            length: bytes.len() as u32,
        }
    }

    /// # Safety
    /// The slice must name live bytes in this module's linear memory — which
    /// is what the host writes there, and what `Slice::of` produces.
    pub unsafe fn as_bytes<'a>(self) -> &'a [u8] {
        if self.length == 0 {
            return &[];
        }
        unsafe {
            core::slice::from_raw_parts(self.pointer as usize as *const u8, self.length as usize)
        }
    }
}

/// `ll_read`'s return area: sixteen bytes, four-byte aligned.
///
/// The two arms of a `result` occupy the *same* twelve bytes, so this is a
/// union with a discriminant in front and is written as one. Naming three
/// fixed fields would silently encode the `ok` arm's shape and hand back
/// nonsense for the other — the `err` arm's string starts where the `ok`
/// arm's inner discriminant is.
///
/// - `[0]` outer discriminant: `0` = ok, `1` = err.
/// - on ok, `[4]` is the `option`'s discriminant (`0` = none, `1` = some,
///   the canonical ABI's declaration order) and `[8..16]` the bytes'
///   `(pointer, length)`.
/// - on err, `[4..12]` is the message's `(pointer, length)`.
///
/// The one-byte discriminants sit in four-byte slots because that is the
/// canonical ABI's padding rule, not because it is tidier.
#[repr(C, align(4))]
#[derive(Clone, Copy, Default)]
pub struct ReadResult {
    pub discriminant: u32,
    pub arm: [u32; 3],
}

impl ReadResult {
    pub fn is_error(&self) -> bool {
        self.discriminant != 0
    }

    /// The bytes, on `ok(some)`. `None` covers both `ok(none)` and `err`, so
    /// callers that care about the difference must ask [`Self::is_error`]
    /// first.
    pub fn value(&self) -> Option<Slice> {
        if self.is_error() || self.arm[0] != OPTION_SOME {
            return None;
        }
        Some(Slice {
            pointer: self.arm[1],
            length: self.arm[2],
        })
    }

    /// The diagnostic string, on `err`. A diagnostic, never an errno: what a
    /// store failure means to the guest is decided by the syscall row that
    /// provoked it.
    pub fn error(&self) -> Option<Slice> {
        self.is_error().then_some(Slice {
            pointer: self.arm[0],
            length: self.arm[1],
        })
    }
}

/// `ll_write`'s return area: twelve bytes, and a union for the same reason.
///
/// - `[0]` discriminant: `0` = ok, `1` = err.
/// - `[4..12]` a `(pointer, length)`: the result path's element array on ok,
///   the message on err.
#[repr(C, align(4))]
#[derive(Clone, Copy, Default)]
pub struct WriteResult {
    pub discriminant: u32,
    pub payload: Slice,
}

impl WriteResult {
    pub fn is_error(&self) -> bool {
        self.discriminant != 0
    }
}

// The two host imports. Undefined here; the runner supplies them.
//
// Named into the `env` module explicitly rather than left undefined for
// `--allow-undefined` to sweep up: a link that turns a *typo* into an import
// is a link that fails at instantiation instead of at build time, and the
// whole point of a typed seam is that disagreements are link errors.
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    #[link_name = "ll_read"]
    fn host_read(path: u32, path_length: u32, result: u32);
    #[link_name = "ll_write"]
    fn host_write(path: u32, path_length: u32, data: u32, data_length: u32, result: u32);
}

/// What a store call answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreOutcome {
    /// `ok(some(bytes))` for a read, `ok(_)` for a write.
    Present,
    /// `ok(none)`: the path exists as an address but holds nothing.
    Absent,
    /// `err(message)`. The message is a diagnostic, never an errno — errno
    /// is decided by the syscall row that provoked the call, because POSIX
    /// lives in the kernel and nowhere else.
    Failed,
}

/// The store, as the kernel sees it: paths in, bytes out, nothing else.
///
/// A trait rather than two free functions so the whole kernel above it is
/// testable natively against an in-memory double — which is where every
/// piece of kernel logic gets falsified first, in milliseconds, before
/// emulation is involved at all.
pub trait Store {
    fn read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome;
    fn write(&mut self, path: &[&[u8]], data: &[u8]) -> StoreOutcome;

    /// The diagnostic from the most recent failure, appended to `into`.
    ///
    /// A store error is a string, and it is the only thing on this boundary
    /// that says *why*. The errno the guest sees is decided by the syscall
    /// row and cannot carry it, so without this the reason is simply lost.
    /// Valid only until the end of the current syscall, which is the arena's
    /// lifetime.
    fn last_error(&self, into: &mut Vec<u8>) {
        let _ = into;
    }
}

/// A store several processes reach.
///
/// A container is a process *tree* and the host boundary is the container's:
/// a child's `write` to the console has to arrive on the same console its
/// parent writes to. The kernel is replicated per process — which is what
/// makes inheritance across a fork correct by construction — and the store
/// is the one thing that must not be.
///
/// Interior mutability rather than a borrow because the processes outlive
/// each other in no particular order, and a lifetime saying otherwise would
/// be saying something untrue.
pub struct Shared<S>(std::rc::Rc<std::cell::RefCell<S>>);

impl<S> Clone for Shared<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S> Shared<S> {
    pub fn new(store: S) -> Self {
        Self(std::rc::Rc::new(std::cell::RefCell::new(store)))
    }

    /// The store itself, for a host reading back what the container wrote.
    pub fn borrow(&self) -> std::cell::Ref<'_, S> {
        self.0.borrow()
    }
}

impl<S: Store> Store for Shared<S> {
    fn read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome {
        self.0.borrow_mut().read(path, into)
    }

    fn write(&mut self, path: &[&[u8]], data: &[u8]) -> StoreOutcome {
        self.0.borrow_mut().write(path, data)
    }

    fn last_error(&self, into: &mut Vec<u8>) {
        self.0.borrow().last_error(into);
    }
}

/// The real store: the two imports, called through the canonical lowering.
///
/// `Clone` because a fork clones it, and there is nothing here to copy: the
/// imports are module-level functions and a child calling them reaches the
/// same host the parent does, which is what a forked process expects of its
/// descriptors.
#[derive(Default, Clone)]
pub struct HostStore {
    /// Read only by the wasm implementation; the native one has no host to
    /// have failed.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    failure: Option<Slice>,
}

#[cfg(target_arch = "wasm32")]
impl Store for HostStore {
    fn read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome {
        let segments = lower_path(path);
        let mut result = ReadResult::default();
        unsafe {
            host_read(
                segments.as_ptr() as usize as u32,
                segments.len() as u32,
                &raw mut result as usize as u32,
            );
        }
        if let Some(message) = result.error() {
            self.failure = Some(message);
            return StoreOutcome::Failed;
        }
        self.failure = None;
        let Some(value) = result.value() else {
            return StoreOutcome::Absent;
        };
        into.extend_from_slice(unsafe { value.as_bytes() });
        StoreOutcome::Present
    }

    fn write(&mut self, path: &[&[u8]], data: &[u8]) -> StoreOutcome {
        let segments = lower_path(path);
        let mut result = WriteResult::default();
        unsafe {
            host_write(
                segments.as_ptr() as usize as u32,
                segments.len() as u32,
                data.as_ptr() as usize as u32,
                data.len() as u32,
                &raw mut result as usize as u32,
            );
        }
        if result.is_error() {
            self.failure = Some(result.payload);
            return StoreOutcome::Failed;
        }
        self.failure = None;
        StoreOutcome::Present
    }

    fn last_error(&self, into: &mut Vec<u8>) {
        if let Some(message) = self.failure {
            // SAFETY: the host placed it in the transfer arena, which lives
            // until the end of this syscall.
            into.extend_from_slice(unsafe { message.as_bytes() });
        }
    }
}

/// Off wasm there is no linear memory to hand the host, and no host. The
/// native build carries the type so that the rest of the kernel compiles
/// unchanged for its unit tests; reaching it would be a bug, and it says so.
#[cfg(not(target_arch = "wasm32"))]
impl Store for HostStore {
    fn read(&mut self, _path: &[&[u8]], _into: &mut Vec<u8>) -> StoreOutcome {
        unreachable!("the host store exists only inside the wasm module")
    }

    fn write(&mut self, _path: &[&[u8]], _data: &[u8]) -> StoreOutcome {
        unreachable!("the host store exists only inside the wasm module")
    }
}

#[cfg(target_arch = "wasm32")]
fn lower_path(path: &[&[u8]]) -> Vec<Slice> {
    path.iter().map(|segment| Slice::of(segment)).collect()
}

/// The arena the host places returned bytes in.
///
/// A general allocator would be wrong here, and not only wastefully: nothing
/// on this boundary is ever freed, because the canonical ABI has the host
/// write into memory the guest owns and gives the host no way to hand it
/// back. Left on a general heap, every `ll_read` would leak its payload
/// permanently into a 32-bit address space — and M3's filesystem traffic is
/// hundreds of reads per process start.
///
/// So the region is a bump arena with a lifetime of exactly one syscall. The
/// kernel resets it on entry and every caller copies what it wanted out
/// before returning, which is what makes the reset safe: nothing the host
/// placed here outlives the call that provoked it.
#[cfg(target_arch = "wasm32")]
mod arena {
    /// Sized for the largest single-syscall transfer M1 can provoke, with
    /// room to grow before M3 needs a real answer. Overrunning it is a loud
    /// failure, never a wrap.
    const CAPACITY: usize = 64 * 1024;

    static mut BYTES: [u8; CAPACITY] = [0; CAPACITY];
    static mut USED: usize = 0;

    /// Drops everything the previous syscall was given.
    pub fn reset() {
        // SAFETY: one instance, one thread of execution, and the scheduler
        // cannot switch inside kernel code.
        unsafe { USED = 0 };
    }

    pub fn allocate(size: usize, align: usize) -> Option<u32> {
        unsafe {
            let base = (&raw mut BYTES) as *mut u8;
            let start = USED.next_multiple_of(align.max(1));
            let end = start.checked_add(size)?;
            if end > CAPACITY {
                return None;
            }
            USED = end;
            Some(base.add(start) as usize as u32)
        }
    }
}

/// Drops whatever the host placed in guest memory for the previous syscall.
/// Called once at the top of each dispatch.
pub fn reset_transfer_arena() {
    #[cfg(target_arch = "wasm32")]
    arena::reset();
}

/// The allocator the host reaches back through when it returns a list.
///
/// This is the canonical ABI's `cabi_realloc`, narrowed to what this boundary
/// actually does: the host only ever allocates fresh, never grows or frees,
/// so a request to grow an existing block is a contract violation rather than
/// something to serve.
///
/// # Safety
/// Called by the host with the canonical ABI's contract.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cabi_realloc(
    old_pointer: u32,
    old_size: u32,
    align: u32,
    new_size: u32,
) -> u32 {
    if new_size == 0 {
        // A zero-length list needs no storage, only a well-aligned address
        // that is never dereferenced.
        return align.max(1);
    }
    assert!(
        old_pointer == 0 && old_size == 0,
        "the host asked to grow a block it was given; this boundary only \
         ever allocates"
    );
    match arena::allocate(new_size as usize, align as usize) {
        Some(pointer) => pointer,
        None => panic!(
            "kisal: the host returned more than the transfer arena holds \
             ({new_size} bytes)"
        ),
    }
}
