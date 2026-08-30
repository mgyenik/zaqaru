//! Booting the interpreted world, from inside the module.
//!
//! The counterpart of [`crate::kisal_boot`], and the shorter one. That entry
//! resolves the program's entry point to a *table slot* — because the bake
//! translated the program into wasm functions and the only way to reach one
//! is through the table — and then drives a run loop whose only job is to
//! catch what the seam throws. This one sets a program counter.
//!
//! Everything else about the container is the same: the same image, the same
//! kernel, the same two host imports. What changes is the machine underneath.

#![cfg(target_arch = "wasm32")]

use crate::abi::HostStore;
use crate::machine::Interpreted;
use crate::run::{Exit, Process};
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
pub unsafe extern "C" fn targum_boot() -> i32 {
    report_panics();
    let image = crate::image::baked()
        .unwrap_or_else(|error| panic!("kisal: the linked image is not readable: {error:?}"));
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
            panic!("kisal: {message}");
        }
    };
    match process.run() {
        Exit::Status(status) => status,
        // Loud, and named. A container that stopped because the engine does
        // not implement an instruction, or the kernel a syscall, must not
        // look like a program that chose to fail.
        other => {
            let mut message = String::from("kisal: the container stopped: ");
            describe(&other, &mut message);
            crate::report_to(&mut process.kernel, &message);
            UNIMPLEMENTED
        }
    }
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
        let mut message = String::from("kisal: panic");
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
