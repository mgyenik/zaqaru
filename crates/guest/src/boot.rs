//! Booting the container, from inside the module.
//!
//! The one export the host calls. Its first call reads the command line and
//! environment the bake recorded in the image, boots a kernel on the
//! interpreter's machine and loads the program; every call runs the process
//! table for as long as it is asked to, and reports how the run went, and
//! what it cost, through the store.

use cpu::block::BlockCache;
use kernel::abi::{Store, StoreOutcome};
use kernel::machine::Interpreted;
use kernel::run::{Exit, Process};
use kernel::syscall::{Enforcement, Kernel};
use kernel::system::{System, Turn};
use kernel::{paths, push_decimal, push_hex, report_to};

use crate::abi::HostStore;

/// The status a container reports when something the engine or the kernel
/// does not implement stops it.
///
/// Not an ordinary exit code: a guest that chose its own status did so with
/// `exit_group`, and this is the other thing. It is out of the range a
/// program can return so that a caller reading a status can tell them apart.
pub const UNIMPLEMENTED: i32 = 126;

/// The container, once booted. A static because a wasm instance *is* a
/// process: one instance, one memory, one system — and because the host
/// holds it between turns, when nothing is on the wasm stack.
static mut SYSTEM: Option<System<'static, HostStore>> = None;
/// How the container ended, once it has.
static mut FINISHED: Option<i32> = None;

/// The kinds a call to [`zaqaru_run`] answers, in the low byte; a finished
/// container's status is in the byte above.
pub const KIND_RUNNING: i32 = 0;
pub const KIND_IDLE: i32 = 1;
pub const KIND_FINISHED: i32 = 2;

/// Runs the container, and answers what happened.
///
/// The first call boots: it reads the command line and environment the
/// bake recorded, loads the program, and builds the process table. Every
/// call then runs scheduler turns until the container has retired `until`
/// instructions in total, or finished, or — when `until` is not negative —
/// found nothing runnable and waited once on the host. A negative `until`
/// runs to completion, which is what a plain `zaqaru run` asks for.
///
/// Between calls nothing is on the wasm stack, so linear memory is the
/// whole machine: what a snapshot copies and what a debugger inspects. A
/// call with `until` at or below the count already retired runs nothing and
/// returns at once.
///
/// # Safety
/// Called by the host, one call at a time, on an instance nothing else
/// drives.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zaqaru_run(until: i64) -> i32 {
    // SAFETY: one instance, one thread of execution, and the host makes one
    // call at a time.
    if let Some(status) = unsafe { FINISHED } {
        return finished(status);
    }
    let slot = unsafe { &mut *(&raw mut SYSTEM) };
    let system = match slot {
        Some(system) => system,
        None => match boot() {
            Ok(booted) => slot.insert(booted),
            Err(status) => {
                unsafe { FINISHED = Some(status) };
                return finished(status);
            }
        },
    };
    loop {
        if until >= 0 && system.retired() >= until as u64 {
            // Whatever the host asked of the container's store, answered at
            // the instant it is stopped at.
            system.serve();
            return KIND_RUNNING;
        }
        match system.turn() {
            Turn::Ran => {}
            Turn::Idle if until < 0 => {}
            Turn::Idle => {
                system.serve();
                return KIND_IDLE;
            }
            Turn::Finished(outcome) => {
                system.serve();
                // What the run cost, before anything is said about how it
                // ended. A module has no clock of its own worth trusting and
                // no way to time itself, so the host measures the seconds
                // and this supplies the numerator: how much work was done
                // to fill them.
                report_statistics(system);
                let status = match outcome {
                    Exit::Status(status) => status,
                    // Loud, and named. A container that stopped because the
                    // engine does not implement an instruction, or the
                    // kernel a syscall, must not look like a program that
                    // chose to fail.
                    other => {
                        let mut message = String::from("guest: the container stopped: ");
                        describe(&other, &mut message);
                        report_to(&mut system.current().kernel, &message);
                        UNIMPLEMENTED
                    }
                };
                unsafe { FINISHED = Some(status) };
                return finished(status);
            }
        }
    }
}

fn finished(status: i32) -> i32 {
    KIND_FINISHED | (status & 0xff) << 8
}

/// Boots: the image's command line and environment, a kernel on the
/// interpreter's machine, the program loaded, a process table of one.
///
/// A failure is reported through the store and answered as the status the
/// container would have exited with.
fn boot() -> Result<System<'static, HostStore>, i32> {
    report_panics();
    let image = kernel::image::baked()
        .unwrap_or_else(|error| panic!("guest: the linked image is not readable: {error:?}"));
    let recorded: Vec<&[u8]> = image.command_line().collect();
    let argv: &[&[u8]] = match recorded.is_empty() {
        true => &[b"/init"],
        false => &recorded,
    };
    let environment: Vec<&[u8]> = image.environment().collect();

    let kernel = Kernel::with_enforcement(
        HostStore::default(),
        Interpreted::new(),
        image,
        // The interpreter's world: a page is reachable because something
        // mapped it. Which is what makes a null dereference a real
        // `SIGSEGV` rather than a read of whatever is at address zero.
        Enforcement::Mapped,
    );
    // Whether the host asked for the plain interpreter: `/iso/config/bytecode`
    // holding `0`. The default, and the absence of the mount, is the
    // accelerator.
    let cache = match interpret_only(&mut HostStore::default()) {
        true => BlockCache::interpreting(),
        false => BlockCache::new(),
    };
    let process = match Process::boot_with_cache(kernel, argv[0], argv, &environment, cache) {
        Ok(process) => process,
        Err(error) => {
            let mut message = String::new();
            error.message(&mut message);
            panic!("guest: {message}");
        }
    };
    // The process table, which is what makes a `fork` inside the module a
    // second address space rather than an error. One process is the common
    // case and costs a vector of one.
    let mut system = System::new(process);
    // The Block's interface, declared at start-up as the isotope spec asks:
    // the same paths the manifest names, through the store.
    let _ = Store::write(
        &mut system.current().kernel.store,
        paths::SELF_INTERFACE,
        kernel::system::server::INTERFACE.as_bytes(),
    );
    Ok(system)
}

