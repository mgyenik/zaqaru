//! The read-only filesystem rows.
//!
//! Eighty per cent of a real application's syscalls land here, and none of
//! them reach the host: the image is a data segment in the same linear memory
//! the guest runs in, so a `read` is a copy and a `stat` is an index multiply.
//! That is the whole reason the design puts a kernel inside the module.
//!
//! Every structure written back to the guest is Linux's, byte for byte, at
//! the offsets x86-64 uses. A libc reads these with a fixed layout compiled
//! into it; there is no negotiation and no version to check.

use crate::abi::Store;
use crate::errno::Errno;
use crate::fd::{Backing, Console, MAX_DESCRIPTORS};
use crate::image::{Inode, file_type};
use crate::machine::Machine;
use crate::mount::Vnode;
use crate::paths;
use crate::syscall::{Arguments, Fault, Kernel, Outcome, number};
use crate::vfs::Lookup;

/// `open(2)` flags, in the x86-64 numbering.
pub mod open_flags {
    pub const ACCESS_MODE: i32 = 0o3;
    pub const READ_ONLY: i32 = 0o0;
    pub const WRITE_ONLY: i32 = 0o1;
    pub const READ_WRITE: i32 = 0o2;
    pub const CREATE: i32 = 0o100;
    pub const EXCLUSIVE: i32 = 0o200;
    pub const TRUNCATE: i32 = 0o1000;
    pub const APPEND: i32 = 0o2000;
    pub const NONBLOCK: i32 = 0o4000;
    /// On a pipe: packet mode. On a file: bypass the page cache.
    pub const DIRECT: i32 = 0o40000;
    pub const DIRECTORY: i32 = 0o200000;
    pub const NOFOLLOW: i32 = 0o400000;
    pub const CLOEXEC: i32 = 0o2000000;
    pub const PATH: i32 = 0o10000000;
}

/// The `…at` family's shared constants.
pub mod at {
    /// Resolve relative to the working directory. Negative, and not a
    /// descriptor: `openat(AT_FDCWD, …)` is `open(…)`.
    pub const FDCWD: i64 = -100;
    pub const SYMLINK_NOFOLLOW: i32 = 0x100;
    pub const REMOVEDIR: i32 = 0x200;
    pub const EACCESS: i32 = 0x200;
    pub const NO_AUTOMOUNT: i32 = 0x800;
    /// `statx`'s synchronisation hint, three values in two bits. There is
    /// nothing to synchronise here, so any of them is accepted and ignored.
    pub const STATX_SYNC_TYPE: i32 = 0x6000;
    pub const EMPTY_PATH: i32 = 0x1000;
    /// `linkat`'s opt-in to following a trailing symlink. `link` does not
    /// follow one by default — the link itself is what gains a name —
    /// which is the opposite of every other row that changes something.
    pub const SYMLINK_FOLLOW: i32 = 0x400;
}

pub mod seek {
    pub const SET: i32 = 0;
    pub const CURRENT: i32 = 1;
    pub const END: i32 = 2;
    pub const DATA: i32 = 3;
    pub const HOLE: i32 = 4;
}

/// The terminal `ioctl` requests a libc uses to decide whether it is talking
/// to a terminal. Every one of them answers `ENOTTY` here — see
/// [`Kernel::ioctl`].
pub mod ioctl_request {
    pub const TCGETS: u64 = 0x5401;
    pub const TCSETS: u64 = 0x5402;
    pub const TCSETSW: u64 = 0x5403;
    pub const TCSETSF: u64 = 0x5404;
    pub const TIOCGPGRP: u64 = 0x540f;
    pub const TIOCSPGRP: u64 = 0x5410;
    pub const TIOCGWINSZ: u64 = 0x5413;
    pub const TIOCSWINSZ: u64 = 0x5414;
    /// Set close-on-exec, and clear it — `fcntl(F_SETFD, FD_CLOEXEC)`
    /// spelled as an `ioctl`, and the spelling CPython uses.
    pub const FIONCLEX: u64 = 0x5450;
    pub const FIOCLEX: u64 = 0x5451;
}

pub mod fcntl_command {
    pub const DUPFD: i32 = 0;
    pub const GETFD: i32 = 1;
    pub const SETFD: i32 = 2;
    pub const GETFL: i32 = 3;
    pub const SETFL: i32 = 4;
    pub const GETLK: i32 = 5;
    pub const SETLK: i32 = 6;
    pub const SETLKW: i32 = 7;
    pub const SETOWN: i32 = 8;
    pub const GETOWN: i32 = 9;
    pub const GETOWN_EX: i32 = 16;
    pub const OFD_GETLK: i32 = 36;
    pub const OFD_SETLK: i32 = 37;
    pub const OFD_SETLKW: i32 = 38;
    pub const DUPFD_CLOEXEC: i32 = 1030;
}

/// Forced on for every descriptor on a 64-bit kernel, and reported by
/// `F_GETFL`. Its absence is how some callers conclude they are on a 32-bit
/// kernel without large-file support.
pub const O_LARGEFILE: i32 = 0o100000;

pub const FD_CLOEXEC: i32 = 1;

/// `access(2)` mode bits.
pub mod access_mode {
    pub const EXISTS: i32 = 0;
    pub const EXECUTE: i32 = 1;
    pub const WRITE: i32 = 2;
    pub const READ: i32 = 4;
}

/// Linux's `PATH_MAX`, which bounds every path read out of guest memory.
pub const PATH_MAX: usize = 4096;
/// Linux's `NAME_MAX`. Bounds a single component, in both directions: what
/// the guest hands to a lookup, and what comes back out of the index.
pub const MAX_NAME: usize = 255;
fn is_device_node(inode: &Inode) -> bool {
    matches!(inode.file_type(), file_type::CHARACTER | file_type::BLOCK)
}

/// How much of a generated stream is produced at a time. A guest asking for
/// a gigabyte of zeros must not make the kernel ask its allocator for one.
const DEVICE_CHUNK: usize = 4096;

/// Linux's `XATTR_NAME_MAX`. A longer name is `ERANGE`, which is what the
/// kernel answers rather than truncating it into a name that matches
/// something else.
pub const XATTR_NAME_MAX: usize = 255;

/// `struct stat` on x86-64: 144 bytes, and the offsets are not negotiable.
pub const STAT_SIZE: usize = 144;
/// `struct statx`: 256 bytes.
pub const STATX_SIZE: usize = 256;
/// `STATX_BASIC_STATS` — everything `stat` would have answered, and nothing
/// more. Linux on ext4 answers `0x17ff`, which adds `STATX_BTIME` and
/// `STATX_MNT_ID`; the image has no birth time, and this kernel does not
/// give a mount an id a guest could ask for, so advertising either would
/// claim a field it cannot fill.
const STATX_BASIC_STATS: u32 = 0x7ff;

/// The block size reported for image files. Arbitrary in the sense that there
/// is no device, and 4096 in the sense that everything expects a page.
const BLOCK_SIZE: i64 = 4096;

/// The device the console streams report, and the device number they carry.
/// `(5, 1)` is `/dev/console` on Linux; the streams are not a tty here (see
/// the `ioctl` decision in the build plan) but they are character devices,
/// and reporting them as anything else makes `isatty` and every buffering
/// heuristic read the wrong story.
const CONSOLE_DEVICE: u64 = 0x0000_0006;
const CONSOLE_RDEV: u64 = (5 << 8) | 1;
/// Inode numbers for the console streams, outside the range image inodes can
/// reach: `st_dev` plus `st_ino` is how userspace decides two descriptors name
/// one file, so these have to be distinct from each other and from the image.
const CONSOLE_INODE_BASE: u64 = 0xffff_ff00;

/// Whether a `dirfd` argument names the working directory.
///
/// **A descriptor is an `int`, whatever width the register carrying it has,
/// and this is the one place that has to remember it.** The syscall ABI
/// hands every argument over in a 64-bit register, and a caller that writes
/// only the low half leaves the top half zero: glibc's `openat` wrapper sets
/// `edi`, so `AT_FDCWD` arrives as `0x0000_0000_ffff_ff9c` and not as −100.
/// Linux reads the low 32 bits as an `int`, so a comparison at 64 bits sees
/// a number that is not `AT_FDCWD` and not a descriptor either.
///
/// Measured cost of getting it wrong: every *relative* path through the
/// `…at` family answered `EBADF`. Absolute ones did not, because an absolute
/// path never looks at the descriptor at all — which is why the whole
/// dynamic tier, `ld.so` and all, ran for a day without noticing. CPython
/// noticed on the first thing it looks for that is not absolute:
/// `openat(AT_FDCWD, "pyvenv.cfg")`, three lines into `getpath`.
///
/// The truncation is not merely the fix, it is the conformant answer: a
/// descriptor argument of `0x1_0000_0005` is descriptor 5 on Linux, because
/// the top half was never part of the number.
fn names_working_directory(dirfd: i64) -> bool {
    dirfd as i32 == at::FDCWD as i32
}

