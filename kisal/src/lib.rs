//! kisal — the Linux-personality kernel that runs *inside* the guest module.
//!
//! The bet the whole container design rests on is that a `syscall` is
//! rewritten into an ordinary typed wasm call, and that the thing on the
//! other side of that call is just more code linked into the same module.
//! This crate is that code. It has two faces:
//!
//! - **upward**, the Linux syscall ABI, reached through the generated seam
//!   (`x86_syscall` → [`kisal_syscall`]);
//! - **downward**, a single `ll-store` pair under `/iso`, which is the
//!   entire host interface — see [`abi`].
//!
//! Between them there is no host crossing at all. A `read` of a file in the
//! baked image is a copy inside linear memory; only time, entropy, external
//! I/O and the console ever reach the runner. That is the "resources, not
//! syscalls" rule, and it is what makes the filesystem torrent — 80% of a
//! real application's syscalls — free.
//!
//! Everything above the store is ordinary Rust, so it is unit-tested
//! natively in milliseconds. Emulation is reserved for what only emulation
//! can check.

pub mod abi;
pub mod errno;
pub mod fd;
pub mod file;
pub mod image;
pub mod machine;
pub mod memory;
pub mod mmap;
pub mod mount;
pub mod overlay;
pub mod paths;
pub mod random;
pub mod space;
pub mod synthetic;
pub mod syscall;
pub mod vfs;
pub mod write;

use abi::{HostStore, Store};
use machine::GuestMachine;
use syscall::{Arguments, Kernel, Outcome};

/// The kernel, as one process's worth of state.
///
/// Static because a wasm instance *is* a process: one instance, one memory,
/// one kernel. There is no second one to be confused with, and no other
/// thread can be running while this code is — the scheduler switches only at
/// blocking points, and the kernel is never one.
static mut KERNEL: Option<Kernel<'static, HostStore, GuestMachine>> = None;

/// Whether the kernel is currently borrowed.
///
/// The invariant above is an argument, and arguments decay. This makes it a
/// check. It is not idle: serving a syscall can call *back* into the guest —
/// the host places returned bytes through `cabi_realloc` — and the day
/// anything on that path reaches the kernel again, the aliasing would be
/// undefined behaviour with no symptom. One bool per syscall turns that into
/// a panic naming it.
static mut BORROWED: bool = false;

/// Runs `body` with the kernel, for exactly the duration of the call.
///
/// Handing out a `&'static mut` instead — which an earlier version did —
/// makes every borrow unbounded and every re-entrancy question unanswerable.
fn with_kernel<R>(body: impl FnOnce(&mut Kernel<'static, HostStore, GuestMachine>) -> R) -> R {
    // SAFETY: one instance, one thread of execution, and the re-entrancy
    // check below is what makes that a fact rather than a belief.
    unsafe {
        assert!(
            !BORROWED,
            "kisal: the kernel was re-entered while it was already serving a \
             call, which the single-actor invariant says cannot happen"
        );
        BORROWED = true;
        let slot = &mut *(&raw mut KERNEL);
        let kernel = slot.get_or_insert_with(|| {
            let image = image::baked().unwrap_or_else(|error| {
                panic!("kisal: the linked image is not readable: {error:?}")
            });
            Kernel::new(HostStore::default(), GuestMachine, image)
        });
        let result = body(kernel);
        BORROWED = false;
        result
    }
}

/// What the generated seam calls.
///
/// The signature is fixed by Linux and stated in one place on each side, so
/// a disagreement is a link error rather than a mystery at run time.
///
/// # Safety
/// Called only by the generated `x86_syscall`, which has already flushed the
/// guest's machine state into the register globals.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kisal_syscall(
    number: i64,
    a1: i64,
    a2: i64,
    a3: i64,
    a4: i64,
    a5: i64,
    a6: i64,
) -> i64 {
    // Whatever the previous syscall was handed by the host is dead now. The
    // arena's lifetime is one call, which is what stops the boundary leaking.
    abi::reset_transfer_arena();
    let outcome = with_kernel(|kernel| {
        match kernel.dispatch(number, Arguments::new([a1, a2, a3, a4, a5, a6])) {
            Outcome::Done(value) => Ok(value),
            Outcome::Fault(fault) => {
                let mut message = String::new();
                fault.message(&mut message);
                report(kernel, &message);
                Err(message)
            }
            // Neither is reachable until M6 and M7 build the paths that
            // produce them. They are refused loudly rather than silently
            // mishandled, because a wrong answer here would be a corrupted
            // guest.
            Outcome::Blocked => {
                let message = "kisal: a syscall blocked before the scheduler exists";
                report(kernel, message);
                Err(message.to_string())
            }
            Outcome::Exit(_) => {
                let message = "kisal: a syscall exited before the boot path exists";
                report(kernel, message);
                Err(message.to_string())
            }
        }
    });
    // The panic is outside the borrow so that unwinding — or, under
    // `panic = abort`, not unwinding — cannot leave the kernel marked
    // borrowed for a run that is still going.
    match outcome {
        Ok(value) => value,
        Err(message) => panic!("{message}"),
    }
}

/// Sends a kernel complaint to `/iso/log/error`.
///
/// Best-effort by design: if the log mount is unavailable there is nothing
/// useful to do about it, and the panic that follows is the loud part.
fn report(kernel: &mut Kernel<'_, HostStore, GuestMachine>, message: &str) {
    let _ = kernel.store.write(paths::LOG_ERROR, message.as_bytes());
}
