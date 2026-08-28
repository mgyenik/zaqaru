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
pub mod exec;
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
            // Not reachable until M7 builds the scheduler: with one
            // thread there is nothing to switch to, so a wait that could
            // not be satisfied would be a hang rather than a block.
            Outcome::Blocked => {
                let message = "kisal: a syscall blocked before the scheduler exists";
                report(kernel, message);
                Err(message.to_string())
            }
            // The process is finished. The kernel cannot throw — an
            // exception unwinds wasm frames without running Rust's drops —
            // so it records the status and hands back the sentinel that
            // tells the seam to throw on its behalf. `boot` is what catches
            // it.
            Outcome::Exit(status) => {
                kernel.status = Some(status);
                Ok(LEAVE)
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

/// What a completed syscall must never return, because it is what the seam
/// reads as "this thread is leaving".
///
/// Stated here as well as in the generator, which is where the seam's own
/// copy lives; `the_leave_sentinel_agrees_across_the_seam` is what keeps the
/// two the same number. The kernel is the only producer of syscall results,
/// so the sentinel is unambiguous as long as no completed call returns it —
/// which is asserted below rather than assumed.
pub const LEAVE: i64 = 0x7a61_7161_7275_0001u64 as i64;

/// Sends a kernel complaint to `/iso/log/error`.
///
/// Best-effort by design: if the log mount is unavailable there is nothing
/// useful to do about it, and the panic that follows is the loud part.
fn report(kernel: &mut Kernel<'_, HostStore, GuestMachine>, message: &str) {
    let _ = kernel.store.write(paths::LOG_ERROR, message.as_bytes());
}

/// The generated seam, as the kernel names it.
///
/// Both are defined inside the same link, so a signature disagreement is a
/// link error. `x86_run_thread` is the scheduler's catch, used here for the
/// same reason it exists there: starting a process and scheduling a thread
/// are the same act, and both need somewhere for the unwind to land.
#[cfg(target_arch = "wasm32")]
unsafe extern "C" {
    #[link_name = "x86_slot_of"]
    fn slot_of(address: i64) -> i32;
    #[link_name = "x86_run_thread"]
    fn run_thread(slot: i32) -> i32;
}

/// Boots the container: loads the program, runs it, and reports how it went.
///
/// The host calls this and nothing else. Everything between is inside the
/// module — the program's bytes come from the image the module carries, the
/// segments are copied within linear memory, and the only thing that crosses
/// the boundary is the exit status, on its way out through
/// `/iso/shutdown/complete`.
///
/// # Safety
/// Called once, by the host, before any guest code has run.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kisal_boot() -> i32 {
    let program = with_kernel(|kernel| {
        // Neither is configurable yet: M6 runs one program with a fixed
        // invocation, and where the arguments come from is the baker's
        // question rather than the kernel's.
        kernel.exec(b"/init", &[b"/init"], &[]).map_err(|error| {
            let mut message = String::new();
            error.message(&mut message);
            report(kernel, &message);
            message
        })
    });
    let entry = match program {
        Ok(entry) => entry,
        Err(message) => panic!("{message}"),
    };

    // SAFETY: the address is a translated function's, because the loader
    // read it out of the same ELF the translator did.
    let left = unsafe { run_thread(slot_of(entry as i64)) };

    with_kernel(|kernel| {
        let status = match (left, kernel.status) {
            // It threw, and the kernel recorded why: the process exited.
            (1, Some(status)) => status,
            // It threw with nothing recorded, which is the scheduler's
            // block arriving before there is a scheduler to catch it.
            (1, None) => {
                let message = "kisal: the guest left without exiting, and there is no \
                               scheduler to have parked it";
                report(kernel, message);
                panic!("{message}");
            }
            // It returned. A program leaves through `exit_group`; running
            // off the end of `_start` means the entry point returned to a
            // caller that does not exist.
            (_, _) => {
                let message = "kisal: the guest returned from its entry point instead of \
                               exiting";
                report(kernel, message);
                panic!("{message}");
            }
        };
        let mut payload = String::new();
        push_status(&mut payload, status);
        let _ = kernel
            .store
            .write(paths::SHUTDOWN_COMPLETE, payload.as_bytes());
        status
    })
}

/// The exit status as the payload carries it: decimal, and nothing else.
#[cfg(target_arch = "wasm32")]
fn push_status(into: &mut String, status: i32) {
    if status == 0 {
        into.push('0');
        return;
    }
    let mut digits = [0u8; 12];
    let mut length = 0;
    let mut value = status.unsigned_abs();
    while value != 0 {
        digits[length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
    }
    if status < 0 {
        into.push('-');
    }
    for index in (0..length).rev() {
        into.push(digits[index] as char);
    }
}
