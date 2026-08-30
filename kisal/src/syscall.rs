//! The syscall dispatcher: one table, one row per Linux syscall kisal
//! implements, and a loud error for everything else.
//!
//! The table is the project's worklist made executable. A syscall with no
//! row does not quietly become `ENOSYS` — `ENOSYS` is a *conformant answer*
//! for a handful of calls and a lie for all the rest, and a lie here shows up
//! ten minutes later as a hang in something unrelated. So an unimplemented
//! row names itself and stops the run.

use crate::abi::Store;
use crate::errno::Errno;
use crate::machine::Machine;
use crate::memory::GuestMemory;
use crate::paths;

/// The Linux syscall numbers kisal knows about. Named as constants rather
/// than matched as literals so that the loud error can print the name, which
/// is the difference between a worklist and a puzzle.
pub mod number {
    pub const READ: i64 = 0;
    pub const WRITE: i64 = 1;
    pub const OPEN: i64 = 2;
    pub const CLOSE: i64 = 3;
    pub const STAT: i64 = 4;
    pub const FSTAT: i64 = 5;
    pub const LSTAT: i64 = 6;
    pub const LSEEK: i64 = 8;
    pub const PREAD64: i64 = 17;
    pub const ACCESS: i64 = 21;
    pub const DUP: i64 = 32;
    pub const DUP2: i64 = 33;
    pub const MMAP: i64 = 9;
    pub const MPROTECT: i64 = 10;
    pub const MUNMAP: i64 = 11;
    pub const BRK: i64 = 12;
    pub const RT_SIGACTION: i64 = 13;
    pub const RT_SIGPROCMASK: i64 = 14;
    pub const IOCTL: i64 = 16;
    pub const FCNTL: i64 = 72;
    pub const WRITEV: i64 = 20;
    pub const READV: i64 = 19;
    pub const MADVISE: i64 = 28;
    pub const MREMAP: i64 = 25;
    pub const MSYNC: i64 = 26;
    pub const GETPID: i64 = 39;
    pub const UNAME: i64 = 63;
    pub const PRCTL: i64 = 157;
    pub const GETUID: i64 = 102;
    pub const GETGID: i64 = 104;
    pub const GETEUID: i64 = 107;
    pub const GETEGID: i64 = 108;
    pub const GETPPID: i64 = 110;
    pub const TIME: i64 = 201;
    pub const SENDFILE: i64 = 40;
    pub const CLONE: i64 = 56;
    pub const GETCWD: i64 = 79;
    pub const CHDIR: i64 = 80;
    pub const FCHDIR: i64 = 81;
    pub const READLINK: i64 = 89;
    pub const EXIT: i64 = 60;
    pub const ARCH_PRCTL: i64 = 158;
    pub const FUTEX: i64 = 202;
    pub const GETDENTS64: i64 = 217;
    pub const SET_TID_ADDRESS: i64 = 218;
    pub const GETTID: i64 = 186;
    pub const TGKILL: i64 = 234;
    pub const CLOCK_GETTIME: i64 = 228;
    pub const EXIT_GROUP: i64 = 231;
    pub const OPENAT: i64 = 257;
    pub const NEWFSTATAT: i64 = 262;
    pub const READLINKAT: i64 = 267;
    pub const FACCESSAT: i64 = 269;
    pub const DUP3: i64 = 292;
    pub const GETRANDOM: i64 = 318;
    pub const STATX: i64 = 332;
    pub const RSEQ: i64 = 334;
    pub const CLONE3: i64 = 435;
    pub const SET_ROBUST_LIST: i64 = 273;
    pub const PRLIMIT64: i64 = 302;

    pub const PWRITE64: i64 = 18;
    pub const RENAME: i64 = 82;
    pub const MKDIR: i64 = 83;
    pub const RMDIR: i64 = 84;
    pub const LINK: i64 = 86;
    pub const LINKAT: i64 = 265;
    pub const UNLINK: i64 = 87;
    pub const SYMLINK: i64 = 88;
    pub const TRUNCATE: i64 = 76;
    pub const FTRUNCATE: i64 = 77;
    pub const FSYNC: i64 = 74;
    pub const FDATASYNC: i64 = 75;
    pub const FLOCK: i64 = 73;
    pub const UTIMENSAT: i64 = 280;
    pub const MKDIRAT: i64 = 258;
    pub const UNLINKAT: i64 = 263;
    pub const RENAMEAT: i64 = 264;
    pub const RENAMEAT2: i64 = 316;
    pub const SYMLINKAT: i64 = 266;
    pub const CHMOD: i64 = 90;
    pub const FCHMOD: i64 = 91;
    pub const FCHMODAT: i64 = 268;
    /// glibc >= 2.33 routes `eaccess`, `euidaccess` and `faccessat(…,
    /// AT_EACCESS)` through this, so `bash -c 'test -x /bin/ls'` reaches it
    /// on the first line of any entrypoint script. `faccessat` (269) never
    /// took a flags argument in the kernel, which is why 439 exists.
    pub const FACCESSAT2: i64 = 439;

    pub const SETXATTR: i64 = 188;
    pub const LSETXATTR: i64 = 189;
    pub const FSETXATTR: i64 = 190;
    pub const GETXATTR: i64 = 191;
    pub const LGETXATTR: i64 = 192;
    pub const FGETXATTR: i64 = 193;
    pub const LISTXATTR: i64 = 194;
    pub const LLISTXATTR: i64 = 195;
    pub const FLISTXATTR: i64 = 196;
    pub const REMOVEXATTR: i64 = 197;
    pub const LREMOVEXATTR: i64 = 198;
    pub const FREMOVEXATTR: i64 = 199;

    /// Which argument of a syscall is a path, for a trace that says what
    /// happened rather than where a pointer was.
    ///
    /// Only the ones a container's own diagnosis needs. An entry missing
    /// here costs a hex number in a log line; a wrong one would print
    /// whatever some other argument pointed at, so the list is short and
    /// checked rather than long and assumed.
    pub fn path_argument(number: i64) -> Option<usize> {
        Some(match number {
            OPEN | ACCESS | STAT | LSTAT | READLINK | CHDIR | UNLINK | RMDIR | MKDIR
            | TRUNCATE | CHMOD | GETXATTR | LGETXATTR | SETXATTR | LSETXATTR | LISTXATTR
            | LLISTXATTR | REMOVEXATTR | LREMOVEXATTR => 0,
            OPENAT | NEWFSTATAT | STATX | READLINKAT | FACCESSAT | FACCESSAT2 | UNLINKAT
            | MKDIRAT | FCHMODAT | UTIMENSAT => 1,
            _ => return None,
        })
    }

    /// The name of a syscall number, for the loud error. Exhaustive over
    /// what the design doc's traces contain, and honest — an unrecognised
    /// number prints as its number, never as a guess.
    pub fn name(number: i64) -> Option<&'static str> {
        Some(match number {
            READ => "read",
            WRITE => "write",
            OPEN => "open",
            CLOSE => "close",
            STAT => "stat",
            FSTAT => "fstat",
            LSTAT => "lstat",
            LSEEK => "lseek",
            PREAD64 => "pread64",
            ACCESS => "access",
            DUP => "dup",
            DUP2 => "dup2",
            DUP3 => "dup3",
            GETCWD => "getcwd",
            CHDIR => "chdir",
            FCHDIR => "fchdir",
            READLINK => "readlink",
            READLINKAT => "readlinkat",
            READV => "readv",
            FACCESSAT => "faccessat",
            FACCESSAT2 => "faccessat2",
            PWRITE64 => "pwrite64",
            RENAME => "rename",
            MKDIR => "mkdir",
            RMDIR => "rmdir",
            LINK => "link",
            LINKAT => "linkat",
            UNLINK => "unlink",
            SYMLINK => "symlink",
            TRUNCATE => "truncate",
            FTRUNCATE => "ftruncate",
            FSYNC => "fsync",
            FDATASYNC => "fdatasync",
            FLOCK => "flock",
            UTIMENSAT => "utimensat",
            MKDIRAT => "mkdirat",
            UNLINKAT => "unlinkat",
            RENAMEAT => "renameat",
            RENAMEAT2 => "renameat2",
            SYMLINKAT => "symlinkat",
            CHMOD => "chmod",
            FCHMOD => "fchmod",
            FCHMODAT => "fchmodat",
            SETXATTR => "setxattr",
            LSETXATTR => "lsetxattr",
            FSETXATTR => "fsetxattr",
            GETXATTR => "getxattr",
            LGETXATTR => "lgetxattr",
            FGETXATTR => "fgetxattr",
            LISTXATTR => "listxattr",
            LLISTXATTR => "llistxattr",
            FLISTXATTR => "flistxattr",
            REMOVEXATTR => "removexattr",
            LREMOVEXATTR => "lremovexattr",
            FREMOVEXATTR => "fremovexattr",
            MMAP => "mmap",
            MPROTECT => "mprotect",
            MUNMAP => "munmap",
            BRK => "brk",
            RT_SIGACTION => "rt_sigaction",
            RT_SIGPROCMASK => "rt_sigprocmask",
            IOCTL => "ioctl",
            FCNTL => "fcntl",
            WRITEV => "writev",
            MADVISE => "madvise",
            MREMAP => "mremap",
            MSYNC => "msync",
            GETPID => "getpid",
            UNAME => "uname",
            PRCTL => "prctl",
            GETUID => "getuid",
            GETGID => "getgid",
            GETEUID => "geteuid",
            GETEGID => "getegid",
            GETPPID => "getppid",
            TIME => "time",
            SENDFILE => "sendfile",
            CLONE => "clone",
            EXIT => "exit",
            ARCH_PRCTL => "arch_prctl",
            FUTEX => "futex",
            GETDENTS64 => "getdents64",
            SET_TID_ADDRESS => "set_tid_address",
            SET_ROBUST_LIST => "set_robust_list",
            PRLIMIT64 => "prlimit64",
            GETTID => "gettid",
            CLOCK_GETTIME => "clock_gettime",
            EXIT_GROUP => "exit_group",
            TGKILL => "tgkill",
            OPENAT => "openat",
            NEWFSTATAT => "newfstatat",
            GETRANDOM => "getrandom",
            STATX => "statx",
            RSEQ => "rseq",
            CLONE3 => "clone3",
            _ => return None,
        })
    }
}

