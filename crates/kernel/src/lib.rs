//! The Linux-personality kernel that runs *inside* the guest module.
//!
//! The bet the whole container design rests on is that a `syscall` is an
//! ordinary call into more code linked into the same module. This crate is
//! that code. It has two faces:
//!
//! - **upward**, the Linux syscall ABI: the interpreter's run loop
//!   ([`run`]) stops at a `syscall`, hands the kernel six registers, and
//!   writes one back;
//! - **downward**, a single `ll-store` pair under `/iso`, which is the
//!   entire host interface — see [`abi`].
//!
//! Between them there is no host crossing at all. A `read` of a file in the
//! baked image is a copy inside linear memory; only time, entropy, external
//! I/O and the console ever reach the host. That is the "resources, not
//! syscalls" rule, and it is what makes the filesystem torrent — 80% of a
//! real application's syscalls — free.
//!
//! Everything above the store is ordinary Rust, so it is unit-tested
//! natively in milliseconds. Emulation is reserved for what only emulation
//! can check. The module's one entry point is in [`vm`].

pub mod abi;
pub mod errno;
pub mod eventfd;
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
pub mod poll;
pub mod ring;
pub mod socket;
pub mod random;
pub mod resident;
pub mod signal;
pub mod run;
pub mod vm;
pub mod space;
pub mod synthetic;
pub mod syscall;
pub mod system;
pub mod thread;
pub mod vdso;
pub mod vfs;
pub mod write;

use abi::Store;
use syscall::Kernel;

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
/// purpose — the fidelity check is a diff against a real `strace`, and a
/// format that has to be converted first is a format that will disagree in
/// ways nobody can attribute.
pub(crate) fn trace<S: Store, M: machine::Machine>(kernel: &mut Kernel<'_, S, M>, line: &str) {
    // SAFETY: one instance, one thread of execution: the run loop is the
    // only caller, and it is never re-entered.
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
    // The pid, the way `strace -f` prefixes one. Not decoration once a
    // container is a process tree: five processes interleaved with no
    // attribution is a trace nobody can read, and it is also what the
    // eventual diff against a native `strace -f` has to line up against.
    line.push('[');
    push_decimal(&mut line, i64::from(kernel.pid));
    line.push_str("] ");
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

/// Sends a kernel complaint to `/iso/log/error`.
///
/// Best-effort by design: if the log mount is unavailable there is nothing
/// useful to do about it, and the panic that follows is the loud part.
pub(crate) fn report_to<S: Store, M: machine::Machine>(
    kernel: &mut Kernel<'_, S, M>,
    message: &str,
) {
    let _ = kernel.store.write(paths::LOG_ERROR, message.as_bytes());
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