/// The Block's static manifest, per the isotope spec: what the container's
/// store serves, as JSON, before it runs.
///
/// Answers the address of a NUL-terminated string in linear memory. The
/// wasm-level shape of this export is provisional until featherweight's is
/// known; the content is the spec's.
///
/// # Safety
/// Called by the host on an instance; reads nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn manifest() -> i32 {
    // The interface text with a terminator, built once and kept: one
    // instance, one thread of execution.
    static mut TEXT: Option<Vec<u8>> = None;
    let text = unsafe { &mut *(&raw mut TEXT) }.get_or_insert_with(|| {
        let mut bytes = kernel::system::server::INTERFACE.as_bytes().to_vec();
        bytes.push(0);
        bytes
    });
    text.as_ptr() as usize as i32
}

/// Whether the host asked for a run without the bytecode accelerator.
fn interpret_only(store: &mut HostStore) -> bool {
    let mut answer = Vec::new();
    store.read(paths::CONFIG_BYTECODE, &mut answer) == StoreOutcome::Present
        && answer.first().is_some_and(|byte| *byte == b'0')
}

/// Writes what the run cost to `/iso/log/statistics`.
///
/// Two numbers and no rate: instructions retired and blocks decoded, across
/// every process the container had, living or gone — a container that forks
/// does most of its work in children and a count that lost them would
/// understate the engine by however far the guest fanned out. Dividing by a
/// wall time is the host's job, because the host is the only one holding
/// one.
fn report_statistics(system: &mut System<'_, HostStore>) {
    let mut message = String::from("retired ");
    push_decimal(&mut message, system.retired() as i64);
    message.push_str("\naccelerated ");
    push_decimal(&mut message, system.accelerated() as i64);
    message.push_str("\ndecoded ");
    push_decimal(&mut message, system.decoded() as i64);
    message.push('\n');
    let _ = Store::write(
        &mut system.current().kernel.store,
        paths::LOG_STATISTICS,
        message.as_bytes(),
    );
}

/// Sends a panic's message to `/iso/log/error` before the module aborts.
///
/// Inside a module there is no standard error and no console a runtime
/// writes to, so a panic reaches the host as `wasm trap: unreachable` and a
/// backtrace of mangled symbols — the message, which is the only part that
/// says *what went wrong*, is simply gone. Every loud error this project
/// writes is worth nothing under those conditions.
///
/// The store is the way out: it is the container's only channel to the host
/// and it is stateless, so a hook can open one without reaching the kernel
/// that is panicking.
fn report_panics() {
    std::panic::set_hook(Box::new(|panic| {
        let mut message = String::from("guest: panic");
        if let Some(location) = panic.location() {
            message.push_str(" at ");
            message.push_str(location.file());
            message.push(':');
            push_decimal(&mut message, i64::from(location.line()));
        }
        message.push_str(": ");
        // `PanicHookInfo::payload` carries the formatted message for the
        // two shapes a `panic!` produces.
        if let Some(text) = panic.payload().downcast_ref::<&str>() {
            message.push_str(text);
        } else if let Some(text) = panic.payload().downcast_ref::<String>() {
            message.push_str(text);
        } else {
            message.push_str("(a payload with no text)");
        }
        message.push('\n');
        let mut store = HostStore::default();
        let _ = Store::write(&mut store, paths::LOG_ERROR, message.as_bytes());
    }));
}

/// Renders an outcome without a formatter, which a container does not carry.
fn describe(exit: &Exit, into: &mut String) {
    match exit {
        Exit::Status(status) => {
            into.push_str("it exited with ");
            push_decimal(into, i64::from(*status));
        }
        Exit::Signalled {
            signal,
            address,
            rip,
            ..
        } => {
            into.push_str("signal ");
            push_decimal(into, i64::from(*signal));
            into.push_str(" at ");
            push_hex(into, *rip);
            into.push_str(", touching ");
            push_hex(into, *address);
        }
        Exit::Unimplemented(fault) => fault.message(into),
        Exit::Unsupported(unsupported) => {
            into.push_str("the engine does not implement the instruction at ");
            push_hex(into, unsupported.address);
        }
        Exit::Deadlocked => {
            into.push_str("every thread is parked on a futex nothing will wake");
        }
        // Never returned from `run`: the loop consumes it and goes round
        // again. Named rather than caught by a wildcard, so that a variant
        // added later has to be thought about here.
        Exit::Delivered => into.push_str("a signal was delivered"),
    }
}