/// A syscall's six arguments, in the order the `syscall` instruction's
/// registers supply them.
#[derive(Clone, Copy, Debug)]
pub struct Arguments {
    pub values: [i64; 6],
}

impl Arguments {
    pub fn new(values: [i64; 6]) -> Self {
        Self { values }
    }

    pub fn get(&self, index: usize) -> i64 {
        self.values[index]
    }
}

/// How a syscall ended.
///
/// Three of the four variants are unreachable at M1 and exist anyway,
/// because the shape of the protocol is what the generated seam is compiled
/// against: adding a variant later would be an ABI change, and adding a
/// *use* of one is not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The value to put in `rax`, negative errno included.
    Done(i64),
    /// The thread cannot proceed. Its control block is already parked on the
    /// wait object with a completion recipe; the seam turns this into the
    /// throw that discards its redundant wasm frames.
    Blocked,
    /// The process is finished. The seam unwinds to the run loop, which
    /// reports the status through `/iso/shutdown/complete`.
    Exit(i32),
    /// Something the kernel does not implement, named. Never an errno: a
    /// silent `ENOSYS` here is a hang somewhere else later.
    Fault(Fault),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    pub number: i64,
    pub name: Option<&'static str>,
    /// What the call was given. A worklist entry saying only "mmap" sends
    /// someone to read the trace again; one carrying the arguments is the
    /// trace. It is also the only place the seam's register marshalling is
    /// observable from outside — six values in, six values named.
    pub arguments: [i64; 6],
    /// Which *part* of the syscall is missing, when the syscall itself is
    /// implemented and one of its operations is not — an `arch_prctl`
    /// sub-function, later an `ioctl` request or a `clone` flag. Without it
    /// the worklist would say "arch_prctl" for a call that mostly works.
    pub detail: Option<&'static str>,
}

impl Fault {
    pub fn of(number: i64, arguments: Arguments) -> Self {
        Self {
            number,
            name: number::name(number),
            arguments: arguments.values,
            detail: None,
        }
    }

    pub fn detailed(number: i64, arguments: Arguments, detail: &'static str) -> Self {
        Self {
            detail: Some(detail),
            ..Self::of(number, arguments)
        }
    }

    pub fn message(&self, into: &mut String) {
        into.push_str("kisal: unimplemented syscall ");
        match self.name {
            Some(name) => {
                into.push_str(name);
                into.push_str(" (");
                push_decimal(into, self.number);
                into.push(')');
            }
            None => push_decimal(into, self.number),
        }
        if let Some(detail) = self.detail {
            into.push_str(": ");
            into.push_str(detail);
        }
        into.push_str(" with (");
        for (index, argument) in self.arguments.iter().enumerate() {
            if index != 0 {
                into.push_str(", ");
            }
            push_decimal(into, *argument);
        }
        into.push(')');
    }
}

fn push_decimal(into: &mut String, value: i64) {
    let mut buffer = [0u8; 24];
    let mut length = 0;
    let negative = value < 0;
    let mut magnitude = value.unsigned_abs();
    loop {
        buffer[length] = b'0' + (magnitude % 10) as u8;
        length += 1;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        into.push('-');
    }
    for index in (0..length).rev() {
        into.push(buffer[index] as char);
    }
}

/// The kernel's state, and its two downward faces: the store it reaches the
/// world through, and the machine cells it reads and writes one at a time.
/// The fd table, the VFS, the VMA tree and the scheduler all land here in
/// their own milestones.
pub struct Kernel<'a, S: Store, M: Machine> {
    pub store: S,
    pub machine: M,
    /// The kernel's random bytes, seeded once at boot. See
    /// [`crate::random`] for why the seed crosses the boundary exactly once.
    pub random: crate::random::Random,
    /// What `/proc/self/exe` points at. Empty until something sets it —
    /// M6's `execve` is what knows the answer — and reading the link before
    /// then is a named fault rather than a plausible path.
    pub executable: String,
    /// Resolution, and the mount table it walks. One filesystem is attached
    /// until M4's overlay; what a path *means* is decided here either way.
    pub vfs: crate::vfs::Vfs<'a>,
    /// The descriptor table, and the open file descriptions under it.
    pub files: crate::fd::FdTable,
    /// The counter new timestamps come from. See `Kernel::now`.
    pub clock: i64,
    /// The status the process finished with, once something has finished.
    /// The boot path reads it after the unwind, which is the only moment
    /// there is anything to read.
    pub status: Option<i32>,
    /// The guest's address space: the arenas, and the tree of what is
    /// mapped where.
    pub space: crate::space::Space,
    /// Where the kernel would write a zero when this thread ends, from
    /// `set_tid_address`. Recorded until M7 has a thread whose ending
    /// something could be waiting for.
    pub clear_child_tid: u64,
    /// Which signals this thread has blocked, one bit each, signal one at
    /// bit zero. Only self-directed signals can ever be affected by it —
    /// nothing outside a container can send one.
    blocked_signals: u64,
    /// What each signal is set to do, indexed by signal minus one.
    ///
    /// Recorded from M6 and delivered at M10, which is the build plan's own
    /// sequencing and not a deferral invented here: CPython installs its
    /// handlers before it runs a line, so refusing `rt_sigaction` stops the
    /// interpreter at startup, while *running* a handler needs the chain
    /// surgery M10 builds. What is recorded is enough to answer the two
    /// questions a program can ask before then — what is this signal set to,
    /// and what happens when I raise it at myself.
    dispositions: [Disposition; 64],
    /// The head of this thread's robust futex list, from
    /// `set_robust_list`. Recorded with the same horizon.
    pub robust_list: u64,
}

/// Which way a vectored transfer moves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    In,
    Out,
}

/// The container's only process, which is the first in its own namespace.
pub const PROCESS_ID: i64 = 1;

/// How much linear memory the guest's arenas are given.
///
/// Reserved in one block at boot rather than grown as needed, because the
/// kernel's allocator grows from the end of memory too and the two would
/// otherwise interleave — see [`crate::space::Space`]. Wasm memory never
/// shrinks and untouched pages cost address space rather than anything
/// else, so a generous block is close to free and a stingy one is an
/// `ENOMEM` a guest did not deserve.
pub const GUEST_ADDRESS_SPACE: u64 = 512 * 1024 * 1024;

/// The size Linux requires of a `struct robust_list_head`, and refuses any
/// other. A caller passing a different one was built against a different
/// kernel.
const ROBUST_LIST_HEAD_SIZE: u64 = 24;

