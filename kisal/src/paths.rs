//! The `/iso` paths the kernel speaks downward.
//!
//! Every one of them is a *mount point* as far as the kernel is concerned:
//! what a path is backed by is decided by the runner's mount table at boot,
//! never by kisal while serving a syscall. Which is also the capability
//! model — a container with no `/iso/net` mount has no network, decided in
//! configuration rather than in code.
//!
//! Note what is *not* here: none of these are visible in the guest's POSIX
//! namespace. A container calling `stat("/iso")` gets `ENOENT`. The
//! courtyard's service gate is not an address in town.

/// Guest standard input, output and error.
pub const CONSOLE_STDIN: &[&[u8]] = &[b"iso", b"console", b"stdin"];
pub const CONSOLE_STDOUT: &[&[u8]] = &[b"iso", b"console", b"stdout"];
pub const CONSOLE_STDERR: &[&[u8]] = &[b"iso", b"console", b"stderr"];

/// The boot seed for the kernel's random generator. Thirty-two bytes, taken
/// once: everything a container ever reads from `/dev/urandom` or
/// `getrandom` is this expanded, so a host that gives two containers the same
/// seed gives them the same "random" numbers — and one that records it can
/// replay a run exactly.
///
/// A container with no `/iso/random` mount has no entropy, and asking for
/// some is refused by name. That is the capability model: what a container
/// can do is decided in the mount table, in configuration, rather than in
/// code.
pub const RANDOM_SEED: &[&[u8]] = &[b"iso", b"random", b"bytes", b"32"];

/// The clock, in nanoseconds.
///
/// Two of them, because they answer different questions and a program that
/// wants one is wrong to be given the other: `REALTIME` is the wall clock,
/// which can jump backwards when the host's is corrected, and `MONOTONIC`
/// only ever moves forward and means nothing as a date.
///
/// Nanoseconds as a decimal integer rather than the spec's ISO 8601 and
/// counter forms: every caller here is `clock_gettime`, which wants a
/// `timespec`, and going through a formatted date to reach one would mean
/// parsing a calendar in the kernel to produce a number the host already
/// had. This is the ns-typed extension the design proposes to isotope.
///
/// A container with no `/iso/time` mount has no clock, and asking what time
/// it is fails rather than inventing an answer — the same capability
/// decision as [`RANDOM_SEED`], for a stronger reason: a plausible wrong
/// time is a signed certificate that verifies, an expiry that has not
/// passed, and a log that says the wrong thing.
pub const TIME_REALTIME: &[&[u8]] = &[b"iso", b"time", b"realtime_ns"];
pub const TIME_MONOTONIC: &[&[u8]] = &[b"iso", b"time", b"monotonic_ns"];

/// The network edge. A container with nothing mounted here has `lo` and
/// nothing else, which is exactly a Linux network namespace with no
/// interfaces attached — see `docs/network-plan.md` §11, amendment 1.
pub const NET_LISTEN: &[&[u8]] = &[b"iso", b"net", b"listen"];
pub const NET_EVENTS: &[&[u8]] = &[b"iso", b"net", b"events"];

/// Kernel diagnostics. Distinct from guest stderr on purpose: a container's
/// own output and the kernel's complaints about it must never be interleaved
/// into one stream that nobody can separate afterwards.
pub const LOG_ERROR: &[&[u8]] = &[b"iso", b"log", b"error"];

/// Where a syscall trace goes when one is asked for.
///
/// Separate from the error log because it is a different kind of thing: the
/// error log carries what went wrong, and this carries what happened. A run
/// that succeeds writes nothing to the first and everything to the second.
pub const LOG_DEBUG: &[&[u8]] = &[b"iso", b"log", b"debug"];

/// Whether to trace. Read once, at the first syscall.
///
/// A mount rather than a flag, because the kernel has no other channel: the
/// host interface is the store, so a question the host wants to answer is a
/// path the host mounts. A container whose embedder mounts nothing here
/// traces nothing and pays one failed read for the privilege.
pub const CONFIG_TRACE: &[&[u8]] = &[b"iso", b"config", b"trace"];

/// Where the exit status goes when the process is finished.
///
/// A path rather than a return value because the host reaches the container
/// through the store and nothing else. The payload is the status; writing it
/// is the last thing the kernel does, and the run loop returns to the host
/// immediately after.
pub const SHUTDOWN_COMPLETE: &[&[u8]] = &[b"iso", b"shutdown", b"complete"];