impl<S: Store, M: Machine> Kernel<'_, S, M> {
    // ---- resolution helpers ------------------------------------------

    /// Where a path in the `…at` family starts from.
    pub(crate) fn start_directory(&self, dirfd: i64) -> Result<Vnode, Errno> {
        if names_working_directory(dirfd) {
            return Ok(self.vfs.working_directory());
        }
        let fd = dirfd as i32;
        let Backing::Image(inode) = self.files.description(fd)?.backing else {
            return Err(Errno::NotDir);
        };
        // `openat` on a descriptor that is not a directory is `ENOTDIR`, and
        // saying so here keeps the walk from having to care.
        if !self.vfs.inode(inode)?.is_directory() {
            return Err(Errno::NotDir);
        }
        Ok(inode)
    }

    /// What `AT_EMPTY_PATH` names: the descriptor itself rather than a path
    /// under it, which is how `fstat` and `readlink`-on-a-descriptor are
    /// spelled through the `…at` family. `AT_FDCWD` names the working
    /// directory, which is what Linux does and what `statx(AT_FDCWD, "", …)`
    /// relies on.
    ///
    /// One helper rather than three copies: the three rows that accept the
    /// flag were drifting apart, and a descriptor's meaning cannot be
    /// allowed to depend on which syscall asked.
    fn empty_path_target(&self, dirfd: i64) -> Result<Backing, Errno> {
        if names_working_directory(dirfd) {
            return Ok(Backing::Image(self.vfs.working_directory()));
        }
        Ok(self.files.description(dirfd as i32)?.backing)
    }

    /// The image inode a descriptor names, for the rows that only make sense
    /// against one. `not_a_file` is what a console descriptor answers, which
    /// differs per row: `ESPIPE` for a seek, `ENOTDIR` for a directory read.
    fn image_inode(&self, fd: i32, not_a_file: Errno) -> Result<Vnode, Errno> {
        match self.files.description(fd)?.backing {
            Backing::Image(inode) => Ok(inode),
            Backing::Console(_)
            | Backing::Pipe { .. }
            | Backing::Epoll(_)
            | Backing::Socket(_) => Err(not_a_file),
        }
    }

    /// Reads a path out of guest memory, bounded by `PATH_MAX`.
    pub(crate) fn path_at(&self, address: i64) -> Result<&'static [u8], Errno> {
        // SAFETY: bounds-checked against the guest's memory, and refuses a
        // string with no terminator inside the bound rather than reading on.
        unsafe { self.memory().c_string(address as u64, PATH_MAX) }
    }

    fn resolve_at(&self, dirfd: i64, path: i64, lookup: Lookup) -> Result<Vnode, Errno> {
        let path = self.path_at(path)?;
        // An absolute path ignores the descriptor, so the descriptor is not
        // validated either — Linux never dereferences `dirfd` once it has
        // seen a leading slash. Checking it first turns `openat(closed_fd,
        // "/etc/passwd")` into `EBADF` on a call that works everywhere else,
        // which breaks anything that caches a dirfd and keeps using absolute
        // paths through it.
        let start = if path.first() == Some(&b'/') {
            self.vfs.root()
        } else {
            self.start_directory(dirfd)?
        };
        self.vfs.resolve(start, path, lookup)
    }

    // ---- the rows ----------------------------------------------------

    pub(crate) fn open(&mut self, arguments: Arguments) -> Outcome {
        self.openat(Arguments::new([
            at::FDCWD,
            arguments.get(0),
            arguments.get(1),
            arguments.get(2),
            0,
            0,
        ]))
    }

    pub(crate) fn openat(&mut self, arguments: Arguments) -> Outcome {
        let flags = arguments.get(2) as i32;
        // `O_PATH` opens a *reference* to a file rather than the file: the
        // access mode is ignored, no read or write is permitted afterwards,
        // and the descriptor is good for `fstat`, for `close`, and as a
        // directory descriptor for the `…at` family. glibc uses it in
        // `fexecve` and `realpath`; declaring the flag and ignoring it, as
        // this did, hands the caller an ordinary readable descriptor instead.
        let path_only = flags & open_flags::PATH != 0;
        let lookup = Lookup {
            follow_final: flags & open_flags::NOFOLLOW == 0,
            require_directory: flags & open_flags::DIRECTORY != 0,
        };
        let resolution = self.resolve_at(arguments.get(0), arguments.get(1), lookup);

        // Linux decides read-only-ness *before* it decides the file is
        // missing, because creating is what it would have had to do next:
        // `open(missing, O_CREAT)` on a read-only filesystem is `EROFS`, not
        // `ENOENT`. Getting the order wrong tells a caller the directory
        // vanished when the truth is that nothing here can be written.
        let inode = match resolution {
            Ok(inode) => inode,
            // The file is not there and the caller asked for it to be
            // created, which is the one case where a failed resolution is
            // not the answer.
            Err(Errno::NoEntry) if flags & open_flags::CREATE != 0 && !path_only => {
                // `O_CREAT` creates a regular file, and two ways of asking
                // say the caller wanted a directory instead. Linux refuses
                // both rather than making a file with that name: a trailing
                // slash is `EISDIR`, and `O_DIRECTORY` is `EINVAL`.
                // Measured; without this, `open("/x/", O_CREAT)` creates a
                // regular file called `x` and hands back a descriptor.
                let trailing_slash = match self.path_at(arguments.get(1)) {
                    Ok(text) => text.last() == Some(&b'/'),
                    Err(errno) => return Outcome::Done(errno.as_result()),
                };
                if trailing_slash {
                    return Outcome::Done(Errno::IsDir.as_result());
                }
                if flags & open_flags::DIRECTORY != 0 {
                    return Outcome::Done(Errno::Invalid.as_result());
                }
                match self.create_file(arguments.get(0), arguments.get(1), arguments.get(3) as u32)
                {
                    Ok(created) => {
                        return match self.files.open(
                            Backing::Image(created),
                            flags & !open_flags::CLOEXEC,
                            flags & open_flags::CLOEXEC != 0,
                        ) {
                            Ok(fd) => Outcome::Done(fd as i64),
                            Err(errno) => Outcome::Done(errno.as_result()),
                        };
                    }
                    Err(errno) => return Outcome::Done(errno.as_result()),
                }
            }
            Err(errno) => return Outcome::Done(errno.as_result()),
        };

        let resolved = match self.vfs.inode(inode) {
            Ok(resolved) => resolved,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        // `O_NOFOLLOW` on a symlink is `ELOOP`, Linux's way of saying "this
        // is a link and you said not to follow one" — except under `O_PATH`,
        // where the pair is the documented way to hold the link itself.
        if flags & open_flags::NOFOLLOW != 0 && resolved.is_symlink() && !path_only {
            return Outcome::Done(Errno::Loop.as_result());
        }
        // The file exists, so `O_EXCL` fails here rather than at the
        // read-only check — `EEXIST` beats `EROFS` when both apply.
        if flags & (open_flags::CREATE | open_flags::EXCLUSIVE)
            == (open_flags::CREATE | open_flags::EXCLUSIVE)
        {
            return Outcome::Done(Errno::Exists.as_result());
        }

        let access = flags & open_flags::ACCESS_MODE;
        if !path_only {
            if resolved.is_directory() && access != open_flags::READ_ONLY {
                return Outcome::Done(Errno::IsDir.as_result());
            }
            // Asking to write, or to truncate, is what a read-only image
            // refuses. `O_CREAT` on a file that *exists* asks for neither, and
            // Linux lets it through — so it is not part of this test.
            //
            // A device node is not part of it either. `EROFS` is a fact
            // about the filesystem holding the *name*, and writing to
            // `/dev/null` does not write to the filesystem at all — which is
            // why `access("/dev/null", W_OK)` succeeds on a read-only mount
            // on Linux, checked against one. Opening `/dev/null` for writing
            // is among the first things a daemonising process does.
            let is_device = is_device_node(&resolved);
            let wants_change = access != open_flags::READ_ONLY || flags & open_flags::TRUNCATE != 0;
            if !is_device && wants_change && !self.is_writable(inode) {
                return Outcome::Done(Errno::ReadOnlyFs.as_result());
            }
        }

        // A descriptor opened for writing points at the *upper* copy, made
        // here while the path is still in hand — a file has no parent
        // pointer, so after this the name it was reached through is gone.
        // This is also where overlayfs copies up, and for the same reason.
        let mut inode = inode;
        if !path_only && !resolved.is_directory() && !is_device_node(&resolved) {
            let wants_change = access != open_flags::READ_ONLY || flags & open_flags::TRUNCATE != 0;
            if wants_change {
                // Following a trailing symlink, unless `O_NOFOLLOW` said
                // not to — in which case resolution already refused it.
                inode = match self.copy_up_path(arguments.get(0), arguments.get(1), true) {
                    Ok(upper) => upper,
                    Err(errno) => return Outcome::Done(errno.as_result()),
                };
            }
            // `O_TRUNC` empties the file before the descriptor exists,
            // which is why a program that opens a log this way sees an
            // empty one even if it writes nothing.
            if flags & open_flags::TRUNCATE != 0 && resolved.is_regular() {
                if let Err(errno) = self.truncate_open(inode) {
                    return Outcome::Done(errno.as_result());
                }
            }
        }

        match self.files.open(
            Backing::Image(inode),
            flags & !open_flags::CLOEXEC,
            flags & open_flags::CLOEXEC != 0,
        ) {
            Ok(fd) => Outcome::Done(fd as i64),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    /// `ioctl`, which at this milestone answers exactly one family of
    /// requests and answers it "no".
    ///
    /// Stdio is not a terminal. That is a decision rather than a limitation:
    /// a container writing to a pipe is the ordinary case, and a kernel that
    /// pretended otherwise would have to answer `TCGETS` with a plausible
    /// `termios` and then honour it — echo, canonical mode, window size,
    /// signals on `^C` — none of which exists. CPython responds to `ENOTTY`
    /// by block-buffering, which is correct for a pipe; the image sets
    /// `PYTHONUNBUFFERED=1` rather than this kernel claiming to be a
    /// terminal.
    ///
    /// Everything else is a named fault. `ioctl` is a thousand unrelated
    /// calls behind one number, and `EINVAL` for an unimplemented one would
    /// say "that request was malformed" about a request that was not.
    pub(crate) fn ioctl(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        // The descriptor is checked first: `ioctl` on a closed one is
        // `EBADF` whatever the request would have been.
        if !self.files.is_open(fd) {
            return Outcome::Done(Errno::BadFile.as_result());
        }
        let request = arguments.get(1) as u64;
        match request {
            ioctl_request::TCGETS
            | ioctl_request::TCSETS
            | ioctl_request::TCSETSW
            | ioctl_request::TCSETSF
            | ioctl_request::TIOCGPGRP
            | ioctl_request::TIOCSPGRP
            | ioctl_request::TIOCGWINSZ
            | ioctl_request::TIOCSWINSZ => Outcome::Done(Errno::NoTty.as_result()),
            // Not a terminal question at all: these are the descriptor's
            // close-on-exec flag under another name, and the fd table
            // already holds it. CPython sets it this way rather than through
            // `fcntl` on every descriptor it opens, so an image whose Python
            // opens a file reaches here before it reaches anything else.
            ioctl_request::FIOCLEX | ioctl_request::FIONCLEX => {
                let value = request == ioctl_request::FIOCLEX;
                Outcome::Done(match self.files.set_close_on_exec(fd, value) {
                    Ok(()) => 0,
                    Err(errno) => errno.as_result(),
                })
            }
            _ => Outcome::Fault(Fault::detailed(
                number::IOCTL,
                arguments,
                "an ioctl request this kernel has no driver for",
            )),
        }
    }

    pub(crate) fn close(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        // What this descriptor pointed at, before it stops pointing at
        // anything: an unlinked file is freed when its last descriptor
        // goes, and afterwards there is no way to ask what that was.
        let held = match self.files.description(fd) {
            Ok(file) => match file.backing {
                Backing::Image(vnode) => Some(vnode),
                Backing::Console(_)
                | Backing::Pipe { .. }
                | Backing::Epoll(_)
                | Backing::Socket(_) => None,
            },
            Err(_) => None,
        };
        // A pipe end is counted in descriptors, and this may be the last
        // one — which is what turns a reader's next `read` into end-of-file
        // and a writer's next `write` into `EPIPE`.
        let before = self.shared_census();
        // Which description this descriptor named, read while it still
        // names one: if this is the last descriptor on it, the description
        // is about to go and anything holding its *index* has to be told.
        let had = self.files.description_index(fd).ok();
        let result = self.files.close(fd);
        if result.is_ok() {
            self.reclaim_after_close(held);
            self.reconcile_shared(&before);
            self.forget_description(had);
        }
        Outcome::Done(match result {
            Ok(()) => 0,
            Err(errno) => errno.as_result(),
        })
    }

    pub(crate) fn read(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        let count = arguments.get(2);
        if count < 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        // Before anything that keeps an offset, because a pipe has none and
        // because a pipe read can park — which `read_at`'s signature has no
        // way to say.
        if let Some((ring, end, flags)) = self.pipe_of(fd) {
            if end == crate::ring::End::Write {
                return Outcome::Done(Errno::BadFile.as_result());
            }
            return self.transfer_ring(ring, end, flags, arguments.get(1) as u64, count as u64);
        }
        // And a socket, for the same reason and through the same transfer.
        match self.socket_ring(fd, crate::ring::End::Read) {
            crate::socket::Reach::Ring { ring, flags } => {
                return self.transfer_ring(
                    ring,
                    crate::ring::End::Read,
                    flags,
                    arguments.get(1) as u64,
                    count as u64,
                );
            }
            crate::socket::Reach::Finished => return Outcome::Done(0),
            crate::socket::Reach::Refused(errno) => {
                return Outcome::Done(errno.as_result());
            }
            crate::socket::Reach::Elsewhere => {}
        }
        let offset = match self.files.description(fd) {
            Ok(file) => file.offset,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        match self.read_at(fd, arguments.get(1), count as u64, offset) {
            Ok(read) => {
                self.files
                    .description_mut(fd)
                    .expect("the description was here a moment ago")
                    .offset = offset + read;
                Outcome::Done(read as i64)
            }
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    /// `pread64`: the same read against an explicit offset, leaving the
    /// description's own position alone. That is the whole difference, and it
    /// is why it exists.
    pub(crate) fn pread(&mut self, arguments: Arguments) -> Outcome {
        let count = arguments.get(2);
        let offset = arguments.get(3);
        if offset < 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        match self.read_at(
            arguments.get(0) as i32,
            arguments.get(1),
            count as u64,
            offset as u64,
        ) {
            Ok(read) => Outcome::Done(read as i64),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    fn read_at(&mut self, fd: i32, buffer: i64, count: u64, offset: u64) -> Result<u64, Errno> {
        // Linux checks the caller's buffer against the *requested* count
        // before it looks at the file at all: `read(fd, buf, (size_t)-1)` is
        // `EFAULT`, because no buffer is that big. Clamping to the file's
        // length first instead would turn a garbage count into a cheerful
        // short read — verified against this machine's kernel, which answers
        // `EFAULT` for a count of `-1` and `EINVAL` for an offset of `-1`,
        // in that order.
        self.memory().check(buffer as u64, count)?;
        let file = *self.files.description(fd)?;
        if file.flags & open_flags::ACCESS_MODE == open_flags::WRITE_ONLY {
            return Err(Errno::BadFile);
        }
        let vnode = match file.backing {
            Backing::Image(vnode) => vnode,
            // Standard input is the one descriptor whose bytes come from the
            // host. Everything else the filesystem answers from the image.
            Backing::Console(Console::Input) => {
                return self.read_console_input(buffer, count, offset);
            }
            Backing::Console(_) => return Err(Errno::BadFile),
            // `read` never arrives here — its row answers a pipe before
            // reaching this, because a pipe read can *park* and a
            // `Result<u64, Errno>` has nowhere to say so. What does arrive
            // is `pread`, and a pipe has no position to read at.
            Backing::Pipe { .. } => return Err(Errno::NotSeekable),
            // An `epoll` descriptor is a set, not a stream; reading it is
            // `epoll_wait` and nothing else.
            Backing::Epoll(_) => return Err(Errno::Invalid),
            // `read` answers a socket before reaching here, for the same
            // reason it answers a pipe: the transfer can park. `pread` on
            // a socket has no position to read at.
            Backing::Socket(_) => return Err(Errno::NotSeekable),
        };
        // An `O_PATH` descriptor refers to a file without opening it, so
        // there is nothing to read from.
        if file.flags & open_flags::PATH != 0 {
            return Err(Errno::BadFile);
        }
        let inode = self.vfs.inode(vnode)?;
        // `read` on a directory is `EISDIR`; `getdents64` is how a directory
        // is read, and conflating the two is a classic source of nonsense.
        if inode.is_directory() {
            return Err(Errno::IsDir);
        }
        if inode.file_type() == file_type::CHARACTER {
            // The inode's own type sends the read to a driver, which is what
            // a real VFS does — rather than a path comparison, which
            // something could have moved.
            return self.device_read(inode.payload, buffer, count);
        }
        // A generated file is a window onto kernel state, rendered when it
        // is read. `/proc/self/maps` is the VMA tree, and a snapshot taken
        // when the file was created would describe an address space that
        // has since changed under it.
        if inode.flags & crate::image::inode_flags::GENERATED != 0 {
            return self.read_generated(inode.payload, buffer, count, offset);
        }
        if !inode.is_regular() {
            // A fifo, socket or block device in the image has nothing behind
            // it: no pipe was ever created, no disk was ever attached. An
            // `EINVAL` here would be a plausible answer to a perfectly valid
            // call, so it is refused by name instead.
            return Err(Errno::NoDevice);
        }
        let contents = self
            .vfs
            .filesystem_of(vnode)?
            .contents(&inode, vnode.inode)?;
        // At full width. `usize` is 32 bits inside the module, so casting the
        // guest's offset and count first lets `offset = 1 << 32` read the
        // head of the file and report success, and lets a large count wrap
        // the sum and invert the slice range into a panic.
        let length = contents.len() as u64;
        let from = offset.min(length);
        let to = from.saturating_add(count).min(length);
        let slice = &contents[from as usize..to as usize];
        // The address space by its field rather than through `memory_mut`,
        // because `contents` still borrows the filesystem and the two are
        // disjoint parts of the kernel. Copying the bytes out to satisfy a
        // whole-`self` borrow would put an allocation on the read path,
        // which is the hottest path in the container.
        // SAFETY: bounds-checked against the guest's memory before writing.
        unsafe { crate::memory::GuestMemory::new(&mut self.pages).write(buffer as u64, slice)? };
        Ok(slice.len() as u64)
    }

    /// Standard input, read through the console mount.
    ///
    /// The description's offset is what makes a second read return nothing
    /// rather than the same bytes again — a stream that never reports EOF is
    /// one every reader loops on forever.
    // ---- device drivers ----------------------------------------------

    /// What a character device answers when it is read.
    ///
    /// Dispatched on the device number, which is the identity Linux gives a
    /// driver and the one a program sees in `st_rdev`. A device this kernel
    /// has no driver for is a named fault rather than an errno: the node is
    /// in the image because some base image put it there, and answering
    /// `EINVAL` would say "that call was malformed" about a call that was
    /// not.
    fn device_read(&mut self, rdev: u64, buffer: i64, count: u64) -> Result<u64, Errno> {
        let device = split_device(rdev);
        match device {
            // Always at end of file. Reading `/dev/null` is how a program
            // asks for nothing at all.
            crate::synthetic::NULL_DEVICE => Ok(0),
            // Zeros, and `/dev/full` reads the same — its whole difference
            // is what happens when it is *written*.
            crate::synthetic::ZERO_DEVICE | crate::synthetic::FULL_DEVICE => {
                self.fill_guest(buffer, count, |chunk| chunk.fill(0))
            }
            // One stream, expanded from the boot seed. `/dev/random` and
            // `/dev/urandom` are the same device on Linux since 5.6, and a
            // container whose `/dev/random` blocked would hang programs
            // that still prefer it out of habit.
            crate::synthetic::RANDOM_DEVICE | crate::synthetic::URANDOM_DEVICE => {
                if !self.random.is_seeded() {
                    return Err(Errno::NoDevice);
                }
                let mut buffered = [0u8; DEVICE_CHUNK];
                let mut written = 0u64;
                while written < count {
                    let take = ((count - written) as usize).min(DEVICE_CHUNK);
                    self.random.fill(&mut buffered[..take])?;
                    // SAFETY: bounds-checked against the guest's memory.
                    unsafe {
                        self.memory_mut()
                            .write(buffer as u64 + written, &buffered[..take])?
                    };
                    written += take as u64;
                }
                Ok(written)
            }
            _ => Err(Errno::NoDevice),
        }
    }

    /// What a character device does with bytes written to it.
    pub(crate) fn device_write_bytes(&mut self, rdev: u64, count: u64) -> Result<u64, Errno> {
        match split_device(rdev) {
            // Accepted and discarded, which is the whole job.
            crate::synthetic::NULL_DEVICE | crate::synthetic::ZERO_DEVICE => Ok(count),
            // `/dev/full` exists to fail this way, and programs test their
            // error handling against it.
            crate::synthetic::FULL_DEVICE => Err(Errno::NoSpace),
            // Linux accepts writes and mixes them into the pool. Accepting
            // and discarding is indistinguishable from the guest's side, and
            // it keeps the stream a function of the boot seed alone — which
            // is what makes a run replayable.
            crate::synthetic::RANDOM_DEVICE | crate::synthetic::URANDOM_DEVICE => Ok(count),
            _ => Err(Errno::NoDevice),
        }
    }

    /// Writes a generated pattern into the guest, a chunk at a time.
    ///
    /// A chunk rather than an allocation: the count is the guest's, and a
    /// guest asking for a gigabyte of zeros must not make the kernel ask its
    /// allocator for one.
    fn fill_guest(
        &mut self,
        buffer: i64,
        count: u64,
        pattern: impl Fn(&mut [u8]),
    ) -> Result<u64, Errno> {
        let mut buffered = [0u8; DEVICE_CHUNK];
        pattern(&mut buffered);
        let mut written = 0u64;
        while written < count {
            let take = ((count - written) as usize).min(DEVICE_CHUNK);
            // SAFETY: bounds-checked against the guest's memory.
            unsafe {
                self.memory_mut()
                    .write(buffer as u64 + written, &buffered[..take])?
            };
            written += take as u64;
        }
        Ok(written)
    }

    /// Serves a generated file: render it, then read from the rendering.
    ///
    /// Rendered whole on every read rather than incrementally, which is
    /// what procfs does too — the file has no stable length, so a reader
    /// that seeks into it is reading into a snapshot taken at that moment.
    fn read_generated(
        &mut self,
        view: u64,
        buffer: i64,
        count: u64,
        offset: u64,
    ) -> Result<u64, Errno> {
        let rendered = match view {
            crate::synthetic::view::MAPS => self.render_maps(),
            // A view nothing renders would read as an empty file, which is
            // a plausible wrong answer to a program asking about the kernel.
            _ => return Err(Errno::NoDevice),
        };
        let bytes = rendered.as_bytes();
        let length = bytes.len() as u64;
        let from = offset.min(length);
        let to = from.saturating_add(count).min(length);
        let slice = &bytes[from as usize..to as usize];
        // SAFETY: bounds-checked against the guest's memory before writing.
        unsafe { self.memory_mut().write(buffer as u64, slice)? };
        Ok(slice.len() as u64)
    }

    fn read_console_input(&mut self, buffer: i64, count: u64, offset: u64) -> Result<u64, Errno> {
        let mut bytes = Vec::new();
        match self.store.read(paths::CONSOLE_STDIN, &mut bytes) {
            crate::abi::StoreOutcome::Failed => return Err(Errno::Io),
            crate::abi::StoreOutcome::Absent => return Ok(0),
            crate::abi::StoreOutcome::Present => {}
        }
        let length = bytes.len() as u64;
        let from = offset.min(length);
        let to = from.saturating_add(count).min(length);
        let slice = &bytes[from as usize..to as usize];
        // SAFETY: bounds-checked against the guest's memory before writing.
        unsafe { self.memory_mut().write(buffer as u64, slice)? };
        Ok(slice.len() as u64)
    }

    pub(crate) fn lseek(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        let requested = arguments.get(1);
        let whence = arguments.get(2) as i32;

        let current = match self.files.description(fd) {
            Ok(file) => file.offset,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        // A console stream has no position to seek: `ESPIPE` is what Linux
        // answers for a pipe or a character device, and what every libc's
        // `ftell` translates into "this stream is not seekable".
        let vnode = match self.image_inode(fd, Errno::NotSeekable) {
            Ok(vnode) => vnode,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let inode = match self.vfs.inode(vnode) {
            Ok(inode) => inode,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        // Seeking a directory abandons where the scan had got to. That is
        // what `rewinddir` is, and what `seekdir` to any other cookie means
        // too: the caller is asking for a position rather than for the next
        // thing after a name.
        if inode.is_directory() {
            self.files.clear_resume(fd);
        }
        // A character device has no position at all. Linux answers zero to
        // every seek on one and leaves the offset where it was — not
        // `ESPIPE`, which is what a *pipe* answers, and not the requested
        // offset, which would make a later `read` look like it had skipped
        // bytes it never had. Checked against `/dev/zero` and `/dev/null`.
        if inode.file_type() == file_type::CHARACTER {
            if !matches!(whence, seek::SET | seek::CURRENT | seek::END) {
                return Outcome::Done(Errno::Invalid.as_result());
            }
            return Outcome::Done(0);
        }
        let size = inode.size;

        let base = match whence {
            seek::SET => 0,
            seek::CURRENT => current as i64,
            seek::END => size as i64,
            // An image file has no holes, so every offset inside it is data
            // and the only hole is the end. That is exactly what a
            // non-sparse file reports on Linux, and it is what `cp` and
            // `tar` read to decide there is nothing to skip.
            seek::DATA => {
                if requested < 0 || requested as u64 >= size {
                    return Outcome::Done(Errno::NoData.as_result());
                }
                self.files
                    .description_mut(fd)
                    .expect("the description was here a moment ago")
                    .offset = requested as u64;
                return Outcome::Done(requested);
            }
            seek::HOLE => {
                if requested < 0 || requested as u64 > size {
                    return Outcome::Done(Errno::NoData.as_result());
                }
                self.files
                    .description_mut(fd)
                    .expect("the description was here a moment ago")
                    .offset = size;
                return Outcome::Done(size as i64);
            }
            _ => return Outcome::Done(Errno::Invalid.as_result()),
        };
        let Some(position) = base.checked_add(requested) else {
            return Outcome::Done(Errno::Invalid.as_result());
        };
        // Seeking past the end is legal and ordinary — that is how a sparse
        // file gets written. Seeking before it is `EINVAL`, and so is a seek
        // that overflows.
        //
        // Nothing narrower. Every filesystem has a maximum offset and they
        // differ: this machine's ext4 refuses `lseek(fd, INT64_MAX)` because
        // its `s_maxbytes` is smaller, while tmpfs accepts it — both are
        // legal, and refusing early here would break the one caller that
        // legitimately seeks large, a directory read whose `d_off` cookies
        // are not byte positions at all.
        if position < 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        self.files
            .description_mut(fd)
            .expect("the description was here a moment ago")
            .offset = position as u64;
        Outcome::Done(position)
    }

    pub(crate) fn stat(&mut self, arguments: Arguments) -> Outcome {
        self.stat_path(arguments.get(0), arguments.get(1), Lookup::FOLLOW)
    }

    pub(crate) fn lstat(&mut self, arguments: Arguments) -> Outcome {
        self.stat_path(arguments.get(0), arguments.get(1), Lookup::NO_FOLLOW)
    }

    fn stat_path(&mut self, path: i64, destination: i64, lookup: Lookup) -> Outcome {
        match self.resolve_at(at::FDCWD, path, lookup) {
            Ok(inode) => self.write_stat(inode, destination),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    pub(crate) fn fstat(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        match self.files.description(fd) {
            Ok(file) => match file.backing {
                Backing::Image(inode) => self.write_stat(inode, arguments.get(1)),
                Backing::Console(stream) => self.write_console_stat(stream, arguments.get(1)),
                Backing::Pipe { ring, .. } => self.write_pipe_stat(ring, arguments.get(1)),
                Backing::Epoll(id) => self.write_epoll_stat(id, arguments.get(1)),
                Backing::Socket(id) => self.write_socket_stat(id, arguments.get(1)),
            },
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    /// `fstat` on a pipe.
    ///
    /// `S_IFIFO`, and `st_size` is what is queued — which is not decoration:
    /// a program that stats a descriptor to decide how to read it must see a
    /// fifo rather than a regular file, because the difference is whether
    /// seeking is allowed and whether a short read means end of file.
    fn write_pipe_stat(&mut self, ring: u32, destination: i64) -> Outcome {
        let mut buffer = [0u8; STAT_SIZE];
        let queued = self.rings.borrow().queued(ring) as u64;
        encode_pipe_stat(&mut buffer, ring, queued);
        // SAFETY: bounds-checked against the guest's memory.
        match unsafe { self.memory_mut().write(destination as u64, &buffer) } {
            Ok(()) => Outcome::Done(0),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    /// `fstat` on an `epoll` descriptor.
    ///
    /// A mode with **no file-type bits at all**, which is what an anonymous
    /// inode reports and is measured rather than guessed: `fstat` on a real
    /// `epoll_create1` descriptor answers `0600`, where a pipe answers
    /// `010600`. A program that branches on `S_ISREG` must not be told this
    /// is a file.
    /// `fstat` on a socket: `S_IFSOCK`, and `st_size` is what is waiting to
    /// be read — which is what a program calling `FIONREAD` by another name
    /// would want, and what Linux answers.
    fn write_socket_stat(&mut self, id: u32, destination: i64) -> Outcome {
        let mut buffer = [0u8; STAT_SIZE];
        let queued = match self.sockets.borrow().endpoint(id) {
            Some(endpoint) => self.rings.borrow().queued(endpoint.receive) as u64,
            None => 0,
        };
        let number = SOCKET_INODE_BASE + u64::from(id);
        buffer[0..8].copy_from_slice(&SOCKET_DEVICE.to_le_bytes());
        buffer[8..16].copy_from_slice(&number.to_le_bytes());
        buffer[16..24].copy_from_slice(&1u64.to_le_bytes());
        buffer[24..28].copy_from_slice(&(file_type::SOCKET | 0o777).to_le_bytes());
        buffer[48..56].copy_from_slice(&queued.to_le_bytes());
        buffer[56..64].copy_from_slice(&BLOCK_SIZE.to_le_bytes());
        // SAFETY: bounds-checked against the guest's memory.
        match unsafe { self.memory_mut().write(destination as u64, &buffer) } {
            Ok(()) => Outcome::Done(0),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    fn write_epoll_stat(&mut self, id: u32, destination: i64) -> Outcome {
        let mut buffer = [0u8; STAT_SIZE];
        let number = EPOLL_INODE_BASE + u64::from(id);
        buffer[0..8].copy_from_slice(&EPOLL_DEVICE.to_le_bytes());
        buffer[8..16].copy_from_slice(&number.to_le_bytes());
        buffer[16..24].copy_from_slice(&1u64.to_le_bytes());
        buffer[24..28].copy_from_slice(&0o600u32.to_le_bytes());
        buffer[56..64].copy_from_slice(&BLOCK_SIZE.to_le_bytes());
        // SAFETY: bounds-checked against the guest's memory.
        match unsafe { self.memory_mut().write(destination as u64, &buffer) } {
            Ok(()) => Outcome::Done(0),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    fn write_console_stat(&mut self, stream: Console, destination: i64) -> Outcome {
        let mut buffer = [0u8; STAT_SIZE];
        encode_console_stat(&mut buffer, stream);
        // SAFETY: bounds-checked against the guest's memory.
        match unsafe { self.memory_mut().write(destination as u64, &buffer) } {
            Ok(()) => Outcome::Done(0),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    pub(crate) fn newfstatat(&mut self, arguments: Arguments) -> Outcome {
        let flags = arguments.get(3) as i32;
        const SUPPORTED: i32 = at::SYMLINK_NOFOLLOW | at::EMPTY_PATH | at::NO_AUTOMOUNT;
        if flags & !SUPPORTED != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let lookup = Lookup {
            follow_final: flags & at::SYMLINK_NOFOLLOW == 0,
            require_directory: false,
        };
        // `AT_EMPTY_PATH` makes an empty path mean the descriptor itself,
        // which is how `fstat` is spelled through this call.
        let path = match self.path_at(arguments.get(1)) {
            Ok(path) => path,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        if path.is_empty() && flags & at::EMPTY_PATH != 0 {
            return match self.empty_path_target(arguments.get(0)) {
                Ok(Backing::Image(inode)) => self.write_stat(inode, arguments.get(2)),
                Ok(Backing::Console(stream)) => self.write_console_stat(stream, arguments.get(2)),
                Ok(Backing::Pipe { ring, .. }) => self.write_pipe_stat(ring, arguments.get(2)),
                Ok(Backing::Epoll(id)) => self.write_epoll_stat(id, arguments.get(2)),
                Ok(Backing::Socket(id)) => self.write_socket_stat(id, arguments.get(2)),
                Err(errno) => Outcome::Done(errno.as_result()),
            };
        }
        match self.resolve_at(arguments.get(0), arguments.get(1), lookup) {
            Ok(inode) => self.write_stat(inode, arguments.get(2)),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    fn write_stat(&mut self, vnode: Vnode, destination: i64) -> Outcome {
        let inode = match self.vfs.inode(vnode) {
            Ok(inode) => inode,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let device = match self.vfs.device(vnode) {
            Ok(device) => device,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let mut buffer = [0u8; STAT_SIZE];
        encode_stat(&mut buffer, device, vnode.inode, &inode);
        // SAFETY: bounds-checked against the guest's memory.
        match unsafe { self.memory_mut().write(destination as u64, &buffer) } {
            Ok(()) => Outcome::Done(0),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    pub(crate) fn statx(&mut self, arguments: Arguments) -> Outcome {
        let flags = arguments.get(2) as i32;
        const SUPPORTED: i32 =
            at::SYMLINK_NOFOLLOW | at::EMPTY_PATH | at::NO_AUTOMOUNT | at::STATX_SYNC_TYPE;
        if flags & !SUPPORTED != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let lookup = Lookup {
            follow_final: flags & at::SYMLINK_NOFOLLOW == 0,
            require_directory: false,
        };
        let path = match self.path_at(arguments.get(1)) {
            Ok(path) => path,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let vnode = if path.is_empty() && flags & at::EMPTY_PATH != 0 {
            match self.empty_path_target(arguments.get(0)) {
                Ok(Backing::Image(vnode)) => vnode,
                Ok(Backing::Socket(id)) => {
                    let mut buffer = [0u8; STATX_SIZE];
                    let number = SOCKET_INODE_BASE + u64::from(id);
                    let queued = match self.sockets.borrow().endpoint(id) {
                        Some(endpoint) => self.rings.borrow().queued(endpoint.receive) as u64,
                        None => 0,
                    };
                    buffer[0..4].copy_from_slice(&STATX_BASIC_STATS.to_le_bytes());
                    buffer[4..8].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
                    buffer[16..20].copy_from_slice(&1u32.to_le_bytes());
                    buffer[28..30].copy_from_slice(&((file_type::SOCKET | 0o777) as u16).to_le_bytes());
                    buffer[32..40].copy_from_slice(&number.to_le_bytes());
                    buffer[40..48].copy_from_slice(&queued.to_le_bytes());
                    let (major, minor) = split_device(SOCKET_DEVICE);
                    buffer[136..140].copy_from_slice(&major.to_le_bytes());
                    buffer[140..144].copy_from_slice(&minor.to_le_bytes());
                    // SAFETY: bounds-checked against guest memory.
                    return match unsafe {
                        self.memory_mut().write(arguments.get(4) as u64, &buffer)
                    } {
                        Ok(()) => Outcome::Done(0),
                        Err(errno) => Outcome::Done(errno.as_result()),
                    };
                }
                Ok(Backing::Epoll(id)) => {
                    let mut buffer = [0u8; STATX_SIZE];
                    let number = EPOLL_INODE_BASE + u64::from(id);
                    buffer[0..4].copy_from_slice(&STATX_BASIC_STATS.to_le_bytes());
                    buffer[4..8].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
                    buffer[16..20].copy_from_slice(&1u32.to_le_bytes());
                    buffer[28..30].copy_from_slice(&0o600u16.to_le_bytes());
                    buffer[32..40].copy_from_slice(&number.to_le_bytes());
                    let (major, minor) = split_device(EPOLL_DEVICE);
                    buffer[136..140].copy_from_slice(&major.to_le_bytes());
                    buffer[140..144].copy_from_slice(&minor.to_le_bytes());
                    // SAFETY: bounds-checked against guest memory.
                    return match unsafe {
                        self.memory_mut().write(arguments.get(4) as u64, &buffer)
                    } {
                        Ok(()) => Outcome::Done(0),
                        Err(errno) => Outcome::Done(errno.as_result()),
                    };
                }
                Ok(Backing::Pipe { ring, .. }) => {
                    let mut buffer = [0u8; STATX_SIZE];
                    let queued = self.rings.borrow().queued(ring) as u64;
                    encode_pipe_statx(&mut buffer, ring, queued);
                    // SAFETY: bounds-checked against guest memory.
                    return match unsafe {
                        self.memory_mut().write(arguments.get(4) as u64, &buffer)
                    } {
                        Ok(()) => Outcome::Done(0),
                        Err(errno) => Outcome::Done(errno.as_result()),
                    };
                }
                Ok(Backing::Console(stream)) => {
                    let mut buffer = [0u8; STATX_SIZE];
                    encode_console_statx(&mut buffer, stream);
                    // SAFETY: bounds-checked against guest memory.
                    return match unsafe { self.memory_mut().write(arguments.get(4) as u64, &buffer) } {
                        Ok(()) => Outcome::Done(0),
                        Err(errno) => Outcome::Done(errno.as_result()),
                    };
                }
                Err(errno) => return Outcome::Done(errno.as_result()),
            }
        } else {
            match self.resolve_at(arguments.get(0), arguments.get(1), lookup) {
                Ok(vnode) => vnode,
                Err(errno) => return Outcome::Done(errno.as_result()),
            }
        };

        let inode = match self.vfs.inode(vnode) {
            Ok(inode) => inode,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let device = match self.vfs.device(vnode) {
            Ok(device) => device,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let mut buffer = [0u8; STATX_SIZE];
        encode_statx(&mut buffer, device, vnode.inode, &inode);
        // SAFETY: bounds-checked against the guest's memory.
        match unsafe { self.memory_mut().write(arguments.get(4) as u64, &buffer) } {
            Ok(()) => Outcome::Done(0),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    /// `getdents64`: as many whole entries as fit, and zero at the end.
    ///
    /// The offset is the entry index rather than a byte position, which is
    /// what `d_off` is allowed to be — a cookie the kernel chooses and the
    /// caller only ever hands back.
    ///
    /// `.` and `..` are reported first and are synthesized rather than
    /// stored: the index has no entries for them, because a directory's own
    /// name and its parent are already facts it carries. A real directory has
    /// them, POSIX says a directory has them, and readers assume it — so an
    /// image that omitted them would differ from every filesystem a program
    /// has ever been run against.
    pub(crate) fn getdents64(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        let buffer = arguments.get(1);
        let capacity = arguments.get(2);
        if capacity < 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }

        let start = match self.files.description(fd) {
            Ok(file) => file.offset,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let vnode = match self.image_inode(fd, Errno::NotDir) {
            Ok(vnode) => vnode,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let directory = match self.vfs.inode(vnode) {
            Ok(inode) => inode,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        if !directory.is_directory() {
            return Outcome::Done(Errno::NotDir.as_result());
        }
        let mut written = 0usize;
        // Two synthetic entries in front of the stored ones, so the cookie a
        // caller hands back indexes into `[".", "..", stored…]`.
        const SYNTHETIC: u32 = 2;
        // The offset is a cookie this kernel handed out, so one larger than
        // any it could have handed out names no entry and the listing is
        // over. Narrowing the description's 64-bit offset into the `u32` the
        // index counts in would instead wrap 4 GiB back to `.` and re-list
        // the whole directory — a reader with 64-bit cookies would never
        // reach the end.
        let Ok(mut position) = u32::try_from(start) else {
            return Outcome::Done(0);
        };
        // Where the last batch stopped, by *name*. A position is not enough
        // on its own: a merged directory's listing is computed on demand, so
        // removing an entry shifts everything after it down one and the next
        // batch would skip whatever moved into a position already consumed.
        // `rm -r` interleaves readdir and unlink and would leave files
        // behind. Resuming after a name gives back every entry that
        // survived, exactly once.
        //
        // Copied to the stack rather than borrowed, so the walk below can
        // read the filesystem without the descriptor table still being
        // borrowed — and without an allocation, which this row must not
        // make.
        let mut resume = [0u8; MAX_NAME];
        let mut resume_length = 0usize;
        let mut resuming = false;
        if start != 0
            && let Some(name) = self.files.resume(fd)
        {
            resume[..name.len()].copy_from_slice(name);
            resume_length = name.len();
            resuming = true;
        }
        let mut last_name = [0u8; MAX_NAME];
        let mut last_length = 0usize;
        // Sized from the bound above rather than a number that happens to be
        // bigger: 8 (`d_ino`) + 8 (`d_off`) + 2 (`d_reclen`) + 1 (`d_type`) +
        // the name + its terminator, rounded up to eight.
        let mut record = [0u8; (8 + 8 + 2 + 1 + MAX_NAME + 1).next_multiple_of(8)];

        // One cursor for the whole call. Asking the filesystem for the entry
        // at a position instead restarts the merge on every step, which
        // makes a listing quadratic — 35 ms for two thousand entries against
        // 0.3 ms for two hundred, measured.
        let filesystem = match self.vfs.filesystem_of(vnode) {
            Ok(filesystem) => filesystem,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let mut entries = match filesystem.entries(&directory, vnode.inode) {
            Ok(entries) => entries,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        // A fresh batch resumes at a position; one that has a name resumes
        // after it, from the top, because the position is what the change
        // invalidated.
        let mut skip = if resuming {
            0
        } else {
            position.saturating_sub(SYNTHETIC)
        };
        if resuming {
            position = SYNTHETIC;
        }

        let mut memory_error = None;
        loop {
            let (name, child, entry_type): (&[u8], u32, u8) = if position == 0 {
                (
                    b".",
                    vnode.inode,
                    crate::image::directory_entry_type::DIRECTORY,
                )
            } else if position == 1 {
                (
                    b"..",
                    directory.parent,
                    crate::image::directory_entry_type::DIRECTORY,
                )
            } else {
                let entry = match entries.next() {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    Err(errno) => return Outcome::Done(errno.as_result()),
                };
                // A merged listing does not carry the type — reading it
                // costs an inode lookup, and a walk that only needs names
                // should not pay for one per step.
                let entry_type = if entry.entry_type == crate::image::directory_entry_type::UNKNOWN
                {
                    let child = Vnode::new(vnode.mount, entry.inode);
                    match self.vfs.inode(child) {
                        Ok(inode) => crate::image::directory_entry_type::of_mode(inode.mode),
                        Err(errno) => return Outcome::Done(errno.as_result()),
                    }
                } else {
                    entry.entry_type
                };
                (entry.name, entry.inode, entry_type)
            };

            // Everything the caller has already been given: by count for a
            // batch that follows on, and by name for one that follows a
            // change.
            if position >= SYNTHETIC {
                if skip > 0 {
                    skip -= 1;
                    position += 1;
                    continue;
                }
                if resuming && name <= &resume[..resume_length] {
                    position += 1;
                    continue;
                }
            }

            // `d_reclen` is rounded to eight so the next record is aligned.
            // The record buffer is fixed, so the name has to be bounded
            // *here*, on the way out of the index — `vfs::walk` bounds what
            // goes in, and that is a different direction. A tar archive can
            // carry a name longer than `NAME_MAX`; the reader is the wrong
            // place to be trusting one.
            if name.len() > MAX_NAME {
                return Outcome::Done(Errno::NameTooLong.as_result());
            }
            let length = (8 + 8 + 2 + 1 + name.len() + 1).next_multiple_of(8);
            if (written as u64) + (length as u64) > capacity as u64 {
                // Nothing fit at all: the caller's buffer is too small for
                // even one entry, which is `EINVAL` rather than a short read.
                if written == 0 {
                    return Outcome::Done(Errno::Invalid.as_result());
                }
                break;
            }
            let record = &mut record[..length];
            record.fill(0);
            record[0..8].copy_from_slice(&visible_inode(child).to_le_bytes());
            record[8..16].copy_from_slice(&((position as i64) + 1).to_le_bytes());
            record[16..18].copy_from_slice(&(length as u16).to_le_bytes());
            record[18] = entry_type;
            record[19..19 + name.len()].copy_from_slice(name);
            // SAFETY: bounds-checked against the guest's memory.
            if let Err(errno) =
                unsafe { crate::memory::GuestMemory::new(&mut self.pages).write(buffer as u64 + written as u64, record) }
            {
                memory_error = Some(errno);
                break;
            }
            written += length;
            if position >= SYNTHETIC {
                last_name[..name.len()].copy_from_slice(name);
                last_length = name.len();
            }
            position += 1;
        }
        if let Some(errno) = memory_error {
            return Outcome::Done(errno.as_result());
        }

        self.files
            .description_mut(fd)
            .expect("the description was here a moment ago")
            .offset = position as u64;
        if last_length > 0 {
            let _ = self.files.set_resume(fd, &last_name[..last_length]);
        }
        Outcome::Done(written as i64)
    }

    pub(crate) fn readlink(&mut self, arguments: Arguments) -> Outcome {
        self.readlink_at(
            at::FDCWD,
            arguments.get(0),
            arguments.get(1),
            arguments.get(2),
        )
    }

    pub(crate) fn readlinkat(&mut self, arguments: Arguments) -> Outcome {
        self.readlink_at(
            arguments.get(0),
            arguments.get(1),
            arguments.get(2),
            arguments.get(3),
        )
    }

    fn readlink_at(&mut self, dirfd: i64, path: i64, buffer: i64, capacity: i64) -> Outcome {
        if capacity <= 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        // An empty path means the descriptor itself. `readlinkat` has meant
        // that since 2.6.39 and does *not* require `AT_EMPTY_PATH` to say
        // so — an empty path has no other meaning here, so Linux takes it
        // unconditionally. It is how a symlink held open with
        // `O_PATH|O_NOFOLLOW` is read, which is `realpath`'s inner loop.
        let text = match self.path_at(path) {
            Ok(text) => text,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let vnode = if text.is_empty() {
            match self.empty_path_target(dirfd) {
                // A console stream is not a symlink, and neither is the
                // working directory; both fall out as `EINVAL` below.
                Ok(Backing::Image(vnode)) => vnode,
                Ok(Backing::Console(_))
                | Ok(Backing::Pipe { .. })
                | Ok(Backing::Epoll(_))
                | Ok(Backing::Socket(_)) => {
                    return Outcome::Done(Errno::Invalid.as_result());
                }
                Err(errno) => return Outcome::Done(errno.as_result()),
            }
        } else {
            match self.resolve_at(dirfd, path, Lookup::NO_FOLLOW) {
                Ok(vnode) => vnode,
                Err(errno) => return Outcome::Done(errno.as_result()),
            }
        };
        let inode = match self.vfs.inode(vnode) {
            Ok(inode) => inode,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        if !inode.is_symlink() {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let target = match self
            .vfs
            .filesystem_of(vnode)
            .and_then(|filesystem| filesystem.symlink_target(&inode, vnode.inode))
        {
            Ok(target) => target,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        // A bake never writes an empty symlink target — a link with no
        // target is not something a filesystem can hold — so the only one
        // that exists is `/proc/self/exe` before anything has set the path.
        // Answering an empty string would tell the caller its own executable
        // is called "", which is worse than saying nothing is known.
        if target.is_empty() {
            return Outcome::Fault(Fault::detailed(
                number::READLINK,
                Arguments::new([dirfd, path, buffer, capacity, 0, 0]),
                "the path of the running executable, which nothing has set —                  M6's `execve` is what knows it",
            ));
        }
        // Truncated rather than refused, and no terminator: `readlink` is the
        // one call where the caller is expected to notice a full buffer and
        // ask again with a bigger one.
        let length = (target.len() as u64).min(capacity as u64) as usize;
        // SAFETY: bounds-checked against the guest's memory.
        match unsafe { crate::memory::GuestMemory::new(&mut self.pages).write(buffer as u64, &target[..length]) } {
            Ok(()) => Outcome::Done(length as i64),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    pub(crate) fn access(&mut self, arguments: Arguments) -> Outcome {
        self.access_at(at::FDCWD, arguments.get(0), arguments.get(1) as i32, 0)
    }

    /// `faccessat`, which takes **three** arguments and no flags.
    ///
    /// The libc function of that name takes four, and glibc implements the
    /// fourth by calling `faccessat2` below. The kernel's row never had one:
    /// `SYSCALL_DEFINE3(faccessat, …)`. Reading a fourth register here would
    /// be reading whatever the caller left in `%r10`, and answering `EINVAL`
    /// to a perfectly ordinary call whenever that register happened to be
    /// non-zero. Verified against this machine's kernel — `syscall(269, …,
    /// 0xdeadbeef)` answers exactly what `syscall(269, …, 0)` answers.
    pub(crate) fn faccessat(&mut self, arguments: Arguments) -> Outcome {
        self.access_at(
            arguments.get(0),
            arguments.get(1),
            arguments.get(2) as i32,
            0,
        )
    }

    fn access_at(&mut self, dirfd: i64, path: i64, mode: i32, flags: i32) -> Outcome {
        const SUPPORTED_FLAGS: i32 = at::SYMLINK_NOFOLLOW | at::EACCESS | at::EMPTY_PATH;
        if flags & !SUPPORTED_FLAGS != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        // Linux rejects a mode with bits outside `R_OK|W_OK|X_OK`. Accepting
        // one meant `access(path, 8)` answered "yes, you may", which is a
        // false success for a garbage argument.
        if mode & !(access_mode::READ | access_mode::WRITE | access_mode::EXECUTE) != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let lookup = Lookup {
            follow_final: flags & at::SYMLINK_NOFOLLOW == 0,
            require_directory: false,
        };
        // `AT_EMPTY_PATH` probes the descriptor itself, which is how a
        // program checks a file it already holds open.
        let text = match self.path_at(path) {
            Ok(text) => text,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let number = if text.is_empty() && flags & at::EMPTY_PATH != 0 {
            match self.empty_path_target(dirfd) {
                Ok(Backing::Image(inode)) => inode,
                // A console stream is a character device the guest may read
                // or write by direction. There is no inode behind it, so the
                // answer is given here rather than from the image.
                Ok(Backing::Console(stream)) => {
                    return Outcome::Done(console_access(stream, mode));
                }
                // A socket is readable and writable and never executable,
                // whichever direction it is currently able to move bytes in.
                Ok(Backing::Socket(_)) => {
                    return Outcome::Done(match mode & access_mode::EXECUTE != 0 {
                        true => Errno::Access.as_result(),
                        false => 0,
                    });
                }
                // A pipe end may be read or written by direction, and never
                // executed.
                Ok(Backing::Pipe { end, .. }) => {
                    return Outcome::Done(pipe_access(end, mode));
                }
                // An `epoll` instance is not a path and has no permissions
                // to test; `faccessat` on one is what `AT_EMPTY_PATH` makes
                // possible and Linux allows it read and write.
                Ok(Backing::Epoll(_)) => {
                    return Outcome::Done(match mode & access_mode::EXECUTE != 0 {
                        true => Errno::Access.as_result(),
                        false => 0,
                    });
                }
                Err(errno) => return Outcome::Done(errno.as_result()),
            }
        } else {
            match self.resolve_at(dirfd, path, lookup) {
                Ok(number) => number,
                Err(errno) => return Outcome::Done(errno.as_result()),
            }
        };
        // Existence and executability are answered from the image. A write
        // probe is refused with the errno that says *why* — `EROFS`, which
        // is what Linux answers on a read-only filesystem and what callers
        // branch on to fall back to a writable location. `EACCES` would send
        // them looking for a permission problem that does not exist.
        // Permission bits are preserved by the bake and deliberately not
        // enforced; that decision stays open.
        // Writability is a fact about the *mount*: the root has a writable
        // layer over the image and accepts changes, a synthetic mount has
        // none. `EROFS` rather than `EACCES` is what Linux answers on a
        // read-only filesystem, and what callers branch on to fall back
        // somewhere writable.
        //
        // A device node is exempt, exactly as it is in `open`: writing to
        // `/dev/null` does not write to the filesystem the name lives in.
        // Measured on a genuine read-only mount — `access(chardev, W_OK)`
        // succeeds there while `access(regular, W_OK)` is `EROFS` — and
        // answering otherwise would have this row contradict the row that
        // actually opens the file, which is the pair a daemonising process
        // uses in sequence.
        if mode & access_mode::WRITE != 0 {
            let inode = match self.vfs.inode(number) {
                Ok(inode) => inode,
                Err(errno) => return Outcome::Done(errno.as_result()),
            };
            if !is_device_node(&inode) && !self.is_writable(number) {
                return Outcome::Done(Errno::ReadOnlyFs.as_result());
            }
        }
        if mode & access_mode::EXECUTE != 0 {
            let inode = match self.vfs.inode(number) {
                Ok(inode) => inode,
                Err(errno) => return Outcome::Done(errno.as_result()),
            };
            if inode.mode & 0o111 == 0 {
                return Outcome::Done(Errno::Access.as_result());
            }
        }
        Outcome::Done(0)
    }

    /// `faccessat2`: `faccessat` plus the flags argument the kernel's
    /// `faccessat` never took.
    ///
    /// glibc has routed `eaccess`, `euidaccess` and `faccessat(…,
    /// AT_EACCESS)` through this since 2.33, so it is on the first line of
    /// any entrypoint shell script — `bash -c 'test -x /bin/ls'` issues it
    /// directly. Without the row it was an unimplemented syscall, which in
    /// this kernel is a loud fault that ends the container.
    pub(crate) fn faccessat2(&mut self, arguments: Arguments) -> Outcome {
        self.access_at(
            arguments.get(0),
            arguments.get(1),
            arguments.get(2) as i32,
            arguments.get(3) as i32,
        )
    }

    // ---- extended attributes -----------------------------------------

    /// The inode an xattr call names: a descriptor for the `f` forms, a path
    /// for the rest, followed for the plain forms and not for the `l` ones.
    /// A console stream carries no attributes, and answers the way a real
    /// character device does — `ENODATA` for a name, an empty list for the
    /// listing, never `ENOTSUP`. Verified against `/dev/null` on this
    /// machine: `ENOTSUP` would mean "this filesystem has no attributes at
    /// all", which is a different fact and one callers branch on.
    fn xattr_inode(&self, call: i64, first: i64) -> Result<Option<Vnode>, Errno> {
        match call {
            number::FGETXATTR | number::FLISTXATTR => {
                match self.files.description(first as i32)?.backing {
                    Backing::Image(inode) => Ok(Some(inode)),
                    Backing::Console(_)
                    | Backing::Pipe { .. }
                    | Backing::Epoll(_)
                    | Backing::Socket(_) => Ok(None),
                }
            }
            number::LGETXATTR | number::LLISTXATTR => self
                .resolve_at(at::FDCWD, first, Lookup::NO_FOLLOW)
                .map(Some),
            _ => self.resolve_at(at::FDCWD, first, Lookup::FOLLOW).map(Some),
        }
    }

    /// `getxattr`, `lgetxattr`, `fgetxattr`.
    ///
    /// The bake preserves every attribute an image carries — `security.*`,
    /// `user.*`, and the `system.posix_acl_*` entries that hold ACLs. A
    /// filesystem that stored them and could not read them back would be a
    /// filesystem that lost them, so these rows exist as soon as the baker
    /// writes the region.
    pub(crate) fn getxattr(&mut self, call: i64, arguments: Arguments) -> Outcome {
        let capacity = arguments.get(3);
        if capacity < 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let name = match self.xattr_name(arguments.get(1)) {
            Ok(name) => name,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let vnode = match self.xattr_inode(call, arguments.get(0)) {
            Ok(Some(vnode)) => vnode,
            Ok(None) => return Outcome::Done(Errno::NoData.as_result()),
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let inode = match self.vfs.inode(vnode) {
            Ok(inode) => inode,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let count = match self
            .vfs
            .filesystem_of(vnode)
            .and_then(|filesystem| filesystem.xattr_count(&inode, vnode.inode))
        {
            Ok(count) => count,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        for position in 0..count {
            let (stored, value) = match self
                .vfs
                .filesystem_of(vnode)
                .and_then(|filesystem| filesystem.xattr(&inode, vnode.inode, position))
            {
                Ok(pair) => pair,
                Err(_) => return Outcome::Done(Errno::Io.as_result()),
            };
            if stored != name {
                continue;
            }
            let length = value.len() as i64;
            // A zero capacity is the documented way to ask how big a buffer
            // to allocate, and it is what every caller does first.
            if capacity == 0 {
                return Outcome::Done(length);
            }
            if length > capacity {
                return Outcome::Done(Errno::Range.as_result());
            }
            // SAFETY: bounds-checked against the guest's memory.
            return match unsafe { crate::memory::GuestMemory::new(&mut self.pages).write(arguments.get(2) as u64, value) } {
                Ok(()) => Outcome::Done(length),
                Err(errno) => Outcome::Done(errno.as_result()),
            };
        }
        // `ENODATA` — the file exists and this attribute does not. Distinct
        // from `ENOTSUP`, which says the filesystem has no attributes at all,
        // and callers branch on the difference.
        Outcome::Done(Errno::NoData.as_result())
    }

    /// `listxattr`, `llistxattr`, `flistxattr`: the attribute names, each
    /// terminated, one after another.
    pub(crate) fn listxattr(&mut self, call: i64, arguments: Arguments) -> Outcome {
        let capacity = arguments.get(2);
        if capacity < 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let vnode = match self.xattr_inode(call, arguments.get(0)) {
            Ok(Some(vnode)) => vnode,
            Ok(None) => return Outcome::Done(0),
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let inode = match self.vfs.inode(vnode) {
            Ok(inode) => inode,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let count = match self
            .vfs
            .filesystem_of(vnode)
            .and_then(|filesystem| filesystem.xattr_count(&inode, vnode.inode))
        {
            Ok(count) => count,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        let mut total: u64 = 0;
        for position in 0..count {
            let (name, _) = match self
                .vfs
                .filesystem_of(vnode)
                .and_then(|filesystem| filesystem.xattr(&inode, vnode.inode, position))
            {
                Ok(pair) => pair,
                Err(_) => return Outcome::Done(Errno::Io.as_result()),
            };
            total += name.len() as u64 + 1;
        }
        if capacity == 0 {
            return Outcome::Done(total as i64);
        }
        if total > capacity as u64 {
            return Outcome::Done(Errno::Range.as_result());
        }
        // Written name by name rather than through a buffer of some chosen
        // size: the list has no bound this kernel gets to pick, and a fixed
        // buffer here would be one more place a long name overruns.
        let mut at = arguments.get(1) as u64;
        for position in 0..count {
            let (name, _) = match self
                .vfs
                .filesystem_of(vnode)
                .and_then(|filesystem| filesystem.xattr(&inode, vnode.inode, position))
            {
                Ok(pair) => pair,
                Err(errno) => return Outcome::Done(errno.as_result()),
            };
            // SAFETY: bounds-checked against the guest's memory.
            if let Err(errno) = unsafe { crate::memory::GuestMemory::new(&mut self.pages).write(at, name) } {
                return Outcome::Done(errno.as_result());
            }
            // SAFETY: as above, for the terminator.
            if let Err(errno) = unsafe { crate::memory::GuestMemory::new(&mut self.pages).write(at + name.len() as u64, &[0u8]) } {
                return Outcome::Done(errno.as_result());
            }
            at += name.len() as u64 + 1;
        }
        Outcome::Done(total as i64)
    }

    /// An attribute name out of guest memory, bounded by Linux's
    /// `XATTR_NAME_MAX` rather than by `PATH_MAX`.
    fn xattr_name(&self, address: i64) -> Result<&'static [u8], Errno> {
        // SAFETY: bounds-checked against the guest's memory, and refuses a
        // string with no terminator inside the bound.
        let name = match unsafe { self.memory().c_string(address as u64, XATTR_NAME_MAX + 1) } {
            Ok(name) => name,
            // No terminator within the bound means the name is longer than
            // the maximum, and `ERANGE` is the errno for that here —
            // `ENAMETOOLONG` belongs to paths. Verified against this
            // machine's kernel with a 290-byte name.
            Err(Errno::NameTooLong) => return Err(Errno::Range),
            Err(errno) => return Err(errno),
        };
        if name.is_empty() || name.len() > XATTR_NAME_MAX {
            return Err(Errno::Range);
        }
        Ok(name)
    }

    pub(crate) fn fcntl(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        let command = arguments.get(1) as i32;
        let argument = arguments.get(2);

        let result = match command {
            fcntl_command::DUPFD => self
                .files
                .duplicate(fd, argument as i32, false)
                .map(|fd| fd as i64),
            fcntl_command::DUPFD_CLOEXEC => self
                .files
                .duplicate(fd, argument as i32, true)
                .map(|fd| fd as i64),
            fcntl_command::GETFD => self
                .files
                .close_on_exec(fd)
                .map(|set| if set { FD_CLOEXEC as i64 } else { 0 }),
            fcntl_command::SETFD => self
                .files
                .set_close_on_exec(fd, argument as i32 & FD_CLOEXEC != 0)
                .map(|()| 0),
            fcntl_command::GETFL => self
                .files
                .description(fd)
                .map(|file| (file.flags | O_LARGEFILE) as i64),
            // The status flags a caller may change are the per-description
            // ones; the access mode and the creation flags are fixed at open,
            // and Linux silently ignores an attempt to change them.
            fcntl_command::SETFL => {
                const CHANGEABLE: i32 = open_flags::APPEND | open_flags::NONBLOCK;
                self.files.description_mut(fd).map(|file| {
                    file.flags = (file.flags & !CHANGEABLE) | (argument as i32 & CHANGEABLE);
                    0
                })
            }
            // Commands Linux implements and this does not. `EINVAL` here
            // would be a lie about them — a caller reads it as "this kernel
            // has no such command" and carries on. `Fault::detailed` exists
            // for exactly this: the syscall is implemented, one of its
            // operations is not, and the worklist should say which.
            fcntl_command::GETLK
            | fcntl_command::SETLK
            | fcntl_command::SETLKW
            | fcntl_command::OFD_GETLK
            | fcntl_command::OFD_SETLK
            | fcntl_command::OFD_SETLKW => {
                return Outcome::Fault(Fault::detailed(
                    number::FCNTL,
                    arguments,
                    "record locks, which the design places in kisal as in-guest state",
                ));
            }
            fcntl_command::SETOWN | fcntl_command::GETOWN | fcntl_command::GETOWN_EX => {
                return Outcome::Fault(Fault::detailed(
                    number::FCNTL,
                    arguments,
                    "descriptor ownership, which needs signals",
                ));
            }
            // Genuinely unknown to Linux too, where `EINVAL` is the answer.
            _ => Err(Errno::Invalid),
        };
        Outcome::Done(match result {
            Ok(value) => value,
            Err(errno) => errno.as_result(),
        })
    }

    /// `close_range(2)`: shut a whole span of descriptors, closed or not.
    ///
    /// The call every `fork`-and-`exec` reaches for. Before it existed a
    /// child had to walk `/proc/self/fd` or guess an upper bound and call
    /// `close` a thousand times; CPython's `_posixsubprocess` uses this
    /// where it can and falls back to the walk where it cannot, so a kernel
    /// that refuses it works and is slower — which makes it exactly the kind
    /// of call that is worth having rather than faulting on.
    ///
    /// **A descriptor that is not open is not an error.** That is the whole
    /// ergonomic point: the caller is saying "none of these, whatever they
    /// were", and having to know which ones existed would defeat it.
    pub(crate) fn close_range(&mut self, arguments: Arguments) -> Outcome {
        /// `CLOSE_RANGE_UNSHARE`: give this process its own descriptor table
        /// first. Nothing here shares one — a fork copies it, which is the
        /// only sharing Linux has and is what this flag undoes — so the flag
        /// is satisfied by construction rather than ignored.
        const UNSHARE: u32 = 1 << 1;
        /// `CLOSE_RANGE_CLOEXEC`: mark them instead of closing them.
        const CLOEXEC: u32 = 1 << 2;

        let first = arguments.get(0) as u32;
        let last = arguments.get(1) as u32;
        let flags = arguments.get(2) as u32;
        if first > last || flags & !(UNSHARE | CLOEXEC) != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        // `last` is routinely `UINT_MAX`, which is how a caller says "and
        // everything above". Walking to four billion would be a hang, so the
        // span is clamped to what the table actually holds.
        let ceiling = match self.files.highest() {
            Some(highest) => (highest as u32).min(last),
            None => return Outcome::Done(0),
        };
        self.close_span(first as i32, ceiling as i32, flags & CLOEXEC != 0);
        Outcome::Done(0)
    }

    /// Closes — or marks — every open descriptor in a span.
    ///
    /// Shared by `close_range` and by a process ending, which are the same
    /// operation over different spans: a process that exits lets go of
    /// everything, and it has to, because a zombie is a status and a process
    /// id and *not* an open descriptor. Leaving the table intact leaves a
    /// pipe's writer count standing, and the parent's `poll` on the other
    /// end waits forever for an end-of-file that already happened.
    pub(crate) fn close_span(&mut self, first: i32, last: i32, cloexec: bool) {
        let before = self.shared_census();
        for fd in first..=last {
            if !self.files.is_open(fd) {
                continue;
            }
            if cloexec {
                let _ = self.files.set_close_on_exec(fd, true);
                continue;
            }
            // The same accounting `close` does, and for the same reason: an
            // unlinked file is freed when its last descriptor goes.
            let held = match self.files.description(fd) {
                Ok(file) => match file.backing {
                    Backing::Image(vnode) => Some(vnode),
                    _ => None,
                },
                Err(_) => None,
            };
            let had = self.files.description_index(fd).ok();
            if self.files.close(fd).is_ok() {
                self.reclaim_after_close(held);
                self.forget_description(had);
            }
        }
        self.reconcile_shared(&before);
    }

    /// Everything a process holds, let go of — which is what ending does.
    pub fn relinquish(&mut self) {
        if let Some(highest) = self.files.highest() {
            self.close_span(0, highest, false);
        }
    }

    /// `fadvise64(2)`: what the caller expects to do with a file next.
    ///
    /// Advice, and the kernel is free to do nothing with it — which is what
    /// this one does, because there is no page cache to manage: the image's
    /// bytes are already in linear memory and there is nothing to read ahead
    /// or drop. Answering zero is the honest reply, not a stub: Linux's own
    /// answer for a filesystem with no `fadvise` operation is also zero.
    ///
    /// The *checks* are not skipped, because they are the whole of what a
    /// program can observe here. A bad descriptor is `EBADF`, an advice this
    /// kernel does not know is `EINVAL`, and a pipe is `ESPIPE` — a call
    /// that cheerfully accepted all three would be a call that hides a bug
    /// in whatever asked.
    pub(crate) fn fadvise(&mut self, arguments: Arguments) -> Outcome {
        /// `POSIX_FADV_NORMAL` through `NOREUSE`, in Linux's order.
        const HIGHEST: i64 = 5;
        let fd = arguments.get(0) as i32;
        let advice = arguments.get(3);
        let Ok(file) = self.files.description(fd) else {
            return Outcome::Done(Errno::BadFile.as_result());
        };
        if !(0..=HIGHEST).contains(&advice) {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        match file.backing {
            Backing::Pipe { .. } => Outcome::Done(Errno::NotSeekable.as_result()),
            _ => Outcome::Done(0),
        }
    }

    /// `statfs(2)` and `fstatfs(2)`: what filesystem is this, and how much
    /// room is on it.
    ///
    /// Both questions are answered from what is actually true here rather
    /// than with plausible constants, because both are branched on. `ls`
    /// reads the type to decide whether `d_type` can be trusted; anything
    /// that writes reads the free count before it starts.
    ///
    /// The type is `OVERLAYFS_SUPER_MAGIC`, and it is not a costume: a
    /// container's root *is* a read-only image with a writable layer over
    /// it, which is the filesystem overlayfs describes. Saying `ext4` would
    /// be a claim that unlinking a file in the lower layer frees space, and
    /// it does not.
    ///
    /// The room is the guest's address space, because that is where the
    /// writable layer lives. A container that fills its overlay stops
    /// because linear memory ran out, so the number a program reads before
    /// writing should be the number that will stop it.
    pub(crate) fn statfs(&mut self, number: i64, arguments: Arguments) -> Outcome {
        /// `OVERLAYFS_SUPER_MAGIC`.
        const OVERLAY: u64 = 0x794c_7630;
        /// `struct statfs` on x86-64.
        const SIZE: usize = 120;
        /// `ST_VALID`, which Linux sets to say `f_flags` was filled in at
        /// all — without it a caller ignores the field.
        const VALID: u64 = 0x0020;

        let destination = match number == number::FSTATFS {
            // `fstatfs` takes a descriptor, and every descriptor this kernel
            // has is on the one filesystem — but a closed one is still
            // `EBADF`, which is the whole of what it can tell you apart.
            true => {
                if self.files.description(arguments.get(0) as i32).is_err() {
                    return Outcome::Done(Errno::BadFile.as_result());
                }
                arguments.get(1)
            }
            false => {
                let root = self.vfs.root();
                let Ok(path) = self.path_at(arguments.get(0)) else {
                    return Outcome::Done(Errno::Fault.as_result());
                };
                if let Err(errno) = self.vfs.resolve(root, path, Lookup::FOLLOW) {
                    return Outcome::Done(errno.as_result());
                }
                arguments.get(1)
            }
        };
        let used = self.machine.memory_limit();
        let free = targum::space::CEILING.saturating_sub(used);
        let blocks = (used + free) / BLOCK_SIZE as u64;
        let available = free / BLOCK_SIZE as u64;
        let inodes = match self.vfs.filesystem_of(self.vfs.root()) {
            Ok(filesystem) => u64::from(filesystem.lower().inode_count()),
            Err(_) => 0,
        };

        let mut bytes = [0u8; SIZE];
        let mut put = |at: usize, value: u64| {
            bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
        };
        put(0, OVERLAY);
        put(8, BLOCK_SIZE as u64);
        put(16, blocks);
        put(24, available);
        put(32, available);
        put(40, inodes);
        // A new file needs a block for its contents and a record for its
        // name, so the number of files that can still be made is bounded by
        // the same memory the blocks are.
        put(48, available);
        // `f_fsid` is two words and identifies the filesystem across a
        // `mount`; there is one here and it does not move.
        put(56, 0);
        put(64, MAX_NAME as u64);
        put(72, BLOCK_SIZE as u64);
        put(80, VALID);

        // SAFETY: bounds-checked by the write itself.
        match unsafe { self.memory_mut().write(destination as u64, &bytes) } {
            Ok(()) => Outcome::Done(0),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    pub(crate) fn dup(&mut self, arguments: Arguments) -> Outcome {
        let before = self.shared_census();
        let answer = match self.files.duplicate(arguments.get(0) as i32, 0, false) {
            Ok(fd) => fd as i64,
            Err(errno) => errno.as_result(),
        };
        self.reconcile_shared(&before);
        Outcome::Done(answer)
    }

    /// What this process's descriptors point at that other processes can
    /// also point at — once per *descriptor*, because that is the unit both
    /// counts are kept in.
    pub(crate) fn shared_census(&self) -> Census {
        // Ring ends from pipes come straight off the table; a socket's have
        // to be asked of the arena, because a socket acquires its rings at
        // `connect` or `accept` and its descriptor's backing does not
        // change. A direction already given up by `shutdown` contributes
        // nothing, which is what keeps a census from handing back a
        // reference the guest deliberately dropped.
        let mut rings: Vec<(u32, crate::ring::End)> = self.files.pipe_ends().collect();
        let sockets: Vec<u32> = self.files.socket_ids().collect();
        if !sockets.is_empty() {
            let arena = self.sockets.borrow();
            for id in &sockets {
                let Some(endpoint) = arena.endpoint(*id) else {
                    continue;
                };
                if !endpoint.read_shut {
                    rings.push((endpoint.receive, crate::ring::End::Read));
                }
                if !endpoint.write_shut {
                    rings.push((endpoint.transmit, crate::ring::End::Write));
                }
            }
        }
        Census {
            pipes: rings,
            sockets,
            epolls: self.files.epoll_sets().collect(),
        }
    }

    /// Drops an `epoll` registration whose open file description has gone.
    ///
    /// Linux's `eventpoll_release`, and it is not tidiness: a description's
    /// slot is *reused*, so a registration left behind would start watching
    /// whichever file opened next and report it under the old caller's data
    /// word.
    ///
    /// Called with the index the descriptor *had*, at the one place a
    /// description can die — rather than by comparing the table before and
    /// after, which is the same answer and allocates a vector of every open
    /// descriptor on every `close`. `tests/filesystem.rs` measures that the
    /// syscall path allocates nothing, and it is right to.
    pub(crate) fn forget_description(&mut self, had: Option<usize>) {
        if let Some(index) = had
            && self.files.at(index).is_none()
        {
            self.epolls.borrow_mut().forget(index);
        }
    }

    /// Moves the pipe reference counts to match a descriptor table that has
    /// just changed.
    ///
    /// A pipe's readers and writers are counted in descriptors, so `dup`,
    /// `dup2` and `dup3` all change them — `dup2` in both directions at
    /// once, since it closes whatever the target was first. Four call sites
    /// each remembering to adjust the right count in the right direction is
    /// four places to get it wrong, and getting it wrong does not corrupt
    /// anything: it hangs, or it reports end-of-file to a reader whose
    /// writer is still there. So it is a difference of two censuses instead.
    pub(crate) fn reconcile_shared(&mut self, before: &Census) {
        let after = self.shared_census();
        {
            let mut rings = self.rings.borrow_mut();
            for held in distinct(&before.pipes, &after.pipes) {
                let was = count(&before.pipes, &held);
                let now = count(&after.pipes, &held);
                for _ in was..now {
                    rings.acquire(held.0, held.1);
                }
                for _ in now..was {
                    rings.release(held.0, held.1);
                }
            }
        }
        {
            let mut arena = self.sockets.borrow_mut();
            for held in distinct(&before.sockets, &after.sockets) {
                let was = count(&before.sockets, &held);
                let now = count(&after.sockets, &held);
                for _ in was..now {
                    arena.acquire(held);
                }
                for _ in now..was {
                    arena.release(held);
                }
            }
        }
        let mut epolls = self.epolls.borrow_mut();
        for held in distinct(&before.epolls, &after.epolls) {
            let was = count(&before.epolls, &held);
            let now = count(&after.epolls, &held);
            for _ in was..now {
                epolls.acquire(held);
            }
            for _ in now..was {
                epolls.release(held);
            }
        }
    }

    pub(crate) fn dup2(&mut self, arguments: Arguments) -> Outcome {
        let old = arguments.get(0) as i32;
        let new = arguments.get(1) as i32;
        // `dup2` validates the source even when it changes nothing.
        if old == new {
            return Outcome::Done(if self.files.is_open(old) {
                new as i64
            } else {
                Errno::BadFile.as_result()
            });
        }
        let before = self.shared_census();
        // `dup2` closes whatever the target was, which can be the last
        // descriptor naming its description.
        let displaced = self.files.description_index(new).ok();
        let answer = match self.files.duplicate_to(old, new, false) {
            Ok(fd) => fd as i64,
            Err(errno) => errno.as_result(),
        };
        self.reconcile_shared(&before);
        self.forget_description(displaced);
        Outcome::Done(answer)
    }

    pub(crate) fn dup3(&mut self, arguments: Arguments) -> Outcome {
        let old = arguments.get(0) as i32;
        let new = arguments.get(1) as i32;
        let flags = arguments.get(2) as i32;
        // The one place `dup3` differs from `dup2` other than the flag: equal
        // descriptors are `EINVAL`, because the no-op would silently discard
        // the `O_CLOEXEC` the caller asked for.
        if old == new {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        if flags & !open_flags::CLOEXEC != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let before = self.shared_census();
        let displaced = self.files.description_index(new).ok();
        let answer = match self
            .files
            .duplicate_to(old, new, flags & open_flags::CLOEXEC != 0)
        {
            Ok(fd) => fd as i64,
            Err(errno) => errno.as_result(),
        };
        self.reconcile_shared(&before);
        self.forget_description(displaced);
        Outcome::Done(answer)
    }

    pub(crate) fn getcwd(&mut self, arguments: Arguments) -> Outcome {
        let buffer = arguments.get(0);
        // `size` is `unsigned long` on Linux, so there is no negative case to
        // reject: a buffer too small — including one of zero — is `ERANGE`,
        // and gnulib's replacement `getcwd` probes with small sizes and
        // treats anything other than `ERANGE` as fatal.
        let capacity = arguments.get(1) as u64;

        // One byte of headroom for the terminator, so a path of exactly
        // `PATH_MAX` bytes has somewhere to put it instead of running off the
        // end of the buffer.
        let mut path = [0u8; PATH_MAX + 1];
        let length = match self
            .vfs
            .absolute_path(self.vfs.working_directory(), &mut path[..PATH_MAX])
        {
            Ok(length) => length,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        // The terminator counts against the caller's buffer, and the return
        // value includes it — `getcwd` is the odd one that reports a length
        // with the NUL in it.
        if (length as u64) + 1 > capacity {
            return Outcome::Done(Errno::Range.as_result());
        }
        path[length] = 0;
        // SAFETY: bounds-checked against the guest's memory.
        match unsafe { self.memory_mut().write(buffer as u64, &path[..length + 1]) } {
            Ok(()) => Outcome::Done((length + 1) as i64),
            Err(errno) => Outcome::Done(errno.as_result()),
        }
    }

    pub(crate) fn chdir(&mut self, arguments: Arguments) -> Outcome {
        let number = match self.resolve_at(
            at::FDCWD,
            arguments.get(0),
            Lookup {
                follow_final: true,
                require_directory: true,
            },
        ) {
            Ok(number) => number,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        Outcome::Done(match self.vfs.set_working_directory(number) {
            Ok(()) => 0,
            Err(errno) => errno.as_result(),
        })
    }

    pub(crate) fn fchdir(&mut self, arguments: Arguments) -> Outcome {
        let fd = arguments.get(0) as i32;
        let inode = match self.image_inode(fd, Errno::NotDir) {
            Ok(inode) => inode,
            Err(errno) => return Outcome::Done(errno.as_result()),
        };
        Outcome::Done(match self.vfs.set_working_directory(inode) {
            Ok(()) => 0,
            Err(errno) => errno.as_result(),
        })
    }
}

/// How many 512-byte blocks a file of this size occupies — what `st_blocks`
/// counts, and `du` reads.
fn blocks_of(size: u64) -> i64 {
    size.div_ceil(512) as i64
}

/// The inode number the guest sees.
///
/// One more than the index's, because index zero is a perfectly good inode
/// and `st_ino == 0` is what a lot of code uses to mean "no file".
fn visible_inode(number: u32) -> u64 {
    number as u64 + 1
}

fn encode_stat(into: &mut [u8; STAT_SIZE], device: u64, number: u32, inode: &Inode) {
    let rdev = if matches!(inode.file_type(), file_type::CHARACTER | file_type::BLOCK) {
        inode.payload
    } else {
        0
    };
    into[0..8].copy_from_slice(&device.to_le_bytes());
    into[8..16].copy_from_slice(&visible_inode(number).to_le_bytes());
    into[16..24].copy_from_slice(&(inode.nlink as u64).to_le_bytes());
    into[24..28].copy_from_slice(&inode.mode.to_le_bytes());
    into[28..32].copy_from_slice(&inode.uid.to_le_bytes());
    into[32..36].copy_from_slice(&inode.gid.to_le_bytes());
    into[36..40].copy_from_slice(&0u32.to_le_bytes());
    into[40..48].copy_from_slice(&rdev.to_le_bytes());
    into[48..56].copy_from_slice(&(inode.size as i64).to_le_bytes());
    into[56..64].copy_from_slice(&BLOCK_SIZE.to_le_bytes());
    into[64..72].copy_from_slice(&blocks_of(inode.size).to_le_bytes());
    // Tar carries no access or change time, so the bake has none to preserve
    // and all three report the modification time. The format does not pretend
    // otherwise, and neither does this.
    for offset in [72, 88, 104] {
        into[offset..offset + 8].copy_from_slice(&inode.mtime_sec.to_le_bytes());
        into[offset + 8..offset + 16].copy_from_slice(&(inode.mtime_nsec as u64).to_le_bytes());
    }
}

/// A console stream's `struct stat`: a character device, as it is.
/// The device every pipe reports, and the base its inode numbers come from.
///
/// Distinct from the console's and from the image's, because two files with
/// the same device and inode are the *same file* to anything that compares
/// them — and a program that opens two pipes and stats both would otherwise
/// be told they are one.
const PIPE_DEVICE: u64 = 15;
const PIPE_INODE_BASE: u64 = 0x9000_0000;

/// And sockets, on a third.
const SOCKET_DEVICE: u64 = 17;
const SOCKET_INODE_BASE: u64 = 0x9200_0000;

/// The same for `epoll` instances, which are on a different anonymous
/// filesystem again — measured on this machine, where a pipe reports device
/// 15 and an `epoll` descriptor reports 16.
const EPOLL_DEVICE: u64 = 16;
const EPOLL_INODE_BASE: u64 = 0x9100_0000;

/// A snapshot of the shared things a descriptor table points at. See
/// [`crate::syscall::Kernel::reconcile_shared`].
#[derive(Clone, Debug, Default)]
/// Deliberately only the *shared* things, which a container that has opened
/// no pipe and no `epoll` has none of — so both vectors are empty and
/// neither allocates. What is per-process is handled where it happens; see
/// [`crate::syscall::Kernel::forget_description`].
pub struct Census {
    /// Every ring end the table holds, from pipes and sockets alike — which
    /// is why the field outlived its name.
    pipes: Vec<(u32, crate::ring::End)>,
    sockets: Vec<u32>,
    epolls: Vec<u32>,
}

/// Each value that appears in either list, once.
pub(crate) fn distinct<T: Copy + PartialEq>(before: &[T], after: &[T]) -> Vec<T> {
    let mut seen: Vec<T> = Vec::new();
    for value in before.iter().chain(after.iter()).copied() {
        if !seen.contains(&value) {
            seen.push(value);
        }
    }
    seen
}

pub(crate) fn count<T: PartialEq>(list: &[T], value: &T) -> usize {
    list.iter().filter(|held| *held == value).count()
}

fn encode_pipe_stat(into: &mut [u8; STAT_SIZE], ring: u32, queued: u64) {
    let number = PIPE_INODE_BASE + u64::from(ring);
    into[0..8].copy_from_slice(&PIPE_DEVICE.to_le_bytes());
    into[8..16].copy_from_slice(&number.to_le_bytes());
    into[16..24].copy_from_slice(&1u64.to_le_bytes());
    into[24..28].copy_from_slice(&(file_type::FIFO | 0o600).to_le_bytes());
    into[48..56].copy_from_slice(&queued.to_le_bytes());
    into[56..64].copy_from_slice(&BLOCK_SIZE.to_le_bytes());
}

fn encode_pipe_statx(into: &mut [u8; STATX_SIZE], ring: u32, queued: u64) {
    let number = PIPE_INODE_BASE + u64::from(ring);
    into[0..4].copy_from_slice(&STATX_BASIC_STATS.to_le_bytes());
    into[4..8].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
    into[16..20].copy_from_slice(&1u32.to_le_bytes());
    into[28..30].copy_from_slice(&((file_type::FIFO | 0o600) as u16).to_le_bytes());
    into[32..40].copy_from_slice(&number.to_le_bytes());
    into[40..48].copy_from_slice(&queued.to_le_bytes());
    let (major, minor) = split_device(PIPE_DEVICE);
    into[136..140].copy_from_slice(&major.to_le_bytes());
    into[140..144].copy_from_slice(&minor.to_le_bytes());
}

/// What `faccessok` answers for a pipe end.
fn pipe_access(end: crate::ring::End, mode: i32) -> i64 {
    let readable = end == crate::ring::End::Read;
    if mode & access_mode::READ != 0 && !readable {
        return Errno::Access.as_result();
    }
    if mode & access_mode::WRITE != 0 && readable {
        return Errno::Access.as_result();
    }
    if mode & access_mode::EXECUTE != 0 {
        return Errno::Access.as_result();
    }
    0
}

fn encode_console_stat(into: &mut [u8; STAT_SIZE], stream: Console) {
    let number = CONSOLE_INODE_BASE + stream as u64;
    into[0..8].copy_from_slice(&CONSOLE_DEVICE.to_le_bytes());
    into[8..16].copy_from_slice(&number.to_le_bytes());
    into[16..24].copy_from_slice(&1u64.to_le_bytes());
    into[24..28].copy_from_slice(&(file_type::CHARACTER | 0o620).to_le_bytes());
    into[40..48].copy_from_slice(&CONSOLE_RDEV.to_le_bytes());
    into[56..64].copy_from_slice(&BLOCK_SIZE.to_le_bytes());
}

fn encode_console_statx(into: &mut [u8; STATX_SIZE], stream: Console) {
    let number = CONSOLE_INODE_BASE + stream as u64;
    into[0..4].copy_from_slice(&STATX_BASIC_STATS.to_le_bytes());
    into[4..8].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
    into[16..20].copy_from_slice(&1u32.to_le_bytes());
    into[28..30].copy_from_slice(&((file_type::CHARACTER | 0o620) as u16).to_le_bytes());
    into[32..40].copy_from_slice(&number.to_le_bytes());
    let (major, minor) = split_device(CONSOLE_RDEV);
    into[128..132].copy_from_slice(&major.to_le_bytes());
    into[132..136].copy_from_slice(&minor.to_le_bytes());
    let (major, minor) = split_device(CONSOLE_DEVICE);
    into[136..140].copy_from_slice(&major.to_le_bytes());
    into[140..144].copy_from_slice(&minor.to_le_bytes());
}

/// Linux's `dev_t` decomposition, which is not a simple byte split:
/// `major = (dev & 0xfff00) >> 8`, `minor = (dev & 0xff) | ((dev >> 12) & 0xfff00)`.
/// Getting it wrong is invisible for the small device numbers a test uses and
/// wrong for every minor number above 255.
fn split_device(device: u64) -> (u32, u32) {
    let major = ((device & 0x0000_0000_000f_ff00) >> 8) | ((device >> 32) & 0xffff_f000);
    let minor = (device & 0xff) | ((device >> 12) & 0xffff_ff00);
    (major as u32, minor as u32)
}

/// What an `access` probe on a console stream answers. Existence is yes;
/// nothing on a console is executable; readability and writability follow
/// the direction the stream actually has, because answering "yes" to both
/// would tell a program it can read the output it is writing to.
fn console_access(stream: Console, mode: i32) -> i64 {
    let readable = matches!(stream, Console::Input);
    if mode & access_mode::READ != 0 && !readable {
        return Errno::Access.as_result();
    }
    if mode & access_mode::WRITE != 0 && readable {
        return Errno::Access.as_result();
    }
    if mode & access_mode::EXECUTE != 0 {
        return Errno::Access.as_result();
    }
    0
}

fn encode_statx(into: &mut [u8; STATX_SIZE], device: u64, number: u32, inode: &Inode) {
    into[0..4].copy_from_slice(&STATX_BASIC_STATS.to_le_bytes());
    into[4..8].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
    into[16..20].copy_from_slice(&inode.nlink.to_le_bytes());
    into[20..24].copy_from_slice(&inode.uid.to_le_bytes());
    into[24..28].copy_from_slice(&inode.gid.to_le_bytes());
    into[28..30].copy_from_slice(&(inode.mode as u16).to_le_bytes());
    into[32..40].copy_from_slice(&visible_inode(number).to_le_bytes());
    into[40..48].copy_from_slice(&inode.size.to_le_bytes());
    into[48..56].copy_from_slice(&(blocks_of(inode.size) as u64).to_le_bytes());
    // atime, ctime, mtime — the same three-way answer as `stat`, because the
    // image stores one timestamp: an OCI layer is a tar archive, and tar
    // carries `mtime`.
    //
    // Birth time is left out, and out of the mask. It is the one field
    // `statx` exists to add over `stat`, a caller asks for it *because*
    // `stat` could not answer, and the mask is how it finds out whether it
    // got one. Writing the modification time there and advertising it would
    // be a plausible wrong answer to the only question this call answers
    // that `stat` does not.
    for offset in [64, 96, 112] {
        into[offset..offset + 8].copy_from_slice(&inode.mtime_sec.to_le_bytes());
        into[offset + 8..offset + 12].copy_from_slice(&inode.mtime_nsec.to_le_bytes());
    }
    if matches!(inode.file_type(), file_type::CHARACTER | file_type::BLOCK) {
        let (major, minor) = split_device(inode.payload);
        into[128..132].copy_from_slice(&major.to_le_bytes());
        into[132..136].copy_from_slice(&minor.to_le_bytes());
    }
    let (major, minor) = split_device(device);
    into[136..140].copy_from_slice(&major.to_le_bytes());
    into[140..144].copy_from_slice(&minor.to_le_bytes());
}

/// Asserted so that a change to the descriptor limit cannot silently make
/// `dup2` to a high number succeed where Linux would refuse it.
const _: () = assert!(MAX_DESCRIPTORS >= 256);
