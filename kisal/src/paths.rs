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

/// Kernel diagnostics. Distinct from guest stderr on purpose: a container's
/// own output and the kernel's complaints about it must never be interleaved
/// into one stream that nobody can separate afterwards.
pub const LOG_ERROR: &[&[u8]] = &[b"iso", b"log", b"error"];
