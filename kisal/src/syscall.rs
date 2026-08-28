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
    pub const MADVISE: i64 = 28;
    pub const MREMAP: i64 = 25;
    pub const MSYNC: i64 = 26;
    pub const GETPID: i64 = 39;
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
            CLONE => "clone",
            EXIT => "exit",
            ARCH_PRCTL => "arch_prctl",
            FUTEX => "futex",
            GETDENTS64 => "getdents64",
            SET_TID_ADDRESS => "set_tid_address",
            CLOCK_GETTIME => "clock_gettime",
            EXIT_GROUP => "exit_group",
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
    /// The guest's address space: the arenas, and the tree of what is
    /// mapped where.
    pub space: crate::space::Space,
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
    pub fn new(store: S, machine: M, image: crate::image::Image<'a>) -> Self {
        let space_start = machine.memory_limit();
        let mut kernel = Self {
            store,
            machine,
            random: crate::random::Random::unseeded(),
            executable: String::new(),
            vfs: crate::vfs::Vfs::new(image),
            files: crate::fd::FdTable::with_standard_streams(),
            clock: 1,
            // Carved from the top of whatever the module already occupies:
            // the linker's data, the shadow stack, and anything the
            // kernel's own allocator has taken. Everything the guest is
            // given comes from there up.
            space: crate::space::Space::new(space_start),
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
                    Ok(file)
                        if file.flags & crate::file::open_flags::PATH != 0 =>
                    {
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

            _ => Outcome::Fault(Fault::of(number, arguments)),
        }
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
                    match self.write_file(vnode, file.flags, file.offset, buffer as i64, count as u64)
                    {
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
