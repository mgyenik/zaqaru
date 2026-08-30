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
pub mod pipe;
pub mod random;
pub mod signal;
pub mod run;
pub mod vm;
pub mod space;
pub mod synthetic;
pub mod syscall;
pub mod system;
pub mod thread;
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
            Kernel::new(HostStore::default(), GuestMachine::default(), image)
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
    let arguments = [a1, a2, a3, a4, a5, a6];
    let outcome = with_kernel(|kernel| {
        let answer = kernel.dispatch(number, Arguments::new(arguments));
        let rendered = match &answer {
            Outcome::Done(value) => {
                let mut text = String::new();
                push_decimal(&mut text, *value);
                text
            }
            Outcome::Fault(_) => String::from("<fault>"),
            Outcome::Blocked => String::from("<blocked>"),
            Outcome::Process(_) => String::from("<process>"),
            Outcome::Exit(status) => {
                let mut text = String::from("<exit ");
                push_decimal(&mut text, i64::from(*status));
                text.push('>');
                text
            }
        };
        let line = traced(kernel, number, &arguments, &rendered);
        trace(kernel, &line);
        match answer {
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
            // Only the interpreted world has a process table, and only it
            // can answer these.
            Outcome::Process(_) => {
                let message = "kisal: a process operation on the ahead-of-time path";
                report(kernel, message);
                Err(message.to_string())
            }
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

/// Whether this container is tracing its syscalls, decided once.
///
/// `None` until the first syscall asks, because the store is not reachable
/// before the kernel exists and the answer cannot change afterwards.
static mut TRACING: Option<bool> = None;

/// Writes one line of syscall trace, if a trace was asked for.
///
/// Deliberately after the call rather than before it: what a syscall did is
/// the interesting half, and a line that appears only when the call returns
/// also marks the one that did not. The format is the strace shape on
/// purpose — the M6 oracle is a diff against a real `strace`, and a format
/// that has to be translated first is a format that will disagree in ways
/// nobody can attribute.
pub(crate) fn trace<S: Store, M: machine::Machine>(kernel: &mut Kernel<'_, S, M>, line: &str) {
    // SAFETY: one instance, one thread of execution, and this is reached
    // only from inside `with_kernel`, which is what serialises it.
    let tracing = unsafe {
        match TRACING {
            Some(tracing) => tracing,
            None => {
                let mut answer = Vec::new();
                let asked = kernel.store.read(paths::CONFIG_TRACE, &mut answer)
                    == abi::StoreOutcome::Present
                    && answer.first().is_some_and(|byte| *byte != b'0');
                TRACING = Some(asked);
                asked
            }
        }
    };
    if tracing {
        let _ = kernel.store.write(paths::LOG_DEBUG, line.as_bytes());
    }
}

/// Renders one syscall the way `strace` would.
///
/// The path argument is printed as the path, which is the difference
/// between a trace and a hex dump: "openat(AT_FDCWD, "/lib/libm.so.6") = -2"
/// says what happened, and the same line with a pointer in it says only that
/// something did not open. Every diagnosis this instrument was built for
/// needs the string.
pub(crate) fn traced<S: Store, M: machine::Machine>(
    kernel: &mut Kernel<'_, S, M>,
    number: i64,
    arguments: &[i64; 6],
    outcome: &str,
) -> String {
    let mut line = String::new();
    match syscall::number::name(number) {
        Some(name) => line.push_str(name),
        None => {
            line.push_str("syscall_");
            push_decimal(&mut line, number);
        }
    }
    line.push('(');
    let names_a_path = syscall::number::path_argument(number);
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            line.push_str(", ");
        }
        match names_a_path == Some(index) {
            true => push_guest_string(kernel, &mut line, *argument as u64),
            false => push_hex(&mut line, *argument as u64),
        }
    }
    line.push_str(") = ");
    line.push_str(outcome);
    line.push('\n');
    line
}

/// Renders a NUL-terminated string out of guest memory, quoted.
///
/// Bounded, because this is a diagnostic reading whatever a pointer happens
/// to hold: a run of bytes that never terminates is truncated rather than
/// walked to the end of memory.
fn push_guest_string<S: Store, M: machine::Machine>(
    kernel: &mut Kernel<'_, S, M>,
    into: &mut String,
    at: u64,
) {
    /// Longer than any path a container can hold, and short enough that a
    /// wild pointer costs nothing.
    const LIMIT: u64 = 256;
    if at == 0 {
        into.push_str("NULL");
        return;
    }
    let memory = kernel.memory();
    let length = (0..LIMIT)
        .find(|offset| match memory.check(at + offset, 1) {
            // SAFETY: the byte was just bounds-checked.
            Ok(()) => unsafe { memory.slice(at + offset, 1) }
                .map(|bytes| bytes[0] == 0)
                .unwrap_or(true),
            Err(_) => true,
        })
        .unwrap_or(LIMIT);
    into.push('"');
    // SAFETY: every byte up to `length` was bounds-checked above.
    if let Ok(bytes) = unsafe { memory.slice(at, length) } {
        for byte in bytes {
            match byte.is_ascii_graphic() || *byte == b' ' {
                true => into.push(*byte as char),
                false => into.push('?'),
            }
        }
    }
    into.push('"');
}

pub(crate) fn push_decimal(into: &mut String, value: i64) {
    if value < 0 {
        into.push('-');
    }
    let mut digits = [0u8; 20];
    let mut length = 0;
    let mut left = value.unsigned_abs();
    loop {
        digits[length] = b'0' + (left % 10) as u8;
        length += 1;
        left /= 10;
        if left == 0 {
            break;
        }
    }
    for index in (0..length).rev() {
        into.push(digits[index] as char);
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
    report_to(kernel, message);
}

/// The same, for any kernel — the interpreted world's machine is not the
/// ahead-of-time one.
pub(crate) fn report_to<S: Store, M: machine::Machine>(
    kernel: &mut Kernel<'_, S, M>,
    message: &str,
) {
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
    /// Where the resume driver sits in the function table, so that a thread
    /// holding a continuation can be re-entered. A resume-on guest defines
    /// it; the seam's weak answer is `-1`, which is what a container built
    /// without resume reports and what makes a `longjmp` there impossible
    /// rather than wrong.
    #[link_name = "x86_resume_slot"]
    fn resume_slot() -> i32;
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
    // The command line the bake recorded, or the default. It comes from the
    // image rather than from the host because an invocation is a fact about
    // the container: the same module booted twice runs the same program the
    // same way, which is what makes a run reproducible.
    //
    // The environment is still empty. Nothing has needed one — every binary
    // tried so far is already `BIND_NOW`, so even the loader hint the design
    // names as a belt has had nothing to fasten.
    let baked = image::baked()
        .unwrap_or_else(|error| panic!("kisal: the linked image is not readable: {error:?}"));
    let recorded: Vec<&[u8]> = baked.command_line().collect();
    let argv: &[&[u8]] = match recorded.is_empty() {
        true => &[b"/init"],
        false => &recorded,
    };
    // Eager binding, always. `_dl_runtime_resolve` is the function lazy
    // binding calls on the first use of every imported symbol, and it is the
    // hairiest assembly in userspace — it saves the whole vector register
    // file with `xsave`, an instruction family that should never be on any
    // path here. Binding everything at load means it is never called, which
    // is why the three `xsave` refusals every container carries are
    // harmless.
    //
    // **This is the lesser of the two mechanisms `container-plan.md` names,
    // and it has a cost the other does not.** The design's primary is
    // `DF_1_NOW` in each module's `.dynamic`, set by the bake; this variable
    // is its backup. The variable works — ld.so reads it once at start-up
    // and it covers every object loaded — but it is *visible to the guest*,
    // which `.dynamic` is not: a program that reads its own environment sees
    // a variable no native run would have, and M6's acceptance is a diff
    // against a native `strace`.
    //
    // So this is a shortcut with a recorded price rather than an equivalent.
    // What makes it safe meanwhile is that nothing here should bind lazily.
    // It goes *first*, so that a bake which records `LD_BIND_NOW` of its own
    // is the one the guest reads: ld.so takes the last of a repeated name,
    // as `getenv` does.
    let recorded_environment: Vec<&[u8]> = baked.environment().collect();
    let mut environment: Vec<&[u8]> = Vec::with_capacity(recorded_environment.len() + 1);
    environment.push(b"LD_BIND_NOW=1");
    environment.extend_from_slice(&recorded_environment);
    let program = with_kernel(|kernel| {
        kernel.exec(b"/init", argv, &environment).map_err(|error| {
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
    let slot = unsafe { slot_of(entry as i64) };
    if slot < 0 {
        // The seam's weak answer: this container carries no linked program,
        // so there is nothing to enter. A container is built with a program
        // or it is not, and booting one that was not is a bake that skipped
        // the translator.
        let message = "kisal: this container has no linked program to run";
        with_kernel(|kernel| report(kernel, message));
        panic!("{message}");
    }
    // The degenerate run loop: one thread, and the only thing that can make
    // it runnable again is its own `longjmp`.
    //
    // M7 replaces this with a scheduler that has a run queue and more than
    // one thread to put on it; what it will not change is the shape, because
    // the shape is already what a scheduler is — enter a continuation, catch
    // what it throws, decide whether there is anything left to run. Without
    // even this much, the first `longjmp` unwinds straight out of the
    // container, which is what a throw with nothing recorded used to mean
    // and no longer does.
    let mut slot = slot;
    let status = loop {
        let left = unsafe { run_thread(slot) };
        let next = with_kernel(|kernel| (left, kernel.status, kernel.continuation.take()));
        match next {
            // It threw, and the kernel recorded somewhere to go: a
            // `longjmp`. The continuation goes where the resume driver looks
            // for one — the word at `%rsp` — and the driver pops it, which
            // leaves the stack exactly as the frame `setjmp` returned into
            // expects it. Then round again.
            (1, _, Some(continuation)) => {
                        let entered = with_kernel(|kernel| enter_continuation(kernel, continuation));
                if let Err(message) = entered {
                    with_kernel(|kernel| report(kernel, message));
                    panic!("{message}");
                }
                slot = unsafe { resume_slot() };
                if slot < 0 {
                    let message = "kisal: a `longjmp` arrived in a container built without \
                                   checkpoint-resume, where no continuation can exist";
                    with_kernel(|kernel| report(kernel, message));
                    panic!("{message}");
                }
            }
            // It threw, and the kernel recorded why: the process exited.
            (1, Some(status), None) => break status,
            // It threw with nothing recorded, which is the scheduler's
            // block arriving before there is a scheduler to catch it.
            (1, None, None) => {
                let message = "kisal: the guest left without exiting, and there is no \
                               scheduler to have parked it";
                with_kernel(|kernel| report(kernel, message));
                panic!("{message}");
            }
            // It returned. A program leaves through `exit_group`; running
            // off the end of `_start` means the entry point returned to a
            // caller that does not exist.
            (_, _, _) => {
                let message = "kisal: the guest returned from its entry point instead of \
                               exiting";
                with_kernel(|kernel| report(kernel, message));
                panic!("{message}");
            }
        }
    };

    with_kernel(|kernel| {
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

/// What a function the translator could not translate calls instead of
/// running.
///
/// A real binary carries code for processors this one is not: glibc ships
/// AVX-512 string routines beside SSE2 ones and chooses between them from
/// CPUID, which the design curates to a baseline without AVX so that the
/// SSE2 paths are the ones taken. The AVX bodies are still in the binary,
/// and a translation that refused the whole program over them would refuse
/// every real program.
///
/// So they get this instead. Reaching it means the curation was wrong, or
/// that something needs an instruction nobody has written yet — and either
/// way the answer is the function's name rather than a bare trap.
///
/// # Safety
/// Puts a continuation where the resume driver looks for one.
///
/// The driver reads the word at `%rsp` and pops it, because that is what a
/// suspended thread's stack holds — so entering a continuation that came
/// from somewhere else means writing it there and moving `%rsp` down by the
/// eight bytes the driver will give back. After the pop the stack is exactly
/// what the frame being resumed expects, which is the whole reason this is a
/// push rather than a second entry point into the driver.
///
/// **It cannot be read *out* of the stack instead, and the difference is the
/// design.** `longjmp` has already restored `%rsp` to what `setjmp` saw, but
/// the slot there was overwritten by every later call the same frame made at
/// the same depth: in `if (setjmp(env)) return 1; g();` the slot holds
/// `g()`'s call-site continuation, not `setjmp`'s. Entering that resumes as
/// if the pending call had returned — silent wrong control flow. The
/// `jmp_buf`'s saved word is the only surviving record.
#[cfg(target_arch = "wasm32")]
fn enter_continuation<S: abi::Store, M: machine::Machine>(
    kernel: &mut syscall::Kernel<'_, S, M>,
    continuation: i64,
) -> Result<(), &'static str> {
    let below = kernel
        .machine
        .stack_pointer()
        .checked_sub(8)
        .ok_or("kisal: a `longjmp` left the stack pointer with nowhere to put its continuation")?;
    kernel
        .memory()
        .check(below as u64, 8)
        .map_err(|_| "kisal: a `longjmp` restored a stack pointer outside the guest's memory")?;
    // SAFETY: the eight bytes were bounds-checked immediately above.
    unsafe { kernel.memory_mut().write(below as u64, &continuation.to_le_bytes()) }
        .map_err(|_| "kisal: a `longjmp`'s continuation could not be written to the stack")?;
    kernel.machine.set_stack_pointer(below);
    Ok(())
}

/// `longjmp` has arrived: record where the thread is going.
///
/// Called from the exec map's miss path, which is the one place that can
/// tell a continuation from an address — a resume ID carries a tag bit and
/// an address cannot. All this does is write it down; the throw that
/// discards the wasm frames between here and `setjmp`'s frame is the seam's,
/// raised by the caller immediately afterwards, because a wasm exception
/// must never cross a Rust frame.
///
/// It is a kernel row rather than a guest-side store because the thing being
/// set is thread state, and at M7 the thread is the kernel's.
///
/// # Safety
/// Called only from a generated body.
#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kisal_longjmp(continuation: i64) {
    with_kernel(|kernel| {

        kernel.continuation = Some(continuation)
    });
}

/// Called only from a generated body, with a pointer to that function's
/// name in the module's own data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kisal_untranslated(name: *const u8, length: usize) -> ! {
    // SAFETY: the caller is generated code passing a static string the
    // linker placed; the length is that segment's own size.
    let name = unsafe { core::slice::from_raw_parts(name, length) };
    let name = core::str::from_utf8(name).unwrap_or("<an unreadable name>");
    let message = std::format!(
        "kisal: `{name}` was never translated, and something called it — the \
         instruction it needs is not implemented, or the CPUID this container \
         reports let the guest choose a path that does not exist here"
    );
    with_kernel(|kernel| report(kernel, &message));
    panic!("{message}");
}

/// What the exec map calls when an address is not a function's.
///
/// A function pointer in a linked program is a virtual address, and the map
/// turns one into the slot the indirect-call table is indexed by. Reaching
/// here means the guest computed a pointer to something that is not the
/// start of a translated function: a data address mistaken for code, a
/// relocation the loader has not applied yet, or a function the translator
/// never found.
///
/// The address is the whole of what is worth knowing, so it is reported
/// rather than trapped on — a bare `unreachable` in the middle of a binary
/// search says nothing at all.
///
/// # Safety
/// Called only from the generated exec map, on the path where its search
/// found nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kisal_no_function_at(address: i64) -> ! {
    let mut message = String::from("kisal: ");
    push_hex(&mut message, address as u64);
    message.push_str(
        " is not the address of any translated function, and something \
         called through it",
    );
    with_kernel(|kernel| report(kernel, &message));
    panic!("{message}");
}

/// Hexadecimal, because an address is only legible that way.
pub(crate) fn push_hex(into: &mut String, value: u64) {
    into.push_str("0x");
    let mut started = false;
    for shift in (0..16).rev() {
        let digit = ((value >> (shift * 4)) & 0xf) as u8;
        if digit == 0 && !started && shift != 0 {
            continue;
        }
        started = true;
        into.push(char::from(match digit {
            0..=9 => b'0' + digit,
            _ => b'a' + digit - 10,
        }));
    }
}