/// The soft and hard limits for a resource, or `None` where nothing here
/// decides one.
///
/// Only what has a real answer. A limit invented for a resource this
/// container does not model would be a number a guest could size something
/// against, and nothing would keep it.
pub fn resource_limit_for(resource: u32) -> Option<(u64, u64)> {
    /// No limit, which is how Linux spells one that is not enforced.
    const INFINITY: u64 = u64::MAX;
    const RLIMIT_STACK: u32 = 3;
    const RLIMIT_NOFILE: u32 = 7;
    const RLIMIT_AS: u32 = 9;
    match resource {
        // The stack the guest was actually given, taken from the one place
        // that decides it rather than restated here.
        RLIMIT_STACK => Some((crate::exec::STACK_BYTES, INFINITY)),
        // Descriptors: the table is bounded, and this is that bound.
        RLIMIT_NOFILE => Some((
            crate::fd::MAX_DESCRIPTORS as u64,
            crate::fd::MAX_DESCRIPTORS as u64,
        )),
        // Address space: wasm memory grows to four gigabytes and no
        // further, which is a real ceiling rather than a chosen one.
        RLIMIT_AS => Some((u32::MAX as u64, u32::MAX as u64)),
        _ => None,
    }
}

impl<'a, S: Store, M: Machine> Kernel<'a, S, M> {
    /// Boots a kernel on an image.
    ///
    /// Three things happen here that a running kernel then relies on: the
    /// mount table gets its root filesystem, the descriptor table gets the
    /// standard streams, and the random generator gets its seed. Doing the
    /// seed at boot rather than on first use is what makes a run
    /// reproducible from the outside — the host chose it before the guest
    /// ran a single instruction.
    ///
    /// The synthetic mounts are attached here too, over the directories the
    /// image provides for them. An image that has no `/dev` gets no `/dev`,
    /// which is what mounting means: `mount` on a missing mount point fails
    /// on Linux as well, and inventing the directory would be inventing a
    /// filesystem the image did not ask for.
    pub fn new(store: S, mut machine: M, image: crate::image::Image<'a>) -> Self {
        // The guest's arenas are a block reserved once, here, and not
        // whatever happens to lie above the module. They have to be: the
        // kernel's own allocator takes its pages from the end of linear
        // memory, and so would the arenas — two claimants on the same
        // bytes, with the guest's `brk` memory and the kernel's heap
        // silently the same. Growing to the ceiling now puts every later
        // kernel allocation above it, permanently.
        let space_start = machine.memory_limit();
        let space_ceiling = space_start.saturating_add(GUEST_ADDRESS_SPACE);
        let reserved = machine.grow(space_ceiling);
        let mut kernel = Self {
            store,
            machine,
            random: crate::random::Random::unseeded(),
            executable: String::new(),
            vfs: crate::vfs::Vfs::new(image),
            files: crate::fd::FdTable::with_standard_streams(),
            clock: 1,
            status: None,
            clear_child_tid: 0,
            blocked_signals: 0,
            dispositions: [Disposition::DEFAULT; 64],
            robust_list: 0,
            // Carved from the top of whatever the module already occupies:
            // the linker's data, the shadow stack, and anything the
            // kernel's own allocator has taken. Everything the guest is
            // given comes from there up.
            space: if reserved {
                crate::space::Space::within(space_start, space_ceiling)
            } else {
                // The reservation always succeeds inside a module, where
                // memory grows on request. It fails only in a native test,
                // whose address space is a buffer it allocated — and there
                // the bound has nothing to do, because the thing it keeps
                // the guest away from is a kernel allocator that native
                // tests do not share memory with.
                crate::space::Space::new(space_start)
            },
        };
        kernel.seed_random();
        kernel.mount_synthetics();
        kernel
    }

    /// Takes the boot seed, once.
    fn seed_random(&mut self) {
        let mut bytes = Vec::new();
        if self.store.read(paths::RANDOM_SEED, &mut bytes) != crate::abi::StoreOutcome::Present {
            // No `/iso/random` mount, so no entropy. Left unseeded, and
            // every request for randomness is refused by name — never
            // filled with zeros, which is the one answer that is both
            // plausible and catastrophic.
            return;
        }
        let Ok(seed) = <[u8; crate::random::SEED_BYTES]>::try_from(bytes.as_slice()) else {
            // A short seed is a misconfigured host, not a smaller seed.
            return;
        };
        self.random.seed(&seed);
    }

    /// Sets what `/proc/self/exe` points at, and rebuilds `/proc` so that it
    /// does.
    ///
    /// M6's `execve` is what calls this: the path is a fact about the
    /// program that was started, and nothing before that knows one.
    pub fn set_executable(&mut self, path: &str) {
        self.executable = String::from(path);
        self.mount_synthetics();
    }

    /// Attaches `/dev` and `/proc` over the directories the image provides.
    fn mount_synthetics(&mut self) {
        for (path, index) in [
            (&b"/dev"[..], crate::synthetic::dev()),
            (b"/proc", crate::synthetic::proc(self.executable.as_bytes())),
        ] {
            let root = self.vfs.root();
            let Ok(point) = self.vfs.resolve(root, path, crate::vfs::Lookup::FOLLOW) else {
                continue;
            };
            // Resolution crosses *into* a mount, so on a second call this
            // is the synthetic filesystem's own root rather than the
            // directory it covers. Stepping back out is what makes this a
            // replacement instead of a second mount stacked on the first —
            // which is what an earlier version did, silently, until the
            // table filled up and `/proc/self/exe` stopped changing with no
            // error anywhere.
            let point = match self.vfs.mounts().is_mount_root(point) {
                Ok(true) => match self.vfs.mounts().covers(point.mount) {
                    Ok(Some(covered)) => covered,
                    _ => point,
                },
                _ => point,
            };
            // Leaked deliberately: a mounted filesystem lives as long as the
            // process, and the borrow the mount table holds says exactly
            // that. Nothing unmounts these.
            let index: &'static [u8] = Box::leak(index.into_boxed_slice());
            let Ok(image) = crate::image::Image::parse(index, &[]) else {
                continue;
            };
            if let Err(errno) = self.vfs.mounts_mut().replace(point, image) {
                // A mount table that is full is a kernel that cannot do
                // what it was asked, and saying nothing would leave
                // `/proc/self/exe` quietly answering the previous
                // program's path.
                let mut message = String::from("kisal: cannot mount ");
                message.push_str(&String::from_utf8_lossy(path));
                message.push_str(": ");
                message.push_str(errno.name());
                self.report_mount_failure(&message);
            }
        }
    }

    /// A boot-time mount that did not happen, sent to the kernel log.
    fn report_mount_failure(&mut self, message: &str) {
        let _ = self.store.write(paths::LOG_ERROR, message.as_bytes());
    }

    /// The guest's address space as it stands right now. Read per syscall
    /// rather than cached, because memory grows.
    pub(crate) fn memory(&self) -> GuestMemory {
        GuestMemory::with_limit(self.machine.memory_limit())
    }

