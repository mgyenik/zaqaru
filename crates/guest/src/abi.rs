//! The ll-store boundary, in core wasm.
//!
//! The whole host interface is two functions. Everything the kernel cannot
//! answer from its own memory is a path under `/iso` read or written through
//! this pair, and adding a capability later is adding a mount, never adding
//! an import.
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

use kernel::abi::{Store, StoreOutcome};

use crate::wire::{OPTION_SOME, ReadResult, Slice, WriteResult};

// The two host imports. Undefined here; the host supplies them.
//
// Named into the `env` module explicitly rather than left undefined for
// `--allow-undefined` to sweep up: a link that turns a *typo* into an import
// is a link that fails at instantiation instead of at build time, and the
// whole point of a typed boundary is that disagreements are link errors.
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    #[link_name = "ll_read"]
    fn host_read(path: u32, path_length: u32, result: u32);
    #[link_name = "ll_write"]
    fn host_write(path: u32, path_length: u32, data: u32, data_length: u32, result: u32);
}

/// The real store: the two imports, called through the canonical lowering.
///
/// `Clone` because a fork clones it, and there is nothing here to copy: the
/// imports are module-level functions and a child calling them reaches the
/// same host the parent does, which is what a forked process expects of its
/// descriptors.
#[derive(Default, Clone)]
pub struct HostStore {
    failure: Option<Slice>,
}

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

    /// Drops whatever the host placed in guest memory for the previous
    /// syscall. Without this a container runs perfectly well until the arena
    /// fills, which for a Python process is about thirty-eight seconds in
    /// and looks like the host refusing a forty-four byte read.
    fn begin_syscall(&mut self) {
        arena::reset();
    }
}

fn lower_path(path: &[&[u8]]) -> Vec<Slice> {
    path.iter().map(|segment| Slice::of(segment)).collect()
}

/// The arena the host places returned bytes in.
///
/// A general allocator would be wrong here, and not only wastefully: nothing
/// on this boundary is ever freed, because the canonical ABI has the host
/// write into memory the guest owns and gives the host no way to hand it
/// back. Left on a general heap, every `ll_read` would leak its payload
/// permanently into a 32-bit address space — and a process start alone is
/// hundreds of reads.
///
/// So the region is a bump arena with a lifetime of exactly one syscall. The
/// kernel resets it on entry and every caller copies what it wanted out
/// before returning, which is what makes the reset safe: nothing the host
/// placed here outlives the call that provoked it.
mod arena {
    /// Sixteen megabytes: more than any single syscall transfers, and
    /// overrunning it is a loud failure, never a wrap.
    const CAPACITY: usize = 16 * 1024 * 1024;

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

/// The allocator the host reaches back through when it returns a list.
///
/// This is the canonical ABI's `cabi_realloc`, narrowed to what this boundary
/// actually does: the host only ever allocates fresh, never grows or frees,
/// so a request to grow an existing block is a contract violation rather than
/// something to serve.
///
/// # Safety
/// Called by the host with the canonical ABI's contract.
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
            "guest: the host returned more than the transfer arena holds \
             ({new_size} bytes)"
        ),
    }
}
