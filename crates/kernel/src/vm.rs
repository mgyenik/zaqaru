//! Booting the container, from inside the module.
//!
//! The one export the host calls. It reads the command line and environment
//! the bake recorded in the image, boots a kernel on the interpreter's
//! machine, loads the program, and runs the process table until the first
//! process exits — reporting how the run went, and what it cost, through
//! the store.

#![cfg(target_arch = "wasm32")]

use crate::abi::HostStore;
use crate::machine::Interpreted;
use crate::run::{Exit, Process};
use crate::system::System;
use crate::syscall::{Enforcement, Kernel};

/// The status a container reports when something the engine or the kernel
/// does not implement stops it.
///
/// Not an ordinary exit code: a guest that chose its own status did so with
/// `exit_group`, and this is the other thing. It is out of the range a
/// program can return so that a caller reading a status can tell them apart.
pub const UNIMPLEMENTED: i32 = 126;

/// Runs the container to completion, and answers its status.
///
/// # Safety
/// Called by the host, once, on a fresh instance.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zaqaru_boot() -> i32 {
    report_panics();
    let image = crate::image::baked()
        .unwrap_or_else(|error| panic!("kernel: the linked image is not readable: {error:?}"));
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
    let mut process = match Process::boot(kernel, argv[0], argv, &environment) {
        Ok(process) => process,
        Err(error) => {
            let mut message = String::new();
            error.message(&mut message);
            panic!("kernel: {message}");
        }
    };
    // The process table, which is what makes a `fork` inside the module a
    // second address space rather than an error. One process is the common
    // case and costs a vector of one.
    let mut system = System::new(process);
    let outcome = system.run();
    // What the run cost, before anything is said about how it ended. A
    // module has no clock of its own worth trusting and no way to time
    // itself, so the host measures the seconds and this supplies the
    // numerator: how much work was done to fill them.
    report_statistics(&mut system);
    match outcome {
        Exit::Status(status) => status,
        // Loud, and named. A container that stopped because the engine does
        // not implement an instruction, or the kernel a syscall, must not
        // look like a program that chose to fail.
        other => {
            let mut message = String::from("kernel: the container stopped: ");
            describe(&other, &mut message);
            crate::report_to(&mut system.current().kernel, &message);
            UNIMPLEMENTED
        }
    }
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
    crate::push_decimal(&mut message, system.retired() as i64);
    message.push_str("\naccelerated ");
    crate::push_decimal(&mut message, system.accelerated() as i64);
    message.push_str("\ndecoded ");
    crate::push_decimal(&mut message, system.decoded() as i64);
    message.push('\n');
    let _ = crate::abi::Store::write(
        &mut system.current().kernel.store,
        crate::paths::LOG_STATISTICS,
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
        let mut message = String::from("kernel: panic");
        if let Some(location) = panic.location() {
            message.push_str(" at ");
            message.push_str(location.file());
            message.push(':');
            crate::push_decimal(&mut message, i64::from(location.line()));
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
        let _ = crate::abi::Store::write(&mut store, crate::paths::LOG_ERROR, message.as_bytes());
    }));
}

/// Renders an outcome without a formatter, which a container does not carry.
fn describe(exit: &Exit, into: &mut String) {
    match exit {
        Exit::Status(status) => {
            into.push_str("it exited with ");
            crate::push_decimal(into, i64::from(*status));
        }
        Exit::Signalled {
            signal,
            address,
            rip,
            ..
        } => {
            into.push_str("signal ");
            crate::push_decimal(into, i64::from(*signal));
            into.push_str(" at ");
            crate::push_hex(into, *rip);
            into.push_str(", touching ");
            crate::push_hex(into, *address);
        }
        Exit::Unimplemented(fault) => fault.message(into),
        Exit::Unsupported(unsupported) => {
            into.push_str("the engine does not implement the instruction at ");
            crate::push_hex(into, unsupported.address);
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