    pub fn dispatch(&mut self, number: i64, arguments: Arguments) -> Outcome {
        match number {
            number::WRITE => self.write(arguments),
            number::WRITEV => self.vectored(arguments, Direction::Out),
            number::READV => self.vectored(arguments, Direction::In),
            number::ARCH_PRCTL => self.arch_prctl(arguments),

            // The read-only filesystem. Eighty per cent of a real
            // application's traffic, and none of it leaves the module.
            number::OPEN => self.open(arguments),
            number::OPENAT => self.openat(arguments),
            number::CLOSE => self.close(arguments),
            number::READ => self.read(arguments),
            number::PREAD64 => self.pread(arguments),
            number::LSEEK => self.lseek(arguments),
            number::STAT => self.stat(arguments),
            number::LSTAT => self.lstat(arguments),
            number::FSTAT => self.fstat(arguments),
            number::NEWFSTATAT => self.newfstatat(arguments),
            number::STATX => self.statx(arguments),
            number::GETDENTS64 => self.getdents64(arguments),
            number::READLINK => self.readlink(arguments),
            number::READLINKAT => self.readlinkat(arguments),
            number::ACCESS => self.access(arguments),
            number::FACCESSAT => self.faccessat(arguments),
            number::FACCESSAT2 => self.faccessat2(arguments),
            number::GETXATTR | number::LGETXATTR | number::FGETXATTR => {
                self.getxattr(number, arguments)
            }
            number::LISTXATTR | number::LLISTXATTR | number::FLISTXATTR => {
                self.listxattr(number, arguments)
            }
            number::SETXATTR
            | number::LSETXATTR
            | number::FSETXATTR
            | number::REMOVEXATTR
            | number::LREMOVEXATTR
            | number::FREMOVEXATTR => {
                // The image is read-only, and `EROFS` is the errno that says
                // so. `setfattr` and every archiver that restores attributes
                // branches on it.
                Outcome::Done(Errno::ReadOnlyFs.as_result())
            }
            number::IOCTL => self.ioctl(arguments),
            number::BRK => self.brk(arguments),
            number::MMAP => self.mmap(arguments),
            number::MUNMAP => self.munmap(arguments),
            number::MPROTECT => self.mprotect(arguments),
            number::MREMAP => self.mremap(arguments),
            number::MADVISE => self.madvise(arguments),
            number::MSYNC => self.msync(arguments),
            number::PWRITE64 => self.pwrite(arguments),
            number::MKDIR => self.mkdir(arguments),
            number::MKDIRAT => self.mkdirat(arguments),
            number::RMDIR => self.rmdir(arguments),
            number::LINK => self.link(arguments),
            number::LINKAT => self.linkat(arguments),
            number::UNLINK => self.unlink(arguments),
            number::UNLINKAT => self.unlinkat(arguments),
            number::RENAME => self.rename(arguments),
            number::RENAMEAT => self.renameat(arguments),
            number::RENAMEAT2 => self.renameat2(arguments),
            number::SYMLINK => self.symlink(arguments),
            number::SYMLINKAT => self.symlinkat(arguments),
            number::TRUNCATE => self.truncate(arguments),
            number::FTRUNCATE => self.ftruncate(arguments),
            number::UTIMENSAT => self.utimensat(arguments),
            number::CHMOD => self.chmod(arguments),
            number::FCHMOD => self.fchmod(arguments),
            number::FCHMODAT => self.fchmodat(arguments),
            number::FLOCK => self.flock(arguments),
            // Everything is in memory, so there is nothing to flush and the
            // data is already as durable as the container is. Answering
            // success is the truth here, not a stub: a later `read` sees
            // exactly what a `write` put there, which is all `fsync`
            // promises.
            number::FSYNC | number::FDATASYNC => {
                Outcome::Done(match self.files.description(arguments.get(0) as i32) {
                    // An `O_PATH` descriptor is a reference to a file
                    // rather than a handle on it, and there is nothing to
                    // flush through one.
                    Ok(file) if file.flags & crate::file::open_flags::PATH != 0 => {
                        Errno::BadFile.as_result()
                    }
                    Ok(_) => 0,
                    Err(errno) => errno.as_result(),
                })
            }
            number::FCNTL => self.fcntl(arguments),
            number::DUP => self.dup(arguments),
            number::DUP2 => self.dup2(arguments),
            number::DUP3 => self.dup3(arguments),
            number::GETCWD => self.getcwd(arguments),
            number::CHDIR => self.chdir(arguments),
            number::FCHDIR => self.fchdir(arguments),

            // ---- who and what this process is ---------------------------
            //
            // Everything here is fixed. A container's entry process is the
            // first in its own namespace, which is what makes the answer a
            // constant rather than something to derive.
            number::GETPID | number::GETTID => Outcome::Done(PROCESS_ID),
            // Root, and the same root the initial stack already told the
            // program about: `build_stack` puts zero in `AT_UID` and its
            // three companions, and a libc that read one number there and a
            // different one here would be right to be confused. A container
            // has one user and it is the one that started it.
            number::GETUID | number::GETGID | number::GETEUID | number::GETEGID => {
                Outcome::Done(0)
            }
            // The entry process of a container has no parent inside it.
            // Linux answers zero for a process whose parent is outside its
            // namespace, which is exactly this case.
            number::GETPPID => Outcome::Done(0),
            number::UNAME => self.uname(arguments),
            number::PRCTL => self.prctl(arguments),
            number::SET_TID_ADDRESS => self.set_tid_address(arguments),
            number::SET_ROBUST_LIST => self.set_robust_list(arguments),
            number::PRLIMIT64 => self.prlimit64(arguments),
            number::GETRANDOM => self.getrandom(arguments),
            number::CLOCK_GETTIME => self.clock_gettime(arguments),
            number::TIME => self.time(arguments),
            number::SENDFILE => self.sendfile(arguments),
            number::RT_SIGACTION => self.signal_action(arguments),
            number::RT_SIGPROCMASK => self.signal_mask(arguments),
            number::TGKILL => self.tgkill(arguments),
            // Restartable sequences, refused for real. glibc asks once at
            // startup and takes `ENOSYS` for an answer by never using the
            // feature again — which is the whole point of refusing it here
            // rather than pretending: a registration that appeared to
            // succeed would leave the guest expecting the kernel to restart
            // its critical sections, and nothing would.
            number::RSEQ => Outcome::Done(Errno::NoSys.as_result()),

            // Both leave, and for a single-threaded process they leave the
            // same way: `exit` ends the only thread there is, which ends
            // the process. Once there are threads they part company —
            // `exit` will end one and `exit_group` all of them — and the
            // difference belongs there, not in a status code.
            number::EXIT | number::EXIT_GROUP => {
                // Linux takes the low byte, which is what `wait` reports and
                // what a shell prints.
                Outcome::Exit((arguments.get(0) & 0xff) as i32)
            }

            _ => Outcome::Fault(Fault::of(number, arguments)),
        }
    }

    /// `set_tid_address(2)`: where to write a zero when this thread ends.
    ///
    /// The kernel clears that word and futex-wakes anything waiting on it,
    /// which is how `pthread_join` learns a thread is gone. Recorded rather
    /// than acted on: there is one thread, nothing can be waiting for it,
    /// and by the time it ends the process has. M7 is where the recorded
    /// address starts being used.
    ///
    /// It never fails, and it answers with the caller's thread id.
    fn set_tid_address(&mut self, arguments: Arguments) -> Outcome {
        self.clear_child_tid = arguments.get(0) as u64;
        Outcome::Done(PROCESS_ID)
    }

    /// `set_robust_list(2)`: the list of futexes to release if this thread
    /// dies holding them.
    ///
    /// Recorded for the same reason and with the same horizon as
    /// `set_tid_address`. The length is checked because Linux checks it —
    /// a caller passing a structure of the wrong size is a caller built
    /// against a different kernel, and accepting it would leave the list
    /// unreadable later.
    fn set_robust_list(&mut self, arguments: Arguments) -> Outcome {
        if arguments.get(1) as u64 != ROBUST_LIST_HEAD_SIZE {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        self.robust_list = arguments.get(0) as u64;
        Outcome::Done(0)
    }

    /// `prctl(2)`: one row per option, and a loud refusal for the rest.
    ///
    /// `prctl` is not a syscall so much as sixty of them behind one number,
    /// and implementing it as an API would be implementing sixty things
    /// nothing has asked for. So it is demand-driven exactly like the
    /// instruction set: an option a program actually issues gets an honest
    /// answer, and every other one faults *naming the option*, which makes
    /// the fault a worklist entry rather than a dead end.
    ///
    /// Not `EINVAL`, which is what Linux answers for an option it does not
    /// know. Returning it here would be indistinguishable from Linux for a
    /// program that probes — and silently wrong for the far more common one
    /// that asks the kernel to do something and is told, in effect, that it
    /// did not happen. The unknown-syscall policy is the same policy, for
    /// the same reason.
    fn prctl(&mut self, arguments: Arguments) -> Outcome {
        /// `PR_GET_NAME`. What busybox asks, once, to find out what it is.
        const GET_NAME: i64 = 16;
        /// `TASK_COMM_LEN`: fifteen bytes and a terminator, fixed by Linux
        /// and by the buffer every caller passes.
        const COMM_LEN: usize = 16;

        match arguments.get(0) {
            GET_NAME => {
                // Linux sets `comm` from the basename of what was exec'd and
                // truncates it to fit, so that is what this answers. A
                // container that execs `/init` therefore says `init`, which
                // is the truth about this process rather than about the file
                // it was built from — and the way to make it say something
                // else is to exec the program under its own name.
                let name = self.executable.as_bytes();
                let base = name
                    .iter()
                    .rposition(|byte| *byte == b'/')
                    .map_or(name, |slash| &name[slash + 1..]);
                let mut comm = [0u8; COMM_LEN];
                let kept = base.len().min(COMM_LEN - 1);
                comm[..kept].copy_from_slice(&base[..kept]);

                let at = arguments.get(1) as u64;
                let memory = self.memory();
                if memory.check(at, COMM_LEN as u64).is_err() {
                    return Outcome::Done(Errno::Fault.as_result());
                }
                // SAFETY: the buffer was just bounds-checked.
                match unsafe { memory.write(at, &comm) } {
                    Ok(()) => Outcome::Done(0),
                    Err(_) => Outcome::Done(Errno::Fault.as_result()),
                }
            }
            _ => Outcome::Fault(Fault::detailed(
                number::PRCTL,
                arguments,
                "a `prctl` option with no row here; the number is in the \
                 first argument",
            )),
        }
    }

    /// `uname(2)`: what kind of machine this is.
    ///
    /// Fixed strings, and the fixity is the design rather than a shortcut: a
    /// container's kernel is this one, and there is no host fact underneath
    /// for it to report. The release is stated high enough that a libc's
    /// "is this kernel new enough for X" tests answer yes, because every
    /// syscall kisal implements it implements at its modern shape — there
    /// is no old `stat` here to fall back to.
    ///
    /// The structure is six fixed-size fields with no length prefix, so a
    /// short name is a name followed by zeros; writing the whole block as
    /// zeros first is what makes that true rather than leaving whatever the
    /// guest's buffer held.
    fn uname(&mut self, arguments: Arguments) -> Outcome {
        /// Each field of `struct utsname`, as Linux fixes it.
        const FIELD: u64 = 65;
        const FIELDS: [&[u8]; 6] = [
            b"Linux",
            b"container",
            b"6.1.0",
            b"#1 SMP kisal",
            b"x86_64",
            b"(none)",
        ];
        let at = arguments.get(0) as u64;
        let memory = self.memory();
        if memory.check(at, FIELD * FIELDS.len() as u64).is_err() {
            return Outcome::Done(Errno::Fault.as_result());
        }
        // SAFETY: the whole structure was just bounds-checked.
        unsafe {
            if memory.fill(at, FIELD * FIELDS.len() as u64, 0).is_err() {
                return Outcome::Done(Errno::Fault.as_result());
            }
            for (index, field) in FIELDS.iter().enumerate() {
                if memory.write(at + index as u64 * FIELD, field).is_err() {
                    return Outcome::Done(Errno::Fault.as_result());
                }
            }
        }
        Outcome::Done(0)
    }

    /// `prlimit64(2)`: this process's resource limits.
    ///
    /// Only the reading half, and only for the limits that have a real
    /// answer here. The stack's is the one that matters: glibc reads it at
    /// startup to size a thread's stack attribute, and the number it gets
    /// back has to be the stack the guest was actually given — see
    /// `crate::exec::STACK_BYTES`, which is where it comes from rather than
    /// being restated.
    ///
    /// Setting a limit is refused by name. A limit that appeared to change
    /// and did not would be a guest sizing something against a promise
    /// nothing keeps.
    fn prlimit64(&mut self, arguments: Arguments) -> Outcome {
        let process = arguments.get(0);
        let resource = arguments.get(1) as u32;
        let new_limit = arguments.get(2) as u64;
        let old_limit = arguments.get(3) as u64;

        // Zero means this process, and this process is the only one.
        if process != 0 && process != PROCESS_ID {
            return Outcome::Done(Errno::NoProcess.as_result());
        }
        if new_limit != 0 {
            return Outcome::Fault(Fault::detailed(
                number::PRLIMIT64,
                arguments,
                "changing a resource limit",
            ));
        }
        let Some((soft, hard)) = resource_limit_for(resource) else {
            return Outcome::Fault(Fault::detailed(
                number::PRLIMIT64,
                arguments,
                "a resource whose limit nothing here decides",
            ));
        };
        if old_limit == 0 {
            // Reading nothing, which is how a caller checks that a resource
            // exists at all.
            return Outcome::Done(0);
        }
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&soft.to_le_bytes());
        bytes[8..].copy_from_slice(&hard.to_le_bytes());
        // SAFETY: bounds-checked against the guest's memory before writing.
        match unsafe { self.memory().write(old_limit, &bytes) } {
            Ok(()) => Outcome::Done(0),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    /// `getrandom(2)`: bytes from the kernel's own generator.
    ///
    /// From the seeded stream everything else draws on, so a run replays:
    /// see [`crate::random`]. Never blocks and never partially fills — the
    /// generator is always ready, which is what `GRND_NONBLOCK` asks about
    /// and the only reason a real kernel would answer with less than was
    /// asked for.
    fn getrandom(&mut self, arguments: Arguments) -> Outcome {
        const GRND_NONBLOCK: i64 = 0x1;
        const GRND_RANDOM: i64 = 0x2;
        const GRND_INSECURE: i64 = 0x4;

        let buffer = arguments.get(0) as u64;
        let length = arguments.get(1) as u64;
        let flags = arguments.get(2);
        if flags & !(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE) != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        if let Err(errno) = self.memory().check(buffer, length) {
            return Outcome::Done(errno.as_result());
        }
        if length == 0 {
            return Outcome::Done(0);
        }
        // In chunks, so that a guest asking for a megabyte does not put a
        // megabyte on the kernel's own stack — which is a fixed region with
        // a guard right below it.
        let mut chunk = [0u8; 256];
        let mut written = 0u64;
        while written < length {
            let want = chunk.len().min((length - written) as usize);
            if let Err(errno) = self.random.fill(&mut chunk[..want]) {
                // Nothing has been written yet on the first pass, and a
                // generator that stops mid-way cannot happen: it is refused
                // for having no seed at all or not at all.
                return Outcome::Done(errno.as_result());
            }
            // SAFETY: the whole range was bounds-checked above, and this is
            // a part of it.
            if let Err(errno) = unsafe { self.memory().write(buffer + written, &chunk[..want]) } {
                return Outcome::Done(errno.as_result());
            }
            written += want as u64;
        }
        Outcome::Done(length as i64)
    }

    /// `rt_sigaction(2)`: what a signal is set to do.
    ///
    /// Recorded, not delivered, and that split is the build plan's — M6
    /// records dispositions and M10 makes them live. CPython installs its
    /// handlers before it evaluates a line, so a refusal here stops the
    /// interpreter at startup; running one needs the chain surgery M10
    /// builds. What recording buys immediately is that the two questions a
    /// program can ask *before* delivery exists are answered truthfully:
    /// what is this signal currently set to, and what happens if I raise it
    /// at myself — see [`Self::tgkill`], which now consults this rather than
    /// assuming every signal is at its default.
    ///
    /// The structure exchanged is Linux's `struct kernel_sigaction` and not
    /// glibc's `struct sigaction`: the wrapper converts, and this is the
    /// syscall.
    fn signal_action(&mut self, arguments: Arguments) -> Outcome {
        let signal = arguments.get(0);
        let act = arguments.get(1) as u64;
        let old = arguments.get(2) as u64;
        // Linux checks the mask size and refuses anything else, because a
        // caller passing a different one was built against a different
        // kernel.
        if arguments.get(3) != 8 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        if !(1..=64).contains(&signal) || signal == SIGKILL || signal == SIGSTOP {
            // `SIGKILL` and `SIGSTOP` cannot be caught or ignored, and Linux
            // refuses the call rather than dropping them quietly the way
            // `rt_sigprocmask` drops them from a mask.
            return Outcome::Done(Errno::Invalid.as_result());
        }

        // Both pointers are checked before either is used, so a bad one
        // leaves the disposition exactly as it was. Linux installs first and
        // reports `EFAULT` afterwards if the copy-out faults, which is a
        // difference no conforming caller can observe — glibc's wrapper
        // passes addresses of its own locals.
        if act != 0 {
            if let Err(errno) = self.memory().check(act, SIGACTION_SIZE) {
                return Outcome::Done(errno.as_result());
            }
        }
        if old != 0 {
            if let Err(errno) = self.memory().check(old, SIGACTION_SIZE) {
                return Outcome::Done(errno.as_result());
            }
        }

        let index = (signal - 1) as usize;
        if old != 0 {
            let bytes = self.dispositions[index].to_bytes();
            // SAFETY: the range was bounds-checked immediately above.
            if let Err(errno) = unsafe { self.memory().write(old, &bytes) } {
                return Outcome::Done(errno.as_result());
            }
        }
        if act != 0 {
            // SAFETY: `slice` bounds-checks the range itself, and the bytes
            // are copied out of it before anything else can move memory.
            let installed = match unsafe { self.memory().slice(act, SIGACTION_SIZE) } {
                Ok(bytes) => Disposition::from_bytes(bytes),
                Err(errno) => return Outcome::Done(errno.as_result()),
            };
            self.dispositions[index] = installed;
        }
        Outcome::Done(0)
    }

    /// `rt_sigprocmask(2)`: which signals this thread has blocked.
    ///
    /// Recorded and honoured, but only for signals the process sends to
    /// itself, because those are the only signals there are: nothing outside
    /// a container can send one, and the kernel raises none of its own. So
    /// the mask is not a pretence — it decides exactly what it is asked to
    /// decide, which is what happens when the guest raises a signal on
    /// itself while it is blocked.
    ///
    /// `abort` is the caller that matters. It unblocks `SIGABRT` and then
    /// sends it, precisely so that a handler cannot stop it, and a mask that
    /// was ignored would let a program that had blocked `SIGABRT` keep
    /// running past its own `assert`.
    fn signal_mask(&mut self, arguments: Arguments) -> Outcome {
        const SIG_BLOCK: i64 = 0;
        const SIG_UNBLOCK: i64 = 1;
        const SIG_SETMASK: i64 = 2;

        let how = arguments.get(0);
        let set = arguments.get(1) as u64;
        let old = arguments.get(2) as u64;
        // Linux checks the size and refuses anything else, because a caller
        // passing a different one was built against a different kernel.
        if arguments.get(3) != 8 {
            return Outcome::Done(Errno::Invalid.as_result());
        }

        if old != 0 {
            if let Err(errno) = self.memory().check(old, 8) {
                return Outcome::Done(errno.as_result());
            }
            // SAFETY: the eight bytes were bounds-checked immediately above.
            if let Err(errno) = unsafe {
                self.memory()
                    .write(old, &self.blocked_signals.to_le_bytes())
            } {
                return Outcome::Done(errno.as_result());
            }
        }
        // A null set is a caller that only wanted to read the mask.
        if set == 0 {
            return Outcome::Done(0);
        }
        // SAFETY: `slice` bounds-checks the range itself, and the bytes are
        // copied out before anything else can move memory.
        let mask = match unsafe { self.memory().slice(set, 8) } {
            Ok(bytes) => u64::from_le_bytes(bytes.try_into().expect("eight bytes")),
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        self.blocked_signals = match how {
            SIG_BLOCK => self.blocked_signals | mask,
            SIG_UNBLOCK => self.blocked_signals & !mask,
            SIG_SETMASK => mask,
            _ => return Outcome::Done(Errno::Invalid.as_result()),
        };
        // `SIGKILL` and `SIGSTOP` cannot be blocked, and Linux drops them
        // from the mask silently rather than refusing the call.
        self.blocked_signals &= !(signal_bit(SIGKILL) | signal_bit(SIGSTOP));
        Outcome::Done(0)
    }

    /// `tgkill(2)`: send a signal to a thread, which here is always this one.
    ///
    /// There is one thread and no way for a signal to arrive from outside, so
    /// the only question this has to answer is what happens when a process
    /// signals itself — and for the signals whose default action is to
    /// terminate, the answer is that it dies. `abort` is why: it raises
    /// `SIGABRT` at itself and expects not to come back.
    ///
    /// A blocked signal stays pending forever instead, which is the same
    /// thing that happens on Linux when nothing ever unblocks it.
    ///
    /// What the disposition decides, now that `rt_sigaction` records one:
    /// `SIG_IGN` is ignored, `SIG_DFL` does the default action, and a real
    /// handler is a **named fault**. Running the handler is M10's chain
    /// surgery; terminating instead would be a plausible wrong answer to a
    /// program that installed one precisely so that it would not die, and
    /// silently skipping it would be worse.
    fn tgkill(&mut self, arguments: Arguments) -> Outcome {
        let group = arguments.get(0);
        let thread = arguments.get(1);
        let signal = arguments.get(2);
        if group != PROCESS_ID || thread != PROCESS_ID {
            // The only thread there is. Anything else names a thread that
            // does not exist.
            return Outcome::Done(Errno::NoProcess.as_result());
        }
        if !(1..=64).contains(&signal) {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        if self.blocked_signals & signal_bit(signal) != 0 {
            // Pending, and nothing will ever deliver it.
            return Outcome::Done(0);
        }
        match self.dispositions[(signal - 1) as usize].handler {
            SIG_IGN => return Outcome::Done(0),
            SIG_DFL => {}
            _ => {
                return Outcome::Fault(Fault::detailed(
                    number::TGKILL,
                    arguments,
                    "raising a signal that has a handler installed — delivery \
                     is M10's chain surgery, and the disposition is recorded",
                ));
            }
        }
        if terminates(signal) {
            // What `wait` reports for a process killed by a signal: the
            // number in the low seven bits. A shell prints 128 plus it.
            return Outcome::Exit(128 + signal as i32);
        }
        // The default action is to ignore it, and there is no handler to run.
        Outcome::Done(0)
    }

    /// `clock_gettime(2)`: what time it is, in the clock the caller names.
    ///
    /// The host has the clock and the kernel does not, so this is a store
    /// read on every call rather than a counter kept here. That is the
    /// honest shape: a container's sense of time is something its host
    /// grants, and a container whose host mounted no `/iso/time` has none.
    ///
    /// The vDSO is why this row exists at all. A native process never issues
    /// this syscall — glibc reads the clock out of the vDSO page in
    /// userspace, which is why it appears in no `strace`. There is no vDSO
    /// here and `AT_SYSINFO_EHDR` is absent from the auxv we build, so glibc
    /// takes the syscall path it keeps for exactly that case.
    fn clock_gettime(&mut self, arguments: Arguments) -> Outcome {
        // The clocks Linux numbers. The coarse variants are the same clocks
        // read from a cached page, which is a resolution promise rather than
        // a different time, and `BOOTTIME` differs from `MONOTONIC` only
        // across a suspend this container cannot observe.
        const REALTIME: i64 = 0;
        const MONOTONIC: i64 = 1;
        const MONOTONIC_RAW: i64 = 4;
        const REALTIME_COARSE: i64 = 5;
        const MONOTONIC_COARSE: i64 = 6;
        const BOOTTIME: i64 = 7;

        let path = match arguments.get(0) {
            REALTIME | REALTIME_COARSE => crate::paths::TIME_REALTIME,
            MONOTONIC | MONOTONIC_RAW | MONOTONIC_COARSE | BOOTTIME => crate::paths::TIME_MONOTONIC,
            // Per-process and per-thread CPU time, which this kernel does
            // not account for, and anything else. Linux answers `EINVAL`
            // for a clock it does not have, and so does this.
            _ => return Outcome::Done(Errno::Invalid.as_result()),
        };

        let destination = arguments.get(1) as u64;
        // Two eight-byte fields, checked before anything is read: a caller
        // that passed a bad pointer gets `EFAULT` whatever the clock says.
        if let Err(errno) = self.memory().check(destination, 16) {
            return Outcome::Done(errno.as_result());
        }

        let mut bytes = Vec::new();
        if self.store.read(path, &mut bytes) != crate::abi::StoreOutcome::Present {
            // No clock mounted. Refused by name rather than answered with
            // zero, which would be a time — the epoch — and would be
            // believed.
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let Some(nanoseconds) = parse_nanoseconds(&bytes) else {
            return Outcome::Done(Errno::Invalid.as_result());
        };

        // `timespec` is a whole-seconds field and a remainder that is always
        // positive, which is division rounding towards negative infinity
        // rather than towards zero. Only the wall clock can be negative, and
        // only before 1970, but the arithmetic is written once and correctly
        // rather than for the times we expect.
        let seconds = nanoseconds.div_euclid(1_000_000_000);
        let remainder = nanoseconds.rem_euclid(1_000_000_000);
        let mut image = [0u8; 16];
        image[..8].copy_from_slice(&seconds.to_le_bytes());
        image[8..].copy_from_slice(&remainder.to_le_bytes());
        // SAFETY: the sixteen bytes were bounds-checked above.
        if let Err(errno) = unsafe { self.memory().write(destination, &image) } {
            return Outcome::Done(errno.as_result());
        }
        Outcome::Done(0)
    }

    /// `sendfile(2)`: bytes from one descriptor to another without the
    /// guest holding them.
    ///
    /// Implemented rather than refused, and the distinction is worth stating
    /// because `ENOSYS` would have "worked": busybox tries `sendfile` first
    /// and falls back to `read`/`write` when a kernel does not have it, so
    /// the ladder would have climbed either way. But this is a call that
    /// moves data, and a kernel that answers "I do not have that" to
    /// something it could do is a kernel that will be believed by the next
    /// program without a fallback. `rseq` is refused for real because there
    /// is nothing behind it; this has something behind it.
    ///
    /// The copy goes through the kernel, which is not a compromise here —
    /// there is no page cache to splice and no zero-copy to forfeit, and
    /// the source is already a slice of the image.
    ///
    /// Linux requires the *input* to be mmap-able, which in practice means a
    /// regular file, and refuses a pipe or socket with `EINVAL`. The output
    /// has no such rule. A short answer is allowed and expected: the return
    /// value is what moved, and a caller loops.
    fn sendfile(&mut self, arguments: Arguments) -> Outcome {
        let out = arguments.get(0) as i32;
        let input = arguments.get(1) as i32;
        let position = arguments.get(2) as u64;
        let count = arguments.get(3);
        if count < 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }

        let source = match self.files.description(input) {
            Ok(file) => *file,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        if source.flags & crate::file::open_flags::ACCESS_MODE
            == crate::file::open_flags::WRITE_ONLY
        {
            return Outcome::Done(Errno::BadFile.as_result());
        }
        let crate::fd::Backing::Image(vnode) = source.backing else {
            // A console stream is not a file `sendfile` can read from,
            // which is what Linux says about a pipe.
            return Outcome::Done(Errno::Invalid.as_result());
        };

        // An explicit offset is read from the guest, and is where the
        // transfer starts *without* moving the description's own position —
        // the same distinction `pread` exists for. The pointer is written
        // back afterwards, which is the only way a caller learns how far it
        // got when it passed one.
        let start = match position {
            0 => source.offset,
            at => {
                if let Err(errno) = self.memory().check(at, 8) {
                    return Outcome::Done(errno.as_result());
                }
                // SAFETY: the eight bytes were just bounds-checked.
                let bytes = match unsafe { self.memory().slice(at, 8) } {
                    Ok(bytes) => bytes,
                    Err(errno) => return Outcome::Done(errno.as_result()),
                };
                let offset = i64::from_le_bytes(bytes.try_into().expect("eight bytes"));
                if offset < 0 {
                    return Outcome::Done(Errno::Invalid.as_result());
                }
                offset as u64
            }
        };

        let moving = {
            let inode = match self.vfs.inode(vnode) {
                Ok(inode) => inode,
                Err(errno) => return Outcome::Done(errno.as_result()),
            };
            if !inode.is_regular() {
                return Outcome::Done(Errno::Invalid.as_result());
            }
            let contents = match self
                .vfs
                .filesystem_of(vnode)
                .and_then(|filesystem| filesystem.contents(&inode, vnode.inode))
            {
                Ok(contents) => contents,
                Err(errno) => return Outcome::Done(errno.as_result()),
            };
            let from = start.min(contents.len() as u64) as usize;
            let to = (start.saturating_add(count as u64)).min(contents.len() as u64) as usize;
            // Copied because the write borrows the same tree mutably. There
            // is nothing to be saved by not copying: the bytes are going
            // into another file or out through the store either way.
            contents[from..to].to_vec()
        };

        let moved = match self.send_bytes(out, &moving) {
            Ok(moved) => moved,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };

        match position {
            // No pointer: the description's own position advances, exactly
            // as a `read` of the same bytes would have left it.
            0 => {
                if let Ok(description) = self.files.description_mut(input) {
                    description.offset = start + moved;
                }
            }
            at => {
                // SAFETY: the eight bytes were bounds-checked above.
                if let Err(errno) =
                    unsafe { self.memory().write(at, &(start + moved).to_le_bytes()) }
                {
                    return Outcome::Done(errno.as_result());
                }
            }
        }
        Outcome::Done(moved as i64)
    }

    /// `time(2)`: the wall clock, in whole seconds.
    ///
    /// The oldest shape of the same question `clock_gettime` answers, and it
    /// reads the same clock through the same path — a container whose two
    /// time syscalls disagreed would be worse than one that answered
    /// neither. The seconds field is floored rather than truncated for the
    /// reason given there: only the wall clock can be negative, and the
    /// arithmetic is written correctly once.
    ///
    /// The result is both returned *and* stored, when a pointer is given.
    /// Linux does both, and a caller is entitled to read either.
    fn time(&mut self, arguments: Arguments) -> Outcome {
        let mut bytes = Vec::new();
        if self.store.read(crate::paths::TIME_REALTIME, &mut bytes)
            != crate::abi::StoreOutcome::Present
        {
            // No clock mounted. Refused by name rather than answered with
            // zero, which would be a time — the epoch — and would be
            // believed.
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let Some(nanoseconds) = parse_nanoseconds(&bytes) else {
            return Outcome::Done(Errno::Invalid.as_result());
        };
        let seconds = nanoseconds.div_euclid(1_000_000_000);

        let destination = arguments.get(0) as u64;
        if destination != 0 {
            if let Err(errno) = self.memory().check(destination, 8) {
                return Outcome::Done(errno.as_result());
            }
            // SAFETY: the eight bytes were just bounds-checked.
            if let Err(errno) =
                unsafe { self.memory().write(destination, &seconds.to_le_bytes()) }
            {
                return Outcome::Done(errno.as_result());
            }
        }
        Outcome::Done(seconds)
    }

    /// `arch_prctl(2)`: the only way the thread pointer moves.
    ///
    /// Every writer of `%fs` arrives at a flush boundary by construction —
    /// this one because a syscall is a call, a new thread's because the base
    /// lands in its control block before it runs, and a context switch's
    /// because it happens between threads. So the register can be promoted
    /// into a local inside function bodies with no new discipline: the
    /// translated call reloads it afterwards, and nothing else can have
    /// changed it.
    fn arch_prctl(&mut self, arguments: Arguments) -> Outcome {
        const ARCH_SET_GS: i64 = 0x1001;
        const ARCH_SET_FS: i64 = 0x1002;
        const ARCH_GET_FS: i64 = 0x1003;
        const ARCH_GET_GS: i64 = 0x1004;
        const ARCH_GET_CPUID: i64 = 0x1011;
        const ARCH_SET_CPUID: i64 = 0x1012;

        match arguments.get(0) {
            ARCH_SET_FS => {
                self.machine.set_segment_base(arguments.get(1));
                Outcome::Done(0)
            }
            ARCH_GET_FS => {
                let value = self.machine.segment_base() as u64;
                // SAFETY: the store is bounds-checked against the guest's
                // current memory size, and refuses rather than dereferences
                // anything outside it.
                match unsafe { self.memory().store_u64(arguments.get(1) as u64, value) } {
                    Ok(()) => Outcome::Done(0),
                    Err(errno) => Outcome::Done(errno.as_result()),
                }
            }
            // `%gs` is a loud error in the translator too. A libc that
            // reaches for it is a libc this has never been tested against,
            // and answering plausibly would hide that.
            ARCH_SET_GS | ARCH_GET_GS => Outcome::Fault(Fault::detailed(
                number::ARCH_PRCTL,
                arguments,
                "the `%gs` base, which nothing on this path uses",
            )),
            // CPUID is a control knob the design cares about — the dynamic
            // tier curates its answer so ifunc resolvers pick the SSE2 paths
            // the translation covers — so a guest asking to turn its faulting
            // behaviour on or off is exactly the thing that must not get a
            // plausible answer.
            ARCH_GET_CPUID | ARCH_SET_CPUID => Outcome::Fault(Fault::detailed(
                number::ARCH_PRCTL,
                arguments,
                "CPUID faulting, which the translation curates rather than emulates",
            )),
            // Everything else really is unknown to Linux too, and `EINVAL` is
            // what Linux answers. A deliberate row, not a shrug.
            _ => Outcome::Done(Errno::Invalid.as_result()),
        }
    }

    /// `write(2)`, resolved through the descriptor table like every other row.
    ///
    /// It used to match the literal numbers 1 and 2, which made the
    /// descriptor space two disjoint things: `dup2(file, 1)` succeeded and
    /// then `write(1, …)` went to the console anyway, while `read(1, …)` read
    /// the file. Shell redirection is exactly that idiom, and it failed
    /// silently — the write reported success and the bytes went somewhere
    /// else.
    fn write(&mut self, arguments: Arguments) -> Outcome {
        let descriptor = match i32::try_from(arguments.get(0)) {
            Ok(descriptor) => descriptor,
            Err(_) => return Outcome::Done(Errno::BadFile.as_result()),
        };
        let buffer = arguments.get(1) as u64;
        let count = arguments.get(2);

        let file = match self.files.description(descriptor) {
            Ok(file) => *file,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        if count < 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let path = match file.backing {
            crate::fd::Backing::Console(crate::fd::Console::Output) => paths::CONSOLE_STDOUT,
            crate::fd::Backing::Console(crate::fd::Console::Error) => paths::CONSOLE_STDERR,
            // Standard input is opened read-only: writing to a descriptor
            // whose access mode forbids it is `EBADF`, which is what Linux
            // answers and what distinguishes it from `EROFS` on an *open*
            // that asked for write access.
            crate::fd::Backing::Console(crate::fd::Console::Input) => {
                return Outcome::Done(Errno::BadFile.as_result());
            }
            crate::fd::Backing::Image(vnode) => {
                return Outcome::Done(
                    match self.write_file(
                        vnode,
                        file.flags,
                        file.offset,
                        buffer as i64,
                        count as u64,
                    ) {
                        Ok(written) => {
                            // The description's own position advances, and
                            // `O_APPEND` means it lands at the end whatever
                            // it was.
                            if let Ok(description) = self.files.description_mut(descriptor) {
                                let base = if file.flags & crate::file::open_flags::APPEND != 0 {
                                    description.offset.max(file.offset)
                                } else {
                                    file.offset
                                };
                                description.offset = base + written;
                            }
                            written as i64
                        }
                        Err(errno) => errno.as_result(),
                    },
                );
            }
        };
        // SAFETY: bounds-checked against the guest's current memory size. A
        // zero-length write touches nothing and so may name a null buffer,
        // which callers rely on.
        let bytes = match unsafe { self.memory().slice(buffer, count as u64) } {
            Ok(bytes) => bytes,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        match self.store.write(path, bytes) {
            crate::abi::StoreOutcome::Failed => {
                self.report_store_failure(path);
                Outcome::Done(Errno::Io.as_result())
            }
            _ => Outcome::Done(count),
        }
    }

    /// `readv`/`writev`: one call, several buffers.
    ///
    /// Walked one vector at a time through the ordinary rows, which is
    /// exactly what they mean — the gather and scatter are about saving
    /// syscalls, not about doing anything a sequence of them could not.
    /// Linux additionally makes the whole thing atomic against other writers
    /// on the same description; with one thread there is no other writer,
    /// and M7 is where that stops being true for free.
    ///
    /// The return value is the total moved. A vector that fails after
    /// something has already moved reports what moved rather than the
    /// error, because that is what a caller resumes from — the error comes
    /// back on the next call, when nothing has moved yet.
    fn vectored(&mut self, arguments: Arguments, direction: Direction) -> Outcome {
        /// Bytes per `struct iovec`: a pointer and a length.
        const VECTOR: u64 = 16;
        /// `IOV_MAX`, which Linux refuses to exceed.
        const MAX_VECTORS: i64 = 1024;

        let descriptor = arguments.get(0);
        let vectors = arguments.get(1) as u64;
        let count = arguments.get(2);
        if !(0..=MAX_VECTORS).contains(&count) {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        // SAFETY: the whole array is bounds-checked before any of it is
        // read, so a count that runs off the end fails before anything
        // moves rather than part-way through.
        let array = match unsafe { self.memory().slice(vectors, count as u64 * VECTOR) } {
            Ok(array) => array,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let mut described = [(0u64, 0u64); MAX_VECTORS as usize];
        for index in 0..count as usize {
            let at = index * VECTOR as usize;
            let mut base = [0u8; 8];
            let mut length = [0u8; 8];
            base.copy_from_slice(&array[at..at + 8]);
            length.copy_from_slice(&array[at + 8..at + 16]);
            described[index] = (u64::from_le_bytes(base), u64::from_le_bytes(length));
        }

        let mut total: i64 = 0;
        for &(base, length) in &described[..count as usize] {
            if length == 0 {
                continue;
            }
            if i64::try_from(length).is_err() {
                return Outcome::Done(Errno::Invalid.as_result());
            }
            let one = Arguments::new([descriptor, base as i64, length as i64, 0, 0, 0]);
            let moved = match direction {
                Direction::Out => self.write(one),
                Direction::In => self.read(one),
            };
            match moved {
                Outcome::Done(moved) if moved < 0 => {
                    // Nothing has moved yet, so the error is the answer.
                    return if total == 0 {
                        Outcome::Done(moved)
                    } else {
                        Outcome::Done(total)
                    };
                }
                Outcome::Done(moved) => {
                    total += moved;
                    // A short move ends the call: the next vector would
                    // otherwise skip the bytes that did not go.
                    if (moved as u64) < length {
                        break;
                    }
                }
                other => return other,
            }
        }
        Outcome::Done(total)
    }

    /// `write` to a descriptor the filesystem backs.
    ///
    /// Only a device node accepts one: the image is read-only, and a write
    /// to anything else in it is `EBADF` — the descriptor could not have
    /// been opened for writing in the first place.
    fn write_file(
        &mut self,
        vnode: crate::mount::Vnode,
        flags: i32,
        offset: u64,
        buffer: i64,
        count: u64,
    ) -> Result<u64, Errno> {
        if flags & crate::file::open_flags::ACCESS_MODE == crate::file::open_flags::READ_ONLY {
            return Err(Errno::BadFile);
        }
        let inode = self.vfs.inode(vnode)?;
        match inode.file_type() {
            crate::image::file_type::CHARACTER => self.device_write_bytes(inode.payload, count),
            crate::image::file_type::REGULAR => {
                self.write_regular(vnode, flags, offset, buffer, count)
            }
            crate::image::file_type::DIRECTORY => Err(Errno::BadFile),
            // A fifo or socket in the image has nothing behind it.
            _ => Err(Errno::NoDevice),
        }
    }

    /// Sends a store's own account of a failure to the kernel log.
    ///
    /// The guest gets an errno, which cannot carry a reason; this is where
    /// the reason goes, and it is the only place it exists. Never attempted
    /// for a failure of the log itself — a store that cannot be written is
    /// not a store to report through.
    fn report_store_failure(&mut self, path: &[&[u8]]) {
        if path.starts_with(paths::LOG_ERROR) || paths::LOG_ERROR.starts_with(path) {
            return;
        }
        let mut message = Vec::new();
        message.extend_from_slice(b"kisal: the store at ");
        for segment in path {
            message.push(b'/');
            message.extend_from_slice(segment);
        }
        message.extend_from_slice(b" failed: ");
        self.store.last_error(&mut message);
        let _ = self.store.write(paths::LOG_ERROR, &message);
    }
}

/// A signed decimal integer of nanoseconds, as the store hands it over.
///
/// Written by hand because the answer has to be exact: a clock read through
/// a float would lose the low digits of any realtime value — 2026 is past
/// 2^60 nanoseconds, and an f64 carries 53 bits of significand, so the last
/// hundred nanoseconds would simply not be there.
///
/// Surrounding whitespace is accepted because a host writing a number into a
/// file is entitled to end it with a newline. Anything else is refused: a
/// clock that half-parsed is worse than one that failed.
fn parse_nanoseconds(bytes: &[u8]) -> Option<i64> {
    let text = core::str::from_utf8(bytes).ok()?.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let magnitude = digits.parse::<i128>().ok()?;
    let signed = if negative { -magnitude } else { magnitude };
    i64::try_from(signed).ok()
}

/// `SIGKILL` and `SIGSTOP`, the two a thread may not block.
const SIGKILL: i64 = 9;
const SIGSTOP: i64 = 19;

/// What a signal is set to do: Linux's `struct kernel_sigaction`, which is
/// what the raw `rt_sigaction` syscall exchanges — *not* glibc's `struct
/// sigaction`, which orders its fields differently and is converted by the
/// wrapper before the call.
///
/// Thirty-two bytes: handler, flags, restorer, mask. The restorer is the
/// return trampoline glibc supplies with `SA_RESTORER`, and it is recorded
/// rather than used — `rt_sigreturn` never runs here, because a delivered
/// handler returns through the resume chain kisal builds rather than through
/// a stack frame it has to unwind itself (see `container-plan.md`'s signal
/// design). Recording it anyway costs eight bytes and keeps `oldact` honest.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Disposition {
    handler: u64,
    flags: u64,
    restorer: u64,
    mask: u64,
}

impl Disposition {
    /// `SIG_DFL` is zero, which is also what every other field starts as.
    const DEFAULT: Self = Self {
        handler: SIG_DFL,
        flags: 0,
        restorer: 0,
        mask: 0,
    };

    fn to_bytes(self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0..8].copy_from_slice(&self.handler.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.flags.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.restorer.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.mask.to_le_bytes());
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let word = |at: usize| {
            u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
        };
        Self {
            handler: word(0),
            flags: word(8),
            restorer: word(16),
            mask: word(24),
        }
    }
}

/// The two handler values that are not addresses.
const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

/// The size of a `struct kernel_sigaction` on x86-64.
const SIGACTION_SIZE: u64 = 32;

/// A signal's bit in a mask. Signal one is bit zero, which is the off-by-one
/// every signal mask carries.
fn signal_bit(signal: i64) -> u64 {
    1u64 << (signal - 1)
}

/// Whether a signal's default action ends the process.
///
/// The list is Linux's: what is *not* here is the handful whose default is to
/// ignore or to stop. Getting this backwards for one signal would mean a
/// program that raises it either dies when it should not or keeps running
/// when it should not, so it is written out rather than approximated.
fn terminates(signal: i64) -> bool {
    const SIGCHLD: i64 = 17;
    const SIGCONT: i64 = 18;
    const SIGTSTP: i64 = 20;
    const SIGTTIN: i64 = 21;
    const SIGTTOU: i64 = 22;
    const SIGURG: i64 = 23;
    const SIGWINCH: i64 = 28;
    !matches!(
        signal,
        SIGCHLD | SIGCONT | SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU | SIGURG | SIGWINCH
    )
}
