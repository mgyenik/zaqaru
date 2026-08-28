//! The read-only filesystem, driven through the syscall rows against a real
//! baked image, natively.
//!
//! Everything here would otherwise need a transpiled guest, a link and an
//! instantiation to check. None of it needs emulation to be *decided* — the
//! rows are ordinary Rust over ordinary bytes — so all of it is decided here,
//! in milliseconds, and emulation is spent on what only emulation can show.
//!
//! Guest addresses are real addresses into an arena the test owns, with the
//! kernel's memory limit set to that arena's end. That is not a shortcut past
//! the bounds checking: it is the same checking, against a smaller memory.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use kisal::abi::{Store, StoreOutcome};
use kisal::errno::Errno;
use kisal::file::{
    O_LARGEFILE, PATH_MAX, STAT_SIZE, STATX_SIZE, access_mode, at, fcntl_command, open_flags,
    seek, FD_CLOEXEC,
};
use kisal::machine::Registers;
use kisal::syscall::{Arguments, Kernel, Outcome, number};
use kisal::vfs::Lookup;


// ---- fixtures --------------------------------------------------------------

struct Tree {
    root: PathBuf,
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn tree(label: &str) -> Tree {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("kisal-fs-{label}-{unique}"));
    std::fs::create_dir_all(root.join("etc")).expect("mkdir");
    std::fs::create_dir_all(root.join("usr/lib")).expect("mkdir");
    std::fs::create_dir_all(root.join("empty")).expect("mkdir");
    // The mount points a base image ships and the kernel fills in: on a real
    // system both are mounts, and mounting needs somewhere to mount over.
    std::fs::create_dir_all(root.join("dev")).expect("mkdir");
    std::fs::create_dir_all(root.join("proc")).expect("mkdir");

    write(&root.join("etc/hosts"), b"127.0.0.1 localhost\n");
    write(&root.join("etc/hostname"), b"courtyard\n");
    write(&root.join("usr/lib/libthing.so"), b"\x7fELFcontents");
    write(&root.join("script"), b"#!/bin/sh\necho hi\n");
    std::fs::set_permissions(root.join("script"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");

    // One file under two names, as every base image has: busybox's applets
    // are all the same binary.
    write(&root.join("twin-a"), b"twins\n");
    std::fs::hard_link(root.join("twin-a"), root.join("twin-b")).expect("link");

    std::os::unix::fs::symlink("usr/lib", root.join("lib")).expect("symlink");
    std::os::unix::fs::symlink("etc/hosts", root.join("hosts-link")).expect("symlink");
    std::os::unix::fs::symlink("/etc/hostname", root.join("absolute-link")).expect("symlink");
    // A link whose target does not exist. Every distinction between "the
    // link" and "what it names" is invisible unless the two answer
    // differently, and a dangling link is the cheapest way to make them.
    std::os::unix::fs::symlink("/nowhere/at/all", root.join("dangling")).expect("symlink");
    // A pair that point at each other, which is the shape `ELOOP` exists for.
    std::os::unix::fs::symlink("loop-b", root.join("loop-a")).expect("symlink");
    std::os::unix::fs::symlink("loop-a", root.join("loop-b")).expect("symlink");
    // A link into a directory, so a path can traverse *through* one.
    std::os::unix::fs::symlink("../etc", root.join("usr/etc-link")).expect("symlink");

    // A directory, a symlink and a fifo inside a directory that gets listed,
    // so `getdents64` reports more than one `d_type`. With only regular files
    // in view, a translation that hardcoded `DT_REG` would be indistinguish-
    // able from one that read the image.
    std::fs::create_dir_all(root.join("etc/conf.d")).expect("mkdir");
    std::os::unix::fs::symlink("hosts", root.join("etc/hosts-alias")).expect("symlink");
    let fifo = std::ffi::CString::new(root.join("etc/pipe").as_os_str().as_bytes())
        .expect("path");
    assert_eq!(
        unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) },
        0,
        "the fixture filesystem cannot hold a fifo: {}",
        std::io::Error::last_os_error()
    );

    // A chain of symlinks laid end to end, for the traversal limit. Linux
    // counts total traversals per resolution, not nesting depth, so a long
    // flat chain is the shape that tells the two apart.
    for step in 0..50 {
        let target = if step == 0 {
            "etc/hosts".to_string()
        } else {
            format!("chain{}", step - 1)
        };
        std::os::unix::fs::symlink(target, root.join(format!("chain{step}"))).expect("symlink");
    }

    // Extended attributes on a file the tests can reach. The bake preserves
    // them; these rows are what lets a guest read them back.
    set_xattr(&root.join("etc/hosts"), b"user.origin", b"the bake");
    set_xattr(&root.join("etc/hosts"), b"user.empty", b"");

    Tree { root }
}

fn set_xattr(path: &Path, name: &[u8], value: &[u8]) {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
    let name = std::ffi::CString::new(name).expect("name");
    let result = unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    assert_eq!(
        result,
        0,
        "the fixture filesystem cannot hold an xattr: {}",
        std::io::Error::last_os_error()
    );
}

/// A second, distinct tree, for mounting over a directory of the first. Its
/// contents share no name with the covering tree, so a test that says "this
/// came from the mount" cannot be satisfied by the filesystem underneath.
fn mounted_tree(label: &str) -> Tree {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("kisal-mnt-{label}-{unique}"));
    std::fs::create_dir_all(root.join("inner")).expect("mkdir");
    write(&root.join("payload"), b"from the mounted filesystem\n");
    write(&root.join("inner/deep"), b"deeper\n");
    std::os::unix::fs::symlink("../payload", root.join("inner/up")).expect("symlink");
    Tree { root }
}

fn write(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::File::create(path).expect("create");
    file.write_all(bytes).expect("write");
}

/// A store with a console behind it: standard input answers bytes, and
/// output and error record what was written.
#[derive(Default)]
struct ConsoleStore {
    pub input: Vec<u8>,
    pub written: Vec<(Vec<Vec<u8>>, Vec<u8>)>,
    /// The boot seed, or `None` for a container the host gave no entropy.
    pub seed: Option<[u8; 32]>,
}

impl ConsoleStore {
    fn contents(&self, path: &[&[u8]]) -> Vec<u8> {
        let key: Vec<Vec<u8>> = path.iter().map(|part| part.to_vec()).collect();
        self.written
            .iter()
            .filter(|(written, _)| *written == key)
            .flat_map(|(_, bytes)| bytes.iter().copied())
            .collect()
    }
}

impl Store for ConsoleStore {
    fn read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> StoreOutcome {
        if path == kisal::paths::CONSOLE_STDIN && !self.input.is_empty() {
            into.extend_from_slice(&self.input);
            return StoreOutcome::Present;
        }
        if path == kisal::paths::RANDOM_SEED
            && let Some(seed) = self.seed
        {
            into.extend_from_slice(&seed);
            return StoreOutcome::Present;
        }
        StoreOutcome::Absent
    }
    fn write(&mut self, path: &[&[u8]], data: &[u8]) -> StoreOutcome {
        self.written.push((
            path.iter().map(|part| part.to_vec()).collect(),
            data.to_vec(),
        ));
        StoreOutcome::Present
    }
}

/// The guest's memory, as far as these tests are concerned: one buffer, with
/// the kernel's limit set to its end so that every bounds check is real.
struct Arena {
    bytes: Box<[u8]>,
    used: usize,
}

impl Arena {
    fn new() -> Self {
        Self {
            bytes: vec![0u8; 64 * 1024].into_boxed_slice(),
            used: 0,
        }
    }

    fn limit(&self) -> u64 {
        self.bytes.as_ptr() as usize as u64 + self.bytes.len() as u64
    }

    fn place(&mut self, bytes: &[u8]) -> i64 {
        let start = self.used.next_multiple_of(8);
        assert!(start + bytes.len() <= self.bytes.len(), "arena exhausted");
        self.bytes[start..start + bytes.len()].copy_from_slice(bytes);
        self.used = start + bytes.len();
        self.bytes.as_ptr() as usize as i64 + start as i64
    }

    /// A NUL-terminated path, as a guest would pass one.
    fn path(&mut self, path: &str) -> i64 {
        let mut bytes = path.as_bytes().to_vec();
        bytes.push(0);
        self.place(&bytes)
    }

    fn buffer(&mut self, length: usize) -> i64 {
        self.place(&vec![0u8; length])
    }

    /// Non-zero bytes with no terminator, with room to spare after them.
    fn unterminated(&mut self, length: usize) -> i64 {
        let start = self.used.next_multiple_of(8);
        assert!(start + length <= self.bytes.len(), "arena exhausted");
        self.bytes[start..start + length].fill(b'x');
        self.used = start + length;
        self.bytes.as_ptr() as usize as i64 + start as i64
    }

    /// Non-zero bytes with no terminator, ending exactly at the end of the
    /// guest's memory — the case where the reachable span is shorter than
    /// the limit the caller asked for.
    fn unterminated_to_end(&mut self, length: usize) -> i64 {
        let start = self.bytes.len() - length;
        assert!(start >= self.used, "arena exhausted");
        self.bytes[start..].fill(b'x');
        self.used = self.bytes.len();
        self.bytes.as_ptr() as usize as i64 + start as i64
    }

    fn read(&self, address: i64, length: usize) -> &[u8] {
        let offset = address as usize - self.bytes.as_ptr() as usize;
        &self.bytes[offset..offset + length]
    }
}

struct Fixture {
    kernel: Kernel<'static, ConsoleStore, Registers>,
    arena: Arena,
    _tree: Tree,
}

fn fixture(label: &str) -> Fixture {
    fixture_seeded(label, Some([0x5a; 32]))
}

/// The same, with the boot seed the host would have supplied — or without
/// one, for a container whose host mounted no `/iso/random`.
fn fixture_seeded(label: &str, seed: Option<[u8; 32]>) -> Fixture {
    let tree = tree(label);
    let baked: &'static baker::Image =
        Box::leak(Box::new(baker::bake_directory(&tree.root).expect("bake")));
    let image = kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse");
    let arena = Arena::new();
    let kernel = Kernel::new(
        ConsoleStore {
            seed,
            ..ConsoleStore::default()
        },
        Registers {
            segment_base: 0,
            memory_limit: arena.limit(),
            ceiling: arena.limit(),
        },
        image,
    );
    Fixture {
        kernel,
        arena,
        _tree: tree,
    }
}

impl Fixture {
    fn call(&mut self, number: i64, arguments: [i64; 6]) -> i64 {
        match self.kernel.dispatch(number, Arguments::new(arguments)) {
            Outcome::Done(value) => value,
            other => panic!("syscall {number} produced {other:?} instead of a result"),
        }
    }

    fn open(&mut self, path: &str, flags: i32) -> i64 {
        let address = self.arena.path(path);
        self.call(number::OPEN, [address, flags as i64, 0, 0, 0, 0])
    }

    /// Opens and requires success, which most tests want.
    fn opened(&mut self, path: &str) -> i64 {
        let fd = self.open(path, open_flags::READ_ONLY);
        assert!(fd >= 0, "opening {path} failed with {fd}");
        fd
    }

    /// Bakes a second tree and attaches it over a directory, which is what a
    /// mount table is for. The bake and the tree are leaked deliberately:
    /// the kernel borrows the image for `'static`, and a fixture that
    /// outlives the test process is the cheapest way to say so.
    fn mount(&mut self, at: &str, label: &str) -> u8 {
        let tree = Box::leak(Box::new(mounted_tree(label)));
        let baked: &'static baker::Image =
            Box::leak(Box::new(baker::bake_directory(&tree.root).expect("bake")));
        let image = kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse");
        let point = self
            .kernel
            .vfs
            .resolve(self.kernel.vfs.root(), at.as_bytes(), Lookup::FOLLOW)
            .unwrap_or_else(|errno| panic!("the mount point {at} does not resolve: {errno:?}"));
        self.kernel
            .vfs
            .mounts_mut()
            .attach(point, image)
            .unwrap_or_else(|errno| panic!("attaching over {at} failed: {errno:?}"))
    }

    #[allow(clippy::result_large_err)]
    fn stat(&mut self, number_: i64, path: &str) -> Result<[u8; STAT_SIZE], i64> {
        let path = self.arena.path(path);
        let buffer = self.arena.buffer(STAT_SIZE);
        let result = self.call(number_, [path, buffer, 0, 0, 0, 0]);
        if result < 0 {
            return Err(result);
        }
        Ok(self.arena.read(buffer, STAT_SIZE).try_into().expect("144 bytes"))
    }
}

fn field_u64(stat: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(stat[at..at + 8].try_into().expect("eight bytes"))
}

fn field_u32(stat: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(stat[at..at + 4].try_into().expect("four bytes"))
}

// ---- open, read, close -----------------------------------------------------

#[test]
fn a_file_opens_reads_and_closes() {
    let mut fixture = fixture("read");
    let fd = fixture.opened("/etc/hosts");
    // Three, not zero: standard input, output and error are already open, as
    // they are for any process a Unix starts. "Lowest free" is still the
    // rule — an empty table would hand this file descriptor 0 and every libc
    // would treat it as stdin.
    assert_eq!(fd, 3, "the lowest free descriptor above the standard streams");

    let buffer = fixture.arena.buffer(64);
    let read = fixture.call(number::READ, [fd, buffer, 64, 0, 0, 0]);
    assert_eq!(read, 20);
    assert_eq!(fixture.arena.read(buffer, 20), b"127.0.0.1 localhost\n");

    // A second read is at the end and returns zero, which is how every
    // reader in existence detects EOF.
    assert_eq!(fixture.call(number::READ, [fd, buffer, 64, 0, 0, 0]), 0);
    assert_eq!(fixture.call(number::CLOSE, [fd, 0, 0, 0, 0, 0]), 0);
    assert_eq!(
        fixture.call(number::CLOSE, [fd, 0, 0, 0, 0, 0]),
        Errno::BadFile.as_result(),
        "closing twice is EBADF"
    );
}

#[test]
fn a_short_read_stops_at_the_end_of_the_file() {
    let mut fixture = fixture("short");
    let fd = fixture.opened("/etc/hostname");
    let buffer = fixture.arena.buffer(64);
    // Ten bytes of content, sixty-four asked for.
    assert_eq!(fixture.call(number::READ, [fd, buffer, 64, 0, 0, 0]), 10);
    assert_eq!(fixture.arena.read(buffer, 10), b"courtyard\n");
}

#[test]
fn a_missing_file_is_enoent_and_a_missing_directory_is_too() {
    let mut fixture = fixture("enoent");
    assert_eq!(
        fixture.open("/etc/nothing", open_flags::READ_ONLY),
        Errno::NoEntry.as_result()
    );
    assert_eq!(
        fixture.open("/nowhere/at/all", open_flags::READ_ONLY),
        Errno::NoEntry.as_result()
    );
}

/// Walking *through* something that is not a directory is `ENOTDIR`, which
/// is a different answer from the thing not being there.
#[test]
fn a_path_through_a_file_is_enotdir() {
    let mut fixture = fixture("enotdir");
    assert_eq!(
        fixture.open("/etc/hosts/more", open_flags::READ_ONLY),
        Errno::NotDir.as_result()
    );
    // A trailing slash says the target must be a directory, and it is not.
    assert_eq!(
        fixture.open("/etc/hosts/", open_flags::READ_ONLY),
        Errno::NotDir.as_result()
    );
    // As does `O_DIRECTORY`.
    assert_eq!(
        fixture.open("/etc/hosts", open_flags::DIRECTORY),
        Errno::NotDir.as_result()
    );
}

/// A filesystem with no writable layer says `EROFS`, and one with a writable
/// layer accepts the write — which is the same question asked of two
/// different mounts, not of two different files.
#[test]
fn a_read_only_mount_is_erofs_and_the_overlay_is_not() {
    let mut fixture = fixture("erofs");
    // The root has an overlay over the image, so every one of these opens.
    for flags in [
        open_flags::WRITE_ONLY,
        open_flags::READ_WRITE,
        open_flags::READ_ONLY | open_flags::TRUNCATE,
    ] {
        let fd = fixture.open("/etc/hosts", flags);
        assert!(fd >= 0, "flags {flags:o} were refused with {fd}");
    }

    // `/proc` is a synthetic mount with nothing writable under it, and it
    // answers the errno that says so.
    assert_eq!(
        fixture.open("/proc/self", open_flags::WRITE_ONLY),
        Errno::IsDir.as_result(),
        "a directory refuses a write before the filesystem gets a say"
    );
    assert_eq!(
        fixture.open("/proc/self/new", open_flags::WRITE_ONLY | open_flags::CREATE),
        Errno::ReadOnlyFs.as_result()
    );
    let in_proc = fixture.arena.path("/proc/made");
    assert_eq!(
        fixture.call(number::MKDIR, [in_proc, 0o755, 0, 0, 0, 0]),
        Errno::ReadOnlyFs.as_result()
    );

    // `O_CREAT` on a file that exists asks to create nothing, and Linux
    // lets it through.
    assert!(fixture.open("/etc/hosts", open_flags::READ_ONLY | open_flags::CREATE) >= 0);
    // `O_EXCL` on a file that exists is `EEXIST`.
    assert_eq!(
        fixture.open(
            "/etc/hosts",
            open_flags::READ_WRITE | open_flags::CREATE | open_flags::EXCLUSIVE
        ),
        Errno::Exists.as_result()
    );
    // And on a file that is missing, `O_CREAT` creates it.
    let fd = fixture.open("/etc/nothing", open_flags::WRITE_ONLY | open_flags::CREATE);
    assert!(fd >= 0, "O_CREAT did not create: {fd}");
    assert!(fixture.stat(number::STAT, "/etc/nothing").is_ok());
}

#[test]
fn opening_a_directory_for_writing_is_eisdir() {
    let mut fixture = fixture("eisdir");
    assert_eq!(
        fixture.open("/etc", open_flags::WRITE_ONLY),
        Errno::IsDir.as_result()
    );
    // Reading a directory as a stream is `EISDIR` too; `getdents64` is how a
    // directory is read.
    let fd = fixture.opened("/etc");
    let buffer = fixture.arena.buffer(64);
    assert_eq!(
        fixture.call(number::READ, [fd, buffer, 64, 0, 0, 0]),
        Errno::IsDir.as_result()
    );
}

// ---- offsets ---------------------------------------------------------------

#[test]
fn lseek_moves_the_offset_and_reports_where_it_landed() {
    let mut fixture = fixture("lseek");
    let fd = fixture.opened("/etc/hosts");

    assert_eq!(fixture.call(number::LSEEK, [fd, 10, seek::SET as i64, 0, 0, 0]), 10);
    let buffer = fixture.arena.buffer(64);
    assert_eq!(fixture.call(number::READ, [fd, buffer, 64, 0, 0, 0]), 10);
    assert_eq!(fixture.arena.read(buffer, 10), b"localhost\n");

    assert_eq!(fixture.call(number::LSEEK, [fd, 0, seek::END as i64, 0, 0, 0]), 20);
    assert_eq!(fixture.call(number::LSEEK, [fd, -5, seek::CURRENT as i64, 0, 0, 0]), 15);
    // Past the end is legal — that is how a sparse file gets written.
    assert_eq!(fixture.call(number::LSEEK, [fd, 100, seek::SET as i64, 0, 0, 0]), 100);
    assert_eq!(fixture.call(number::READ, [fd, buffer, 64, 0, 0, 0]), 0);
    // Before it is not.
    assert_eq!(
        fixture.call(number::LSEEK, [fd, -1, seek::SET as i64, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::LSEEK, [fd, 0, 99, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
}

/// The whole point of `pread64`: it reads at an offset and leaves the
/// description's own position where it was.
#[test]
fn pread_does_not_move_the_offset() {
    let mut fixture = fixture("pread");
    let fd = fixture.opened("/etc/hosts");
    let buffer = fixture.arena.buffer(64);

    assert_eq!(fixture.call(number::PREAD64, [fd, buffer, 9, 10, 0, 0]), 9);
    assert_eq!(fixture.arena.read(buffer, 9), b"localhost");
    // Still at zero, so an ordinary read starts at the beginning.
    assert_eq!(fixture.call(number::READ, [fd, buffer, 9, 0, 0, 0]), 9);
    assert_eq!(fixture.arena.read(buffer, 9), b"127.0.0.1");
}

// ---- descriptors and descriptions ------------------------------------------

/// `dup` makes a second descriptor onto the *same description*, so the offset
/// is shared. Getting this wrong is invisible until something shares one.
#[test]
fn a_duplicated_descriptor_shares_the_offset() {
    let mut fixture = fixture("dup");
    let fd = fixture.opened("/etc/hosts");
    let copy = fixture.call(number::DUP, [fd, 0, 0, 0, 0, 0]);
    assert!(copy > fd);

    assert_eq!(fixture.call(number::LSEEK, [fd, 10, seek::SET as i64, 0, 0, 0]), 10);
    assert_eq!(
        fixture.call(number::LSEEK, [copy, 0, seek::CURRENT as i64, 0, 0, 0]),
        10,
        "the copy sees the original's seek"
    );

    // Closing one leaves the other working: the description outlives any one
    // descriptor pointing at it.
    assert_eq!(fixture.call(number::CLOSE, [fd, 0, 0, 0, 0, 0]), 0);
    let buffer = fixture.arena.buffer(64);
    assert_eq!(fixture.call(number::READ, [copy, buffer, 64, 0, 0, 0]), 10);
}

#[test]
fn dup2_and_dup3_place_a_descriptor_where_asked() {
    let mut fixture = fixture("dup2");
    let fd = fixture.opened("/etc/hosts");

    assert_eq!(fixture.call(number::DUP2, [fd, 7, 0, 0, 0, 0]), 7);
    assert_eq!(fixture.call(number::LSEEK, [fd, 5, seek::SET as i64, 0, 0, 0]), 5);
    assert_eq!(fixture.call(number::LSEEK, [7, 0, seek::CURRENT as i64, 0, 0, 0]), 5);

    // `dup2` onto itself validates and does nothing; `dup3` refuses, because
    // the no-op would discard the `O_CLOEXEC` the caller asked for.
    assert_eq!(fixture.call(number::DUP2, [7, 7, 0, 0, 0, 0]), 7);
    assert_eq!(
        fixture.call(number::DUP3, [7, 7, 0, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::DUP2, [99, 5, 0, 0, 0, 0]),
        Errno::BadFile.as_result()
    );

    assert_eq!(
        fixture.call(number::DUP3, [fd, 9, open_flags::CLOEXEC as i64, 0, 0, 0]),
        9
    );
    assert_eq!(
        fixture.call(number::FCNTL, [9, fcntl_command::GETFD as i64, 0, 0, 0, 0]),
        FD_CLOEXEC as i64,
        "dup3 applied O_CLOEXEC to the new descriptor"
    );
    // And not to the old one — the flag is per descriptor, not per
    // description, which is exactly what makes it survivable across `dup`.
    assert_eq!(
        fixture.call(number::FCNTL, [fd, fcntl_command::GETFD as i64, 0, 0, 0, 0]),
        0
    );
}

#[test]
fn fcntl_reads_and_writes_the_flags_it_owns() {
    let mut fixture = fixture("fcntl");
    let fd = fixture.opened("/etc/hosts");

    // `O_LARGEFILE` is forced on for every descriptor on a 64-bit kernel and
    // reported here; its absence is how some callers conclude they are on a
    // kernel without large-file support.
    assert_eq!(
        fixture.call(number::FCNTL, [fd, fcntl_command::GETFL as i64, 0, 0, 0, 0]),
        (open_flags::READ_ONLY | kisal::file::O_LARGEFILE) as i64
    );
    assert_eq!(
        fixture.call(number::FCNTL, [fd, fcntl_command::GETFD as i64, 0, 0, 0, 0]),
        0
    );
    assert_eq!(
        fixture.call(
            number::FCNTL,
            [fd, fcntl_command::SETFD as i64, FD_CLOEXEC as i64, 0, 0, 0]
        ),
        0
    );
    assert_eq!(
        fixture.call(number::FCNTL, [fd, fcntl_command::GETFD as i64, 0, 0, 0, 0]),
        FD_CLOEXEC as i64
    );

    // `F_SETFL` may change the status flags and must ignore an attempt to
    // change the access mode, which is fixed at open.
    assert_eq!(
        fixture.call(
            number::FCNTL,
            [
                fd,
                fcntl_command::SETFL as i64,
                (open_flags::NONBLOCK | open_flags::WRITE_ONLY) as i64,
                0,
                0,
                0
            ]
        ),
        0
    );
    assert_eq!(
        fixture.call(number::FCNTL, [fd, fcntl_command::GETFL as i64, 0, 0, 0, 0]),
        (open_flags::READ_ONLY | open_flags::NONBLOCK | kisal::file::O_LARGEFILE) as i64,
        "the access mode survived an attempt to change it"
    );

    // `F_DUPFD` takes a floor.
    let copy = fixture.call(number::FCNTL, [fd, fcntl_command::DUPFD as i64, 20, 0, 0, 0]);
    assert_eq!(copy, 20);
    let cloexec = fixture.call(
        number::FCNTL,
        [fd, fcntl_command::DUPFD_CLOEXEC as i64, 30, 0, 0, 0],
    );
    assert_eq!(cloexec, 30);
    assert_eq!(
        fixture.call(number::FCNTL, [30, fcntl_command::GETFD as i64, 0, 0, 0, 0]),
        FD_CLOEXEC as i64
    );
}

// ---- metadata --------------------------------------------------------------

#[test]
fn stat_reports_the_fields_the_bake_preserved() {
    let mut fixture = fixture("stat");
    let stat = fixture.stat(number::STAT, "/script").expect("stat");

    assert_eq!(field_u32(&stat, 24) & 0o170000, 0o100000, "S_IFREG");
    assert_eq!(field_u32(&stat, 24) & 0o777, 0o755);
    assert_eq!(field_u64(&stat, 16), 1, "st_nlink");
    assert_eq!(field_u64(&stat, 48), 18, "st_size");
    assert_eq!(field_u64(&stat, 56), 4096, "st_blksize");
    assert_eq!(field_u64(&stat, 64), 1, "st_blocks: 18 bytes is one block");
    assert_ne!(field_u64(&stat, 8), 0, "st_ino is never zero");
    assert_ne!(field_u64(&stat, 0), 0, "st_dev is never zero");
    // The three timestamps agree, because tar carries only one and the
    // format does not invent the others.
    assert_eq!(field_u64(&stat, 72), field_u64(&stat, 88));
    assert_eq!(field_u64(&stat, 88), field_u64(&stat, 104));
}

/// `stat` follows a final symlink and `lstat` does not — the entire
/// difference between them, and the reason both exist.
#[test]
fn stat_follows_a_symlink_and_lstat_does_not() {
    let mut fixture = fixture("lstat");
    let followed = fixture.stat(number::STAT, "/hosts-link").expect("stat");
    let link = fixture.stat(number::LSTAT, "/hosts-link").expect("lstat");

    assert_eq!(field_u32(&followed, 24) & 0o170000, 0o100000, "a regular file");
    assert_eq!(field_u32(&link, 24) & 0o170000, 0o120000, "a symlink");
    assert_eq!(field_u64(&followed, 48), 20, "the target's size");
    assert_eq!(field_u64(&link, 48), 9, "the link's own size is its target's length");
    assert_ne!(field_u64(&followed, 8), field_u64(&link, 8), "different inodes");

    // Both name the same file as the target does.
    let direct = fixture.stat(number::STAT, "/etc/hosts").expect("stat");
    assert_eq!(field_u64(&followed, 8), field_u64(&direct, 8));
}

#[test]
fn fstat_and_newfstatat_agree_with_stat() {
    let mut fixture = fixture("fstat");
    let direct = fixture.stat(number::STAT, "/etc/hosts").expect("stat");

    let fd = fixture.opened("/etc/hosts");
    let buffer = fixture.arena.buffer(STAT_SIZE);
    assert_eq!(fixture.call(number::FSTAT, [fd, buffer, 0, 0, 0, 0]), 0);
    assert_eq!(fixture.arena.read(buffer, STAT_SIZE), &direct[..]);

    // `newfstatat` relative to a directory descriptor.
    let etc = fixture.opened("/etc");
    let name = fixture.arena.path("hosts");
    let relative = fixture.arena.buffer(STAT_SIZE);
    assert_eq!(
        fixture.call(number::NEWFSTATAT, [etc, name, relative, 0, 0, 0]),
        0
    );
    assert_eq!(fixture.arena.read(relative, STAT_SIZE), &direct[..]);

    // And with `AT_EMPTY_PATH`, which is how `fstat` is spelled through it.
    let empty = fixture.arena.path("");
    let through_fd = fixture.arena.buffer(STAT_SIZE);
    assert_eq!(
        fixture.call(
            number::NEWFSTATAT,
            [fd, empty, through_fd, at::EMPTY_PATH as i64, 0, 0]
        ),
        0
    );
    assert_eq!(fixture.arena.read(through_fd, STAT_SIZE), &direct[..]);
}

#[test]
fn statx_answers_the_basic_fields() {
    let mut fixture = fixture("statx");
    let path = fixture.arena.path("/script");
    let buffer = fixture.arena.buffer(STATX_SIZE);
    assert_eq!(
        fixture.call(number::STATX, [at::FDCWD, path, 0, 0x7ff, buffer, 0]),
        0
    );
    let statx = fixture.arena.read(buffer, STATX_SIZE);
    assert_eq!(field_u32(statx, 0) & 0x7ff, 0x7ff, "stx_mask");
    assert_eq!(field_u32(statx, 16), 1, "stx_nlink");
    assert_eq!(
        u16::from_le_bytes(statx[28..30].try_into().expect("two bytes")) & 0o777,
        0o755,
        "stx_mode"
    );
    assert_eq!(field_u64(statx, 40), 18, "stx_size");
    assert_ne!(field_u64(statx, 32), 0, "stx_ino");
}

// ---- directories -----------------------------------------------------------

#[test]
fn getdents64_reports_entries_in_order_with_their_type() {
    let mut fixture = fixture("getdents");
    let fd = fixture.opened("/etc");
    let buffer = fixture.arena.buffer(1024);
    let written = fixture.call(number::GETDENTS64, [fd, buffer, 1024, 0, 0, 0]);
    assert!(written > 0);

    let bytes = fixture.arena.read(buffer, written as usize).to_vec();
    let mut entries = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let length = u16::from_le_bytes(bytes[at + 16..at + 18].try_into().expect("two")) as usize;
        assert_eq!(length % 8, 0, "d_reclen is eight-byte aligned");
        let entry_type = bytes[at + 18];
        let name_end = bytes[at + 19..at + length]
            .iter()
            .position(|byte| *byte == 0)
            .expect("d_name is NUL-terminated");
        let name = bytes[at + 19..at + 19 + name_end].to_vec();
        assert_ne!(
            u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight")),
            0,
            "d_ino is never zero"
        );
        entries.push((name, entry_type));
        at += length;
    }
    use kisal::image::directory_entry_type as kind;
    assert_eq!(
        entries,
        vec![
            // Synthesized, and first: a real directory has them and readers
            // assume it.
            (b".".to_vec(), kind::DIRECTORY),
            (b"..".to_vec(), kind::DIRECTORY),
            // Four different `d_type`s, on purpose. A directory containing
            // only regular files cannot tell a `d_type` read out of the image
            // from one hardcoded to `DT_REG` — a mutation doing exactly that
            // passed the whole suite.
            (b"conf.d".to_vec(), kind::DIRECTORY),
            (b"hostname".to_vec(), kind::REGULAR),
            (b"hosts".to_vec(), kind::REGULAR),
            (b"hosts-alias".to_vec(), kind::SYMLINK),
            (b"pipe".to_vec(), kind::FIFO),
        ]
    );

    // A second call is past the end and reports nothing, which is how a
    // reader stops.
    assert_eq!(fixture.call(number::GETDENTS64, [fd, buffer, 1024, 0, 0, 0]), 0);
}

/// A buffer too small for even one entry is `EINVAL`, not a short read — the
/// caller would otherwise loop forever making no progress.
#[test]
fn getdents64_refuses_a_buffer_that_cannot_hold_one_entry() {
    let mut fixture = fixture("getdents-small");
    let fd = fixture.opened("/etc");
    let buffer = fixture.arena.buffer(16);
    assert_eq!(
        fixture.call(number::GETDENTS64, [fd, buffer, 16, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
}

/// A buffer that holds some entries returns those and resumes where it left
/// off, which is what a real reader's loop depends on.
#[test]
fn getdents64_resumes_where_it_stopped() {
    let mut fixture = fixture("getdents-partial");
    let fd = fixture.opened("/etc");
    let buffer = fixture.arena.buffer(32);
    let mut names = Vec::new();
    loop {
        let written = fixture.call(number::GETDENTS64, [fd, buffer, 32, 0, 0, 0]);
        assert!(written >= 0, "getdents64 failed with {written}");
        if written == 0 {
            break;
        }
        let bytes = fixture.arena.read(buffer, written as usize).to_vec();
        let mut at = 0usize;
        while at < bytes.len() {
            let length =
                u16::from_le_bytes(bytes[at + 16..at + 18].try_into().expect("two")) as usize;
            let end = bytes[at + 19..at + length]
                .iter()
                .position(|byte| *byte == 0)
                .expect("terminated");
            names.push(bytes[at + 19..at + 19 + end].to_vec());
            at += length;
        }
        assert!(names.len() < 20, "the reader is not making progress");
    }
    assert_eq!(
        names,
        vec![
            b".".to_vec(),
            b"..".to_vec(),
            b"conf.d".to_vec(),
            b"hostname".to_vec(),
            b"hosts".to_vec(),
            b"hosts-alias".to_vec(),
            b"pipe".to_vec(),
        ],
        "every entry arrives exactly once across the calls"
    );
}

#[test]
fn getdents64_on_a_file_is_enotdir() {
    let mut fixture = fixture("getdents-file");
    let fd = fixture.opened("/etc/hosts");
    let buffer = fixture.arena.buffer(1024);
    assert_eq!(
        fixture.call(number::GETDENTS64, [fd, buffer, 1024, 0, 0, 0]),
        Errno::NotDir.as_result()
    );
}

// ---- symlinks --------------------------------------------------------------

#[test]
fn readlink_reports_the_target_without_following_it() {
    let mut fixture = fixture("readlink");
    let path = fixture.arena.path("/hosts-link");
    let buffer = fixture.arena.buffer(64);
    let length = fixture.call(number::READLINK, [path, buffer, 64, 0, 0, 0]);
    assert_eq!(length, 9, "`etc/hosts` is nine bytes, with no terminator");
    assert_eq!(fixture.arena.read(buffer, 9), b"etc/hosts");

    // Truncated rather than refused, and with no terminator: the caller is
    // expected to notice a full buffer and ask again.
    let short = fixture.arena.buffer(4);
    assert_eq!(fixture.call(number::READLINK, [path, short, 4, 0, 0, 0]), 4);
    assert_eq!(fixture.arena.read(short, 4), b"etc/");

    // Not a symlink is `EINVAL`.
    let file = fixture.arena.path("/etc/hosts");
    assert_eq!(
        fixture.call(number::READLINK, [file, buffer, 64, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
}

#[test]
fn a_symlink_is_followed_through_and_resolved_against_its_own_directory() {
    let mut fixture = fixture("follow");

    // A relative target resolves against the directory the *link* is in, not
    // the working directory. `/usr/etc-link` is `../etc`.
    let fd = fixture.opened("/usr/etc-link/hosts");
    let buffer = fixture.arena.buffer(64);
    assert_eq!(fixture.call(number::READ, [fd, buffer, 64, 0, 0, 0]), 20);

    // An absolute target ignores where the link lives.
    let absolute = fixture.opened("/absolute-link");
    assert_eq!(fixture.call(number::READ, [absolute, buffer, 64, 0, 0, 0]), 10);

    // And a link to a directory can be walked through.
    let library = fixture.opened("/lib/libthing.so");
    assert_eq!(fixture.call(number::READ, [library, buffer, 64, 0, 0, 0]), 12);
}

#[test]
fn a_symlink_cycle_is_eloop() {
    let mut fixture = fixture("eloop");
    assert_eq!(
        fixture.open("/loop-a", open_flags::READ_ONLY),
        Errno::Loop.as_result()
    );
    // `O_NOFOLLOW` opens the link itself rather than chasing it — and Linux
    // reports that as `ELOOP` too, which is the one place the errno means
    // "this is a link" rather than "too many links".
    assert_eq!(
        fixture.open("/loop-a", open_flags::NOFOLLOW),
        Errno::Loop.as_result()
    );
    // `lstat` sees it without following.
    let link = fixture.stat(number::LSTAT, "/loop-a").expect("lstat");
    assert_eq!(field_u32(&link, 24) & 0o170000, 0o120000);
}

// ---- traversal -------------------------------------------------------------

#[test]
fn dot_and_dot_dot_walk_the_tree() {
    let mut fixture = fixture("dots");
    let direct = fixture.stat(number::STAT, "/etc/hosts").expect("stat");

    for path in [
        "/./etc/hosts",
        "/etc/./hosts",
        "/usr/../etc/hosts",
        "/usr/lib/../../etc/hosts",
        "//etc//hosts",
    ] {
        let walked = fixture.stat(number::STAT, path).expect(path);
        assert_eq!(field_u64(&walked, 8), field_u64(&direct, 8), "{path}");
    }

    // The root is its own parent, so no path escapes upward.
    let root = fixture.stat(number::STAT, "/").expect("stat /");
    let above = fixture.stat(number::STAT, "/../../..").expect("stat /../..");
    assert_eq!(field_u64(&root, 8), field_u64(&above, 8));
}

#[test]
fn a_relative_path_starts_at_the_working_directory() {
    let mut fixture = fixture("cwd");
    let direct = fixture.stat(number::STAT, "/etc/hosts").expect("stat");

    let etc = fixture.arena.path("/etc");
    assert_eq!(fixture.call(number::CHDIR, [etc, 0, 0, 0, 0, 0]), 0);
    let relative = fixture.stat(number::STAT, "hosts").expect("stat");
    assert_eq!(field_u64(&relative, 8), field_u64(&direct, 8));

    let buffer = fixture.arena.buffer(64);
    let length = fixture.call(number::GETCWD, [buffer, 64, 0, 0, 0, 0]);
    assert_eq!(length, 5, "\"/etc\" plus its terminator");
    assert_eq!(fixture.arena.read(buffer, 5), b"/etc\0");

    // `fchdir` gets there through a descriptor instead.
    let usr = fixture.opened("/usr/lib");
    assert_eq!(fixture.call(number::FCHDIR, [usr, 0, 0, 0, 0, 0]), 0);
    let length = fixture.call(number::GETCWD, [buffer, 64, 0, 0, 0, 0]);
    assert_eq!(fixture.arena.read(buffer, length as usize), b"/usr/lib\0");
}

#[test]
fn getcwd_reports_the_root_and_refuses_a_buffer_too_small() {
    let mut fixture = fixture("getcwd");
    let buffer = fixture.arena.buffer(64);
    assert_eq!(fixture.call(number::GETCWD, [buffer, 64, 0, 0, 0, 0]), 2);
    assert_eq!(fixture.arena.read(buffer, 2), b"/\0");

    let etc = fixture.arena.path("/etc");
    fixture.call(number::CHDIR, [etc, 0, 0, 0, 0, 0]);
    assert_eq!(
        fixture.call(number::GETCWD, [buffer, 3, 0, 0, 0, 0]),
        Errno::Range.as_result(),
        "\"/etc\" needs five bytes with its terminator"
    );
}

#[test]
fn chdir_refuses_anything_that_is_not_a_directory() {
    let mut fixture = fixture("chdir");
    let file = fixture.arena.path("/etc/hosts");
    assert_eq!(
        fixture.call(number::CHDIR, [file, 0, 0, 0, 0, 0]),
        Errno::NotDir.as_result()
    );
    let fd = fixture.opened("/etc/hosts");
    assert_eq!(
        fixture.call(number::FCHDIR, [fd, 0, 0, 0, 0, 0]),
        Errno::NotDir.as_result()
    );
}

#[test]
fn openat_resolves_against_a_directory_descriptor() {
    let mut fixture = fixture("openat");
    let etc = fixture.opened("/etc");
    let name = fixture.arena.path("hosts");
    let fd = fixture.call(
        number::OPENAT,
        [etc, name, open_flags::READ_ONLY as i64, 0, 0, 0],
    );
    assert!(fd >= 0);

    // An absolute path ignores the descriptor entirely.
    let absolute = fixture.arena.path("/etc/hostname");
    let other = fixture.call(
        number::OPENAT,
        [etc, absolute, open_flags::READ_ONLY as i64, 0, 0, 0],
    );
    assert!(other >= 0);
    let buffer = fixture.arena.buffer(64);
    assert_eq!(fixture.call(number::READ, [other, buffer, 64, 0, 0, 0]), 10);

    // A descriptor that is not a directory is `ENOTDIR`.
    let file = fixture.opened("/etc/hosts");
    assert_eq!(
        fixture.call(
            number::OPENAT,
            [file, name, open_flags::READ_ONLY as i64, 0, 0, 0]
        ),
        Errno::NotDir.as_result()
    );
}

// ---- access ----------------------------------------------------------------

#[test]
fn access_answers_existence_and_executability() {
    let mut fixture = fixture("access");
    let script = fixture.arena.path("/script");
    let hosts = fixture.arena.path("/etc/hosts");
    let missing = fixture.arena.path("/nothing");

    assert_eq!(
        fixture.call(number::ACCESS, [script, access_mode::EXISTS as i64, 0, 0, 0, 0]),
        0
    );
    assert_eq!(
        fixture.call(number::ACCESS, [missing, access_mode::EXISTS as i64, 0, 0, 0, 0]),
        Errno::NoEntry.as_result()
    );
    assert_eq!(
        fixture.call(number::ACCESS, [script, access_mode::EXECUTE as i64, 0, 0, 0, 0]),
        0
    );
    assert_eq!(
        fixture.call(number::ACCESS, [hosts, access_mode::EXECUTE as i64, 0, 0, 0, 0]),
        Errno::Access.as_result()
    );
    // The root has a writable layer, so a write probe succeeds. On a mount
    // that has none the answer is `EROFS` — which is what Linux answers on a
    // read-only filesystem and what callers branch on to fall back somewhere
    // writable, rather than `EACCES`, which would send them looking for a
    // permission problem that does not exist.
    assert_eq!(
        fixture.call(number::ACCESS, [hosts, access_mode::WRITE as i64, 0, 0, 0, 0]),
        0
    );
    let in_proc = fixture.arena.path("/proc/self");
    assert_eq!(
        fixture.call(number::ACCESS, [in_proc, access_mode::WRITE as i64, 0, 0, 0, 0]),
        Errno::ReadOnlyFs.as_result()
    );
    // A mode with bits outside R/W/X is `EINVAL`, not a cheerful yes.
    assert_eq!(
        fixture.call(number::ACCESS, [hosts, 8, 0, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
}

// ---- guest memory ----------------------------------------------------------

/// A path that runs off the end of the guest's memory with no terminator is
/// refused rather than read past.
#[test]
fn a_path_outside_the_guest_is_refused() {
    let mut fixture = fixture("badpath");
    // An address past the end, and the null pointer. Both are refused
    // before anything is read — the reachable span is zero.
    let outside = fixture.arena.limit() as i64 + 16;
    assert_eq!(
        fixture.call(number::OPEN, [outside, 0, 0, 0, 0, 0]),
        Errno::Fault.as_result()
    );
    assert_eq!(
        fixture.call(number::OPEN, [0, 0, 0, 0, 0, 0]),
        Errno::Fault.as_result()
    );
}

/// A string with no terminator is two different failures, and they are not
/// the same errno.
///
/// If the bytes run off the end of the guest's memory before a terminator
/// appears, the kernel would have to read memory the guest does not own to
/// find one: `EFAULT`. If they run past `PATH_MAX` with memory still to
/// spare, the guest owns every byte and the path is simply too long:
/// `ENAMETOOLONG`. Reading on to find out which is the bug this is here to
/// prevent, and the two branches are indistinguishable unless both are
/// driven.
#[test]
fn an_unterminated_path_is_refused_by_which_way_it_failed() {
    let mut fixture = fixture("unterminated");
    // Room to spare after it, so `PATH_MAX` is what it runs into.
    let long = fixture.arena.unterminated(PATH_MAX + 64);
    assert_eq!(
        fixture.call(number::OPEN, [long, 0, 0, 0, 0, 0]),
        Errno::NameTooLong.as_result()
    );
    // A path that *would* fit, but is not terminated before memory ends.
    let running_off = fixture.arena.unterminated_to_end(64);
    assert_eq!(
        fixture.call(number::OPEN, [running_off, 0, 0, 0, 0, 0]),
        Errno::Fault.as_result()
    );
}

#[test]
fn a_read_into_memory_the_guest_does_not_own_is_efault() {
    let mut fixture = fixture("badbuffer");
    let fd = fixture.opened("/etc/hosts");
    let outside = fixture.arena.limit() as i64 + 16;
    assert_eq!(
        fixture.call(number::READ, [fd, outside, 8, 0, 0, 0]),
        Errno::Fault.as_result()
    );
}

// ---- the standard streams --------------------------------------------------

/// A process starts with standard input, output and error already open, as
/// every Unix hands them over.
///
/// With an empty table the guest's first `open` takes descriptor 0 — which
/// every libc treats as standard input — and `fstat(1)` answers `EBADF`,
/// which is exactly what CPython checks before setting `sys.stdout = None`
/// and turning every `print` into a silent no-op.
#[test]
fn the_standard_streams_are_open_before_the_guest_runs() {
    let mut fixture = fixture("stdio");
    for fd in 0..3i64 {
        let buffer = fixture.arena.buffer(STAT_SIZE);
        assert_eq!(
            fixture.call(number::FSTAT, [fd, buffer, 0, 0, 0, 0]),
            0,
            "fstat({fd}) must not be EBADF"
        );
        let stat = fixture.arena.read(buffer, STAT_SIZE);
        assert_eq!(
            field_u32(stat, 24) & 0o170000,
            0o020000,
            "fd {fd} is a character device"
        );
        assert_ne!(field_u64(stat, 8), 0, "st_ino is never zero");
    }
    // Distinct objects, which is how userspace tells them apart.
    let inode_of = |fixture: &mut Fixture, fd: i64| {
        let buffer = fixture.arena.buffer(STAT_SIZE);
        fixture.call(number::FSTAT, [fd, buffer, 0, 0, 0, 0]);
        field_u64(fixture.arena.read(buffer, STAT_SIZE), 8)
    };
    let (a, b, c) = (
        inode_of(&mut fixture, 0),
        inode_of(&mut fixture, 1),
        inode_of(&mut fixture, 2),
    );
    assert!(a != b && b != c && a != c);
}

/// `write` resolves through the descriptor table like every other row.
///
/// It used to match the literal numbers 1 and 2, which split the descriptor
/// space in two: `dup2(file, 1)` succeeded and `write(1, …)` went to the
/// console anyway while `read(1, …)` read the file. Shell redirection is that
/// idiom, and it failed silently.
#[test]
fn write_follows_the_descriptor_rather_than_its_number() {
    let mut fixture = fixture("write-routing");
    let message = fixture.arena.place(b"to stdout");
    assert_eq!(fixture.call(number::WRITE, [1, message, 9, 0, 0, 0]), 9);
    assert_eq!(fixture.kernel.store.contents(kisal::paths::CONSOLE_STDOUT), b"to stdout");

    // Move a read-only image file onto descriptor 1. Writing there is now
    // `EBADF` — the descriptor's access mode forbids it — and emphatically
    // not a cheerful success that lands on the console.
    let file = fixture.opened("/etc/hosts");
    assert_eq!(fixture.call(number::DUP2, [file, 1, 0, 0, 0, 0]), 1);
    assert_eq!(
        fixture.call(number::WRITE, [1, message, 9, 0, 0, 0]),
        Errno::BadFile.as_result()
    );
    assert_eq!(
        fixture.kernel.store.contents(kisal::paths::CONSOLE_STDOUT),
        b"to stdout",
        "the refused write did not leak to the console"
    );
    // And reading descriptor 1 now reads the file, consistently.
    let buffer = fixture.arena.buffer(32);
    assert_eq!(fixture.call(number::READ, [1, buffer, 32, 0, 0, 0]), 20);
}

/// Standard input's bytes come from the host, which is the only path in the
/// system that reads through the `ll-store` boundary.
#[test]
fn standard_input_reads_through_the_store() {
    let mut fixture = fixture("stdin");
    fixture.kernel.store.input = b"typed by a human\n".to_vec();

    let buffer = fixture.arena.buffer(64);
    assert_eq!(fixture.call(number::READ, [0, buffer, 64, 0, 0, 0]), 17);
    assert_eq!(fixture.arena.read(buffer, 17), b"typed by a human\n");
    // The offset advances, so a second read reports end of input rather than
    // the same bytes again — a stream that never ends is one every reader
    // loops on forever.
    assert_eq!(fixture.call(number::READ, [0, buffer, 64, 0, 0, 0]), 0);
}

#[test]
fn a_console_stream_is_not_seekable_and_is_not_a_directory() {
    let mut fixture = fixture("console-shape");
    assert_eq!(
        fixture.call(number::LSEEK, [1, 0, seek::SET as i64, 0, 0, 0]),
        Errno::NotSeekable.as_result()
    );
    let buffer = fixture.arena.buffer(256);
    assert_eq!(
        fixture.call(number::GETDENTS64, [1, buffer, 256, 0, 0, 0]),
        Errno::NotDir.as_result()
    );
    // Writing to standard *input* is EBADF: it is opened read-only.
    assert_eq!(
        fixture.call(number::WRITE, [0, buffer, 1, 0, 0, 0]),
        Errno::BadFile.as_result()
    );
}

// ---- resolution conformance -----------------------------------------------

/// An absolute path ignores the descriptor, so the descriptor is not
/// validated either. Linux never dereferences `dirfd` after a leading slash.
#[test]
fn an_absolute_path_ignores_the_directory_descriptor() {
    let mut fixture = fixture("openat-absolute");
    let absolute = fixture.arena.path("/etc/hosts");
    for dirfd in [-1i64, 9999, at::FDCWD] {
        let fd = fixture.call(
            number::OPENAT,
            [dirfd, absolute, open_flags::READ_ONLY as i64, 0, 0, 0],
        );
        assert!(fd >= 0, "openat({dirfd}, \"/etc/hosts\") failed with {fd}");
    }
    // A descriptor that is not a directory is likewise irrelevant.
    let file = fixture.opened("/etc/hosts");
    assert!(
        fixture.call(
            number::OPENAT,
            [file, absolute, open_flags::READ_ONLY as i64, 0, 0, 0]
        ) >= 0
    );
}

/// A trailing slash forces the final symlink to be followed, even under
/// `O_NOFOLLOW`. `/lib`, `/bin` and `/sbin` are symlinks in every modern base
/// image, so `stat("/lib/")` is an ordinary thing to do.
#[test]
fn a_trailing_slash_follows_the_final_symlink() {
    let mut fixture = fixture("trailing-slash");
    let followed = fixture.stat(number::LSTAT, "/lib/").expect("lstat /lib/");
    assert_eq!(
        field_u32(&followed, 24) & 0o170000,
        0o040000,
        "`lstat` of a symlink *with* a trailing slash sees the directory"
    );
    let bare = fixture.stat(number::LSTAT, "/lib").expect("lstat /lib");
    assert_eq!(
        field_u32(&bare, 24) & 0o170000,
        0o120000,
        "and without one it sees the link"
    );
    assert!(
        fixture.open("/lib/", open_flags::NOFOLLOW) >= 0,
        "O_NOFOLLOW does not apply to a component a trailing slash made non-final"
    );
}

/// The traversal limit counts links, not nesting. A chain laid end to end
/// costs what a nested one does, which is what Linux counts.
#[test]
fn the_symlink_limit_counts_total_traversals() {
    let mut fixture = fixture("eloop-chain");
    // `chain39` is reached through 40 links, the limit; `chain40` needs 41.
    assert!(
        fixture.stat(number::STAT, "/chain39").is_ok(),
        "forty links is exactly the limit"
    );
    assert_eq!(
        fixture.stat(number::STAT, "/chain40").unwrap_err(),
        Errno::Loop.as_result()
    );
    assert_eq!(
        fixture.stat(number::STAT, "/chain49").unwrap_err(),
        Errno::Loop.as_result()
    );
}

/// A parent that is not a directory is decided before anything about the
/// component's name, which is Linux's precedence.
#[test]
fn enotdir_beats_enametoolong() {
    let mut fixture = fixture("precedence");
    let long = "x".repeat(300);
    assert_eq!(
        fixture.stat(number::STAT, &format!("/etc/hosts/{long}")).unwrap_err(),
        Errno::NotDir.as_result()
    );
    // With a real directory as the parent, the name's length is what fails.
    assert_eq!(
        fixture.stat(number::STAT, &format!("/etc/{long}")).unwrap_err(),
        Errno::NameTooLong.as_result()
    );
}

// ---- descriptor and metadata conformance -----------------------------------

/// `O_PATH` yields a reference to a file, not an open file: no reads, and a
/// symlink can be held without being followed.
#[test]
fn o_path_opens_a_reference_rather_than_a_file() {
    let mut fixture = fixture("o-path");
    let fd = fixture.open("/etc/hosts", open_flags::PATH);
    assert!(fd >= 0);
    let buffer = fixture.arena.buffer(32);
    assert_eq!(
        fixture.call(number::READ, [fd, buffer, 32, 0, 0, 0]),
        Errno::BadFile.as_result(),
        "an O_PATH descriptor is not readable"
    );
    // `fstat` still works — that is what the descriptor is for.
    assert_eq!(fixture.call(number::FSTAT, [fd, buffer, 0, 0, 0, 0]), 0);

    // `O_PATH|O_NOFOLLOW` is the documented way to hold a symlink itself.
    let link = fixture.open("/hosts-link", open_flags::PATH | open_flags::NOFOLLOW);
    assert!(link >= 0, "O_PATH|O_NOFOLLOW on a symlink failed with {link}");
    let stat = fixture.arena.buffer(STAT_SIZE);
    assert_eq!(fixture.call(number::FSTAT, [link, stat, 0, 0, 0, 0]), 0);
    assert_eq!(
        field_u32(fixture.arena.read(stat, STAT_SIZE), 24) & 0o170000,
        0o120000,
        "the descriptor names the link, not its target"
    );
    // And the access mode is ignored under O_PATH, so this is not EROFS.
    assert!(fixture.open("/etc/hosts", open_flags::PATH | open_flags::WRITE_ONLY) >= 0);
}

/// An image file has no holes, so every offset in it is data and the only
/// hole is the end — what a non-sparse file reports on Linux.
#[test]
fn seek_data_and_seek_hole_describe_a_file_with_no_holes() {
    let mut fixture = fixture("seek-data");
    let fd = fixture.opened("/etc/hosts");
    assert_eq!(fixture.call(number::LSEEK, [fd, 0, seek::DATA as i64, 0, 0, 0]), 0);
    assert_eq!(fixture.call(number::LSEEK, [fd, 5, seek::DATA as i64, 0, 0, 0]), 5);
    assert_eq!(fixture.call(number::LSEEK, [fd, 0, seek::HOLE as i64, 0, 0, 0]), 20);
    // Past the end there is no data to find.
    assert_eq!(
        fixture.call(number::LSEEK, [fd, 20, seek::DATA as i64, 0, 0, 0]),
        Errno::NoData.as_result()
    );
}

#[test]
fn fcntl_refuses_an_impossible_floor_and_names_what_it_cannot_do() {
    let mut fixture = fixture("fcntl-edges");
    let fd = fixture.opened("/etc/hosts");
    // A floor outside the descriptor space is EINVAL, not EMFILE: Linux
    // rejects the argument before it looks for room.
    assert_eq!(
        fixture.call(number::FCNTL, [fd, fcntl_command::DUPFD as i64, 1 << 20, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::FCNTL, [fd, fcntl_command::DUPFD as i64, -1, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    // A command Linux implements and this does not is a named fault, never a
    // plausible `EINVAL` that reads as "this kernel has no such command".
    let outcome = fixture.kernel.dispatch(
        number::FCNTL,
        Arguments::new([fd, 6 /* F_SETLK */, 0, 0, 0, 0]),
    );
    let Outcome::Fault(fault) = outcome else {
        panic!("F_SETLK produced {outcome:?} instead of a named fault");
    };
    let mut message = String::new();
    fault.message(&mut message);
    assert!(message.contains("record locks"), "{message}");
}

#[test]
fn the_stat_family_validates_its_flags() {
    let mut fixture = fixture("flags");
    let path = fixture.arena.path("/etc/hosts");
    let buffer = fixture.arena.buffer(STATX_SIZE);
    assert_eq!(
        fixture.call(number::NEWFSTATAT, [at::FDCWD, path, buffer, 0xff00, 0, 0]),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::STATX, [at::FDCWD, path, 0x0f00_0000, 0x7ff, buffer, 0]),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::FACCESSAT2, [at::FDCWD, path, 0, 0x4000, 0, 0]),
        Errno::Invalid.as_result()
    );
}

/// `faccessat2` honours the flags it is given — and `faccessat` has none to
/// honour.
///
/// The kernel's `faccessat` is a three-argument row; the four-argument libc
/// function is implemented by calling `faccessat2`, which exists for exactly
/// that reason. Reading a fourth argument from `faccessat` would answer
/// `EINVAL` to an ordinary call whenever the caller's `%r10` happened to be
/// non-zero, so both halves are asserted here.
#[test]
fn faccessat2_honours_symlink_nofollow() {
    let mut fixture = fixture("faccessat-flags");
    // `/dangling` names a path that does not exist, so following it fails
    // and not following it succeeds. The two answers differ only if the flag
    // is read at all.
    let path = fixture.arena.path("/dangling");
    assert_eq!(
        fixture.call(number::FACCESSAT, [at::FDCWD, path, 0, 0, 0, 0]),
        Errno::NoEntry.as_result()
    );
    assert_eq!(
        fixture.call(
            number::FACCESSAT2,
            [at::FDCWD, path, 0, at::SYMLINK_NOFOLLOW as i64, 0, 0]
        ),
        0
    );
    // The three-argument row ignores whatever is in the fourth register,
    // exactly as the real kernel does.
    assert_eq!(
        fixture.call(
            number::FACCESSAT,
            [at::FDCWD, path, 0, at::SYMLINK_NOFOLLOW as i64, 0, 0]
        ),
        Errno::NoEntry.as_result()
    );
    assert_eq!(
        fixture.call(number::FACCESSAT, [at::FDCWD, path, 0, 0xdead_beef, 0, 0]),
        Errno::NoEntry.as_result()
    );
}

/// A device node baked into the image has no driver behind it. `EINVAL`
/// would be a plausible answer to a perfectly valid call, so it is refused by
/// name instead — `/dev` becomes a synthetic mount at M4.
#[test]
fn reading_a_device_node_is_refused_by_name() {
    let mut fixture = fixture("device");
    let fd = fixture.opened("/etc/pipe");
    let buffer = fixture.arena.buffer(16);
    assert_eq!(
        fixture.call(number::READ, [fd, buffer, 16, 0, 0, 0]),
        Errno::NoDevice.as_result()
    );
}

// ---- extended attributes ---------------------------------------------------

/// The bake preserves every attribute an image carries. A filesystem that
/// stored them and could not read them back would be one that lost them.
#[test]
fn extended_attributes_come_back_out_of_the_image() {
    let mut fixture = fixture("getxattr");
    let path = fixture.arena.path("/etc/hosts");
    let name = fixture.arena.place(b"user.origin\0");
    let value = fixture.arena.buffer(64);

    // A zero capacity asks how much room to make, which is what every
    // caller does first.
    assert_eq!(
        fixture.call(number::GETXATTR, [path, name, 0, 0, 0, 0]),
        8,
        "the length of \"the bake\""
    );
    assert_eq!(fixture.call(number::GETXATTR, [path, name, value, 64, 0, 0]), 8);
    assert_eq!(fixture.arena.read(value, 8), b"the bake");
    // Too small is `ERANGE`, and nothing is written.
    assert_eq!(
        fixture.call(number::GETXATTR, [path, name, value, 7, 0, 0]),
        Errno::Range.as_result()
    );
    // An attribute with an empty value exists and has length zero, which is
    // a different answer from not existing.
    let empty = fixture.arena.place(b"user.empty\0");
    assert_eq!(fixture.call(number::GETXATTR, [path, empty, value, 64, 0, 0]), 0);
    let missing = fixture.arena.place(b"user.missing\0");
    assert_eq!(
        fixture.call(number::GETXATTR, [path, missing, value, 64, 0, 0]),
        Errno::NoData.as_result()
    );
    // A file that has none says `ENODATA` too — the file is there.
    let bare = fixture.arena.path("/etc/hostname");
    assert_eq!(
        fixture.call(number::GETXATTR, [bare, name, value, 64, 0, 0]),
        Errno::NoData.as_result()
    );
    // A path that is not there is `ENOENT`, ahead of anything about names.
    let absent = fixture.arena.path("/nowhere");
    assert_eq!(
        fixture.call(number::GETXATTR, [absent, name, value, 64, 0, 0]),
        Errno::NoEntry.as_result()
    );
}

#[test]
fn the_attribute_list_is_names_one_after_another() {
    let mut fixture = fixture("listxattr");
    let path = fixture.arena.path("/etc/hosts");
    let list = fixture.arena.buffer(128);
    // "user.empty\0user.origin\0" — sorted, because the baker sorts them.
    let total = 11 + 12;
    assert_eq!(fixture.call(number::LISTXATTR, [path, 0, 0, 0, 0, 0]), total);
    assert_eq!(
        fixture.call(number::LISTXATTR, [path, list, total - 1, 0, 0, 0]),
        Errno::Range.as_result()
    );
    assert_eq!(fixture.call(number::LISTXATTR, [path, list, 128, 0, 0, 0]), total);
    assert_eq!(
        fixture.arena.read(list, total as usize),
        b"user.empty\0user.origin\0"
    );
    // A file with no attributes lists nothing and does not fail.
    let bare = fixture.arena.path("/etc/hostname");
    assert_eq!(fixture.call(number::LISTXATTR, [bare, list, 128, 0, 0, 0]), 0);
}

/// The `f` forms take a descriptor and the `l` forms do not follow a final
/// symlink — the same three-way split as `stat`/`lstat`/`fstat`.
#[test]
fn the_descriptor_and_no_follow_forms_name_what_they_say() {
    let mut fixture = fixture("xattr-forms");
    let name = fixture.arena.place(b"user.origin\0");
    let value = fixture.arena.buffer(64);
    let fd = fixture.opened("/etc/hosts");
    assert_eq!(fixture.call(number::FGETXATTR, [fd, name, value, 64, 0, 0]), 8);
    assert_eq!(fixture.call(number::FLISTXATTR, [fd, value, 64, 0, 0, 0]), 23);

    // `/hosts-link` points at `/etc/hosts`. Following finds the attribute;
    // not following lands on the link, which has none.
    let link = fixture.arena.path("/hosts-link");
    assert_eq!(fixture.call(number::GETXATTR, [link, name, value, 64, 0, 0]), 8);
    assert_eq!(
        fixture.call(number::LGETXATTR, [link, name, value, 64, 0, 0]),
        Errno::NoData.as_result()
    );
    assert_eq!(fixture.call(number::LLISTXATTR, [link, value, 64, 0, 0, 0]), 0);
}

/// A console stream answers the way a real character device does. Verified
/// against `/dev/null`: `ENODATA` and an empty list, not `ENOTSUP` — which
/// would claim the whole filesystem has no attributes.
#[test]
fn a_console_stream_has_no_attributes_rather_than_no_support() {
    let mut fixture = fixture("xattr-console");
    let name = fixture.arena.place(b"user.origin\0");
    let value = fixture.arena.buffer(64);
    assert_eq!(
        fixture.call(number::FGETXATTR, [1, name, value, 64, 0, 0]),
        Errno::NoData.as_result()
    );
    assert_eq!(fixture.call(number::FLISTXATTR, [1, value, 64, 0, 0, 0]), 0);
}

#[test]
fn an_attribute_name_is_bounded_and_writing_one_is_refused() {
    let mut fixture = fixture("xattr-limits");
    let path = fixture.arena.path("/etc/hosts");
    let value = fixture.arena.buffer(64);
    // Linux answers `ERANGE` for both an empty name and one over
    // `XATTR_NAME_MAX`, rather than looking either of them up.
    let empty = fixture.arena.place(b"\0");
    assert_eq!(
        fixture.call(number::GETXATTR, [path, empty, value, 64, 0, 0]),
        Errno::Range.as_result()
    );
    let mut long = b"user.".to_vec();
    long.resize(300, b'n');
    long.push(0);
    let long = fixture.arena.place(&long);
    assert_eq!(
        fixture.call(number::GETXATTR, [path, long, value, 64, 0, 0]),
        Errno::Range.as_result()
    );
    // The image is read-only, and `EROFS` is what says so.
    let name = fixture.arena.place(b"user.origin\0");
    for call in [
        number::SETXATTR,
        number::LSETXATTR,
        number::FSETXATTR,
        number::REMOVEXATTR,
        number::LREMOVEXATTR,
        number::FREMOVEXATTR,
    ] {
        assert_eq!(
            fixture.call(call, [path, name, value, 4, 0, 0]),
            Errno::ReadOnlyFs.as_result(),
            "call {call}"
        );
    }
}

// ---- the remaining `…at` conformance --------------------------------------

/// `readlinkat` with an empty path reads the descriptor itself, which is how
/// a symlink held open with `O_PATH|O_NOFOLLOW` is read — `realpath`'s inner
/// loop. Linux takes an empty path here unconditionally; there is no other
/// meaning it could have.
#[test]
fn readlinkat_reads_the_descriptor_itself() {
    let mut fixture = fixture("readlinkat-empty");
    let link = fixture.open("/hosts-link", open_flags::PATH | open_flags::NOFOLLOW);
    assert!(link >= 0);
    let empty = fixture.arena.place(b"\0");
    let buffer = fixture.arena.buffer(64);
    assert_eq!(
        fixture.call(number::READLINKAT, [link, empty, buffer, 64, 0, 0]),
        9
    );
    assert_eq!(fixture.arena.read(buffer, 9), b"etc/hosts");

    // A descriptor that is not a symlink is `EINVAL`, as it is by path.
    let file = fixture.opened("/etc/hosts");
    assert_eq!(
        fixture.call(number::READLINKAT, [file, empty, buffer, 64, 0, 0]),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::READLINKAT, [1, empty, buffer, 64, 0, 0]),
        Errno::Invalid.as_result()
    );
}

/// `faccessat2` accepts `AT_EMPTY_PATH`, which probes a descriptor the
/// caller already holds.
#[test]
fn faccessat2_probes_a_descriptor_through_an_empty_path() {
    let mut fixture = fixture("faccessat2-empty");
    let empty = fixture.arena.place(b"\0");
    let fd = fixture.opened("/script");
    assert_eq!(
        fixture.call(
            number::FACCESSAT2,
            [fd, empty, access_mode::EXECUTE as i64, at::EMPTY_PATH as i64, 0, 0]
        ),
        0,
        "/script is 0755"
    );
    let plain = fixture.opened("/etc/hosts");
    assert_eq!(
        fixture.call(
            number::FACCESSAT2,
            [plain, empty, access_mode::EXECUTE as i64, at::EMPTY_PATH as i64, 0, 0]
        ),
        Errno::Access.as_result()
    );
    // Standard input is readable and not writable; standard output is the
    // other way round. Answering yes to both would tell a program it can
    // read back what it wrote.
    let probe = |fixture: &mut Fixture, fd: i64, mode: i32| {
        fixture.call(
            number::FACCESSAT2,
            [fd, empty, mode as i64, at::EMPTY_PATH as i64, 0, 0],
        )
    };
    assert_eq!(probe(&mut fixture, 0, access_mode::READ), 0);
    assert_eq!(probe(&mut fixture, 0, access_mode::WRITE), Errno::Access.as_result());
    assert_eq!(probe(&mut fixture, 1, access_mode::WRITE), 0);
    assert_eq!(probe(&mut fixture, 1, access_mode::READ), Errno::Access.as_result());
    assert_eq!(probe(&mut fixture, 1, 0), 0, "it exists");
}

/// A directory cookie is 64 bits wide and this kernel counts entries in 32.
/// Narrowing it wrapped 4 GiB back to `.` and re-listed the directory, so a
/// reader with 64-bit cookies never reached the end.
#[test]
fn a_directory_offset_past_every_cookie_is_the_end() {
    let mut fixture = fixture("dirent-cookie");
    let fd = fixture.open("/etc", open_flags::DIRECTORY);
    assert!(fd >= 0);
    let buffer = fixture.arena.buffer(1024);
    assert_eq!(
        fixture.call(number::LSEEK, [fd, 1 << 32, seek::SET as i64, 0, 0, 0]),
        1 << 32
    );
    assert_eq!(
        fixture.call(number::GETDENTS64, [fd, buffer, 1024, 0, 0, 0]),
        0,
        "past every cookie the listing is over"
    );
    // And a cookie inside the range still resumes where it should.
    assert_eq!(fixture.call(number::LSEEK, [fd, 2, seek::SET as i64, 0, 0, 0]), 2);
    assert!(fixture.call(number::GETDENTS64, [fd, buffer, 1024, 0, 0, 0]) > 0);
}

// ---- the mount table -------------------------------------------------------

/// A name that has a filesystem mounted over it reaches that filesystem, and
/// the directory it covers becomes unreachable — which is what a mount point
/// means and the whole reason resolution names a *vnode* rather than an
/// inode.
#[test]
fn a_mount_point_reaches_the_filesystem_mounted_over_it() {
    let mut fixture = fixture("mount-cross");
    // Before: `/empty` is the covering tree's own empty directory.
    let buffer = fixture.arena.buffer(256);
    let empty = fixture.open("/empty", open_flags::DIRECTORY);
    assert!(empty >= 0);
    let before = fixture.call(number::GETDENTS64, [empty, buffer, 256, 0, 0, 0]);
    assert!(fixture.stat(number::STAT, "/empty/payload").is_err());

    fixture.mount("/empty", "cross");

    // After: the same name reaches the other filesystem's root.
    let stat = fixture
        .stat(number::STAT, "/empty/payload")
        .expect("the mounted file");
    assert_eq!(field_u64(&stat, 48), 28, "the mounted payload's size");
    let fd = fixture.opened("/empty/payload");
    let contents = fixture.arena.buffer(64);
    assert_eq!(fixture.call(number::READ, [fd, contents, 64, 0, 0, 0]), 28);
    assert_eq!(
        fixture.arena.read(contents, 28),
        b"from the mounted filesystem\n"
    );

    // The covered directory's own listing is gone: only `.` and `..` were
    // ever in it, and now the listing is the mount's.
    let after_fd = fixture.open("/empty", open_flags::DIRECTORY);
    let after = fixture.call(number::GETDENTS64, [after_fd, buffer, 256, 0, 0, 0]);
    assert!(
        after > before,
        "the mounted root lists more than the empty directory it covers"
    );
}

/// `st_dev` is what makes two files in two filesystems distinguishable when
/// their inode numbers collide, which across two independently baked images
/// they routinely do. `find -xdev`, `du`'s deduplication and every hardlink
/// detector are built on the pair.
#[test]
fn two_mounts_report_different_devices() {
    let mut fixture = fixture("mount-device");
    fixture.mount("/empty", "device");
    let below = fixture.stat(number::STAT, "/etc/hosts").expect("stat");
    let above = fixture.stat(number::STAT, "/empty/payload").expect("stat");
    assert_ne!(
        field_u64(&below, 0),
        field_u64(&above, 0),
        "two filesystems, two device numbers"
    );
    // The root of the mount and the directory it covers are different files
    // in every sense, including which device they are on.
    let root = fixture.stat(number::STAT, "/empty").expect("stat");
    assert_eq!(field_u64(&root, 0), field_u64(&above, 0));

    // `statx` reports the same split, in its own two fields.
    let path = fixture.arena.path("/empty/payload");
    let buffer = fixture.arena.buffer(STATX_SIZE);
    assert_eq!(
        fixture.call(number::STATX, [at::FDCWD, path, 0, 0x7ff, buffer, 0]),
        0
    );
    let statx = fixture.arena.read(buffer, STATX_SIZE).to_vec();
    let device = field_u64(&above, 0);
    assert_eq!(field_u32(&statx, 136) as u64, device >> 8);
    assert_eq!(field_u32(&statx, 140) as u64, device & 0xff);
}

/// `..` at the root of a mounted filesystem leaves it. Getting this wrong
/// traps a process inside the mount, because the root of every filesystem is
/// its own parent.
#[test]
fn dot_dot_at_a_mount_root_leaves_the_filesystem() {
    let mut fixture = fixture("mount-parent");
    fixture.mount("/empty", "parent");

    // Up and back down again, crossing out of the mount and into the tree
    // below it.
    let stat = fixture
        .stat(number::STAT, "/empty/../etc/hosts")
        .expect("../ left the mount");
    assert_eq!(field_u64(&stat, 48), 20);

    // From deeper inside, two levels of `..` land in the same place.
    let deep = fixture
        .stat(number::STAT, "/empty/inner/../../etc/hosts")
        .expect("two levels");
    assert_eq!(field_u64(&deep, 48), 20);

    // And `..` from the mount root arrives at the *covering* directory's
    // parent, which is the real root — not at the mount root again.
    let up = fixture.stat(number::STAT, "/empty/..").expect("stat");
    let root = fixture.stat(number::STAT, "/").expect("stat");
    assert_eq!(field_u64(&up, 0), field_u64(&root, 0), "same device");
    assert_eq!(field_u64(&up, 8), field_u64(&root, 8), "same inode");

    // A relative symlink inside the mount resolves inside it.
    let stat = fixture.stat(number::STAT, "/empty/inner/up").expect("stat");
    assert_eq!(field_u64(&stat, 48), 28, "the mount's own payload");
}

/// `getcwd` inside a mounted filesystem reports the path a caller can hand
/// back, which means naming the mount point rather than the mount's own root.
#[test]
fn getcwd_inside_a_mount_names_the_path_that_reaches_it() {
    let mut fixture = fixture("mount-getcwd");
    fixture.mount("/empty", "getcwd");
    let path = fixture.arena.path("/empty/inner");
    assert_eq!(fixture.call(number::CHDIR, [path, 0, 0, 0, 0, 0]), 0);
    let buffer = fixture.arena.buffer(256);
    let length = fixture.call(number::GETCWD, [buffer, 256, 0, 0, 0, 0]);
    assert_eq!(length, "/empty/inner".len() as i64 + 1);
    assert_eq!(
        fixture.arena.read(buffer, length as usize),
        b"/empty/inner\0"
    );
    // And a relative path from there resolves within the mount.
    let relative = fixture.arena.path("deep");
    let stat = fixture.arena.buffer(STAT_SIZE);
    assert_eq!(fixture.call(number::STAT, [relative, stat, 0, 0, 0, 0]), 0);
    assert_eq!(field_u64(fixture.arena.read(stat, STAT_SIZE), 48), 7);
}

/// What the table refuses, and why each refusal is not the same as any other.
#[test]
fn the_mount_table_refuses_what_it_cannot_represent() {
    let fixture = fixture("mount-refusals");
    let mut kernel = fixture.kernel;
    let tree = mounted_tree("refusals");
    let baked: &'static baker::Image =
        Box::leak(Box::new(baker::bake_directory(&tree.root).expect("bake")));
    let image = kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse");

    let root = kernel.vfs.root();
    let file = kernel
        .vfs
        .resolve(root, b"/etc/hosts", Lookup::FOLLOW)
        .expect("resolve");
    assert_eq!(
        kernel.vfs.mounts_mut().attach(file, image),
        Err(Errno::NotDir),
        "a regular file is not a mount point"
    );

    let empty = kernel
        .vfs
        .resolve(root, b"/empty", Lookup::FOLLOW)
        .expect("resolve");
    assert!(kernel.vfs.mounts_mut().attach(empty, image).is_ok());
    assert_eq!(
        kernel.vfs.mounts_mut().attach(empty, image),
        Err(Errno::Busy),
        "stacking a second filesystem on one directory is refused rather \
         than silently shadowing the first"
    );

    // The table is finite and says so, rather than overrunning.
    let mut at = kernel.vfs.root();
    let mut attached = kernel.vfs.mounts().count();
    while attached < kisal::mount::MAX_MOUNTS {
        // Mount each new filesystem on the previous one's root, which is not
        // stacking: the covered directory is a different vnode each time.
        at = kernel.vfs.mounts().root_of(attached as u8 - 1).expect("root");
        kernel
            .vfs
            .mounts_mut()
            .attach(at, image)
            .expect("room in the table");
        attached += 1;
    }
    let last = kernel
        .vfs
        .mounts()
        .root_of(kisal::mount::MAX_MOUNTS as u8 - 1)
        .expect("root");
    assert_eq!(
        kernel.vfs.mounts_mut().attach(last, image),
        Err(Errno::NoMemory)
    );
    let _ = at;
}

/// `statx` advertises exactly what it filled. Birth time is the one field it
/// exists to add over `stat`, and this filesystem does not have one: a caller
/// asks *because* `stat` could not answer, and the mask is how it finds out.
#[test]
fn statx_does_not_claim_a_birth_time_it_does_not_have() {
    let mut fixture = fixture("statx-btime");
    let path = fixture.arena.path("/etc/hosts");
    let buffer = fixture.arena.buffer(STATX_SIZE);
    assert_eq!(
        fixture.call(number::STATX, [at::FDCWD, path, 0, 0x7ff, buffer, 0]),
        0
    );
    let statx = fixture.arena.read(buffer, STATX_SIZE);
    const STATX_BTIME: u32 = 0x800;
    assert_eq!(
        field_u32(statx, 0) & STATX_BTIME,
        0,
        "the mask must not advertise a birth time"
    );
    assert_eq!(field_u64(statx, 80), 0, "and none is written");
    // The three it does have are all the modification time, because an OCI
    // layer is a tar archive and tar carries one timestamp.
    let mtime = field_u64(statx, 112);
    assert_ne!(mtime, 0);
    assert_eq!(field_u64(statx, 64), mtime, "atime");
    assert_eq!(field_u64(statx, 96), mtime, "ctime");
    // `stat` agrees with `statx` about all three.
    let stat = fixture.stat(number::STAT, "/etc/hosts").expect("stat");
    assert_eq!(field_u64(&stat, 72), mtime);
    assert_eq!(field_u64(&stat, 88), mtime);
    assert_eq!(field_u64(&stat, 104), mtime);
}

// ---- the allocation-free claim, on the real path ---------------------------

// Per thread, not global: `cargo test` runs these in parallel, so a shared
// counter measures every other test's allocations as well as this one's.
// `const` initialisation matters too — a lazily initialised thread-local
// would allocate inside the allocator.
thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

impl Counting {
    fn record() {
        // `try_with` rather than `with`: during thread teardown the local is
        // gone, and an allocator that panics there takes the process with it.
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record();
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations_during<R>(body: impl FnOnce() -> R) -> (R, usize) {
    let before = ALLOCATIONS.try_with(Cell::get).unwrap_or(0);
    let value = body();
    let after = ALLOCATIONS.try_with(Cell::get).unwrap_or(0);
    (value, after - before)
}

/// The claim the whole filesystem design rests on, measured on the path that
/// ships: `stat`, `open`, `read`, `getdents64` and `readlink` driven through
/// `Kernel::dispatch`, hits and misses alike.
///
/// The miss matters more than the hit. It is most of a real workload's
/// filesystem traffic — an interpreter walking its module path — and it is
/// where a naive implementation builds a string to say what it did not find.
#[test]
fn the_syscall_path_allocates_nothing() {
    let mut fixture = fixture("allocation");
    // Addresses first: the arena's own bookkeeping is not what is measured.
    let hit = fixture.arena.path("/usr/lib/libthing.so");
    let miss = fixture.arena.path("/usr/lib/nothing-here");
    let through_link = fixture.arena.path("/lib/libthing.so");
    let up = fixture.arena.path("/usr/lib/../../etc/hosts");
    let link = fixture.arena.path("/hosts-link");
    let directory = fixture.arena.path("/etc");
    let stat = fixture.arena.buffer(STAT_SIZE);
    let statx = fixture.arena.buffer(STATX_SIZE);
    let scratch = fixture.arena.buffer(2048);

    // Warm anything lazy before measuring, so the count is the work itself.
    fixture.call(number::STAT, [hit, stat, 0, 0, 0, 0]);

    let (calls, allocations) = allocations_during(|| {
        let mut calls = 0;
        for _ in 0..50 {
            assert_eq!(fixture.call(number::STAT, [hit, stat, 0, 0, 0, 0]), 0);
            assert_eq!(
                fixture.call(number::STAT, [miss, stat, 0, 0, 0, 0]),
                Errno::NoEntry.as_result()
            );
            assert_eq!(fixture.call(number::STAT, [through_link, stat, 0, 0, 0, 0]), 0);
            assert_eq!(fixture.call(number::STAT, [up, stat, 0, 0, 0, 0]), 0);
            assert_eq!(fixture.call(number::LSTAT, [link, stat, 0, 0, 0, 0]), 0);
            assert_eq!(
                fixture.call(number::STATX, [at::FDCWD, hit, 0, 0x7ff, statx, 0]),
                0
            );
            assert_eq!(fixture.call(number::READLINK, [link, scratch, 64, 0, 0, 0]), 9);

            let fd = fixture.call(number::OPEN, [hit, 0, 0, 0, 0, 0]);
            assert!(fd >= 0);
            assert!(fixture.call(number::READ, [fd, scratch, 2048, 0, 0, 0]) > 0);
            assert_eq!(fixture.call(number::CLOSE, [fd, 0, 0, 0, 0, 0]), 0);

            let dir = fixture.call(
                number::OPEN,
                [directory, open_flags::DIRECTORY as i64, 0, 0, 0, 0],
            );
            assert!(dir >= 0);
            assert!(fixture.call(number::GETDENTS64, [dir, scratch, 2048, 0, 0, 0]) > 0);
            assert_eq!(fixture.call(number::CLOSE, [dir, 0, 0, 0, 0, 0]), 0);
            calls += 12;
        }
        calls
    });
    assert_eq!(calls, 600);
    assert_eq!(
        allocations, 0,
        "the syscall path allocated {allocations} times over 600 calls"
    );
}

/// The instrument for the test above: a deliberate allocation is seen.
/// Without this, a counter that silently stopped counting would read as a
/// clean bill of health.
#[test]
fn the_allocation_counter_notices_an_allocation() {
    let (_, allocations) = allocations_during(|| {
        let planted: Vec<u8> = Vec::with_capacity(64);
        std::hint::black_box(planted);
    });
    assert!(
        allocations > 0,
        "the counting allocator did not see a planted allocation"
    );
}

// ---- the edges of the rows that have them ---------------------------------

#[test]
fn pread_refuses_what_it_cannot_mean() {
    let mut fixture = fixture("pread-edges");
    let fd = fixture.opened("/etc/hosts");
    let buffer = fixture.arena.buffer(64);
    // A negative offset has no meaning: `EINVAL`. A negative *count* is a
    // huge unsigned one, and Linux answers `EFAULT` for it — no buffer is
    // that big — rather than clamping it to the file's length and reporting
    // a cheerful short read. Both verified against this machine's kernel,
    // including the order: the buffer is checked before the offset.
    assert_eq!(
        fixture.call(number::PREAD64, [fd, buffer, 8, -1, 0, 0]),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::PREAD64, [fd, buffer, -1, 0, 0, 0]),
        Errno::Fault.as_result()
    );
    assert_eq!(
        fixture.call(number::PREAD64, [99, buffer, 8, 0, 0, 0]),
        Errno::BadFile.as_result()
    );
    // Past the end is zero, not an error — and the description's own offset
    // is untouched, which is the whole difference from `read`.
    assert_eq!(fixture.call(number::PREAD64, [fd, buffer, 8, 1000, 0, 0]), 0);
    assert_eq!(fixture.call(number::PREAD64, [fd, buffer, 8, 16, 0, 0]), 4);
    assert_eq!(
        fixture.call(number::LSEEK, [fd, 0, seek::CURRENT as i64, 0, 0, 0]),
        0,
        "`pread` does not move the file position"
    );
    // A count larger than the buffer is `EFAULT` for the same reason, and
    // in particular it is not silently narrowed into a small one by a
    // 32-bit `usize`.
    assert_eq!(
        fixture.call(number::PREAD64, [fd, buffer, 1 << 40, 0, 0, 0]),
        Errno::Fault.as_result()
    );
    // A count that merely runs off the end of the *file* is a short read.
    assert_eq!(fixture.call(number::PREAD64, [fd, buffer, 64, 0, 0, 0]), 20);
    assert_eq!(
        fixture.call(number::PREAD64, [fd, buffer, 8, 1 << 33, 0, 0]),
        0,
        "an offset past four gigabytes is past the end, not back at the start"
    );
}

#[test]
fn lseek_refuses_an_offset_that_cannot_exist() {
    let mut fixture = fixture("lseek-edges");
    let fd = fixture.opened("/etc/hosts");
    assert_eq!(
        fixture.call(number::LSEEK, [fd, -1, seek::SET as i64, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    // An addition that would overflow is refused rather than wrapped into a
    // small offset that looks like a successful seek. A large offset on its
    // own is not refused: filesystems differ about their maximum — ext4
    // refuses this one and tmpfs accepts it — and a directory's `d_off`
    // cookies are not byte positions at all.
    assert_eq!(fixture.call(number::LSEEK, [fd, i64::MAX, seek::SET as i64, 0, 0, 0]), i64::MAX);
    assert_eq!(
        fixture.call(number::LSEEK, [fd, i64::MAX, seek::CURRENT as i64, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::LSEEK, [fd, 7, 99, 0, 0, 0]),
        Errno::Invalid.as_result(),
        "an unknown `whence`"
    );
    // Seeking past the end is legal and reading there is end of file.
    assert_eq!(fixture.call(number::LSEEK, [fd, 100, seek::SET as i64, 0, 0, 0]), 100);
    let buffer = fixture.arena.buffer(8);
    assert_eq!(fixture.call(number::READ, [fd, buffer, 8, 0, 0, 0]), 0);
}

#[test]
fn the_duplicating_rows_refuse_what_linux_refuses() {
    let mut fixture = fixture("dup-edges");
    let fd = fixture.opened("/etc/hosts");
    // `dup2` onto itself validates the descriptor and changes nothing…
    assert_eq!(fixture.call(number::DUP2, [fd, fd, 0, 0, 0, 0]), fd);
    // …including a descriptor that is not open, which is `EBADF`.
    assert_eq!(
        fixture.call(number::DUP2, [99, 99, 0, 0, 0, 0]),
        Errno::BadFile.as_result()
    );
    // `dup3` makes the same case `EINVAL`, which is why it exists: there is
    // no way to apply `O_CLOEXEC` to a no-op.
    assert_eq!(
        fixture.call(number::DUP3, [fd, fd, FD_CLOEXEC as i64, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    // And it takes `O_CLOEXEC` and nothing else.
    assert_eq!(
        fixture.call(number::DUP3, [fd, 20, open_flags::APPEND as i64, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::DUP2, [fd, 1 << 20, 0, 0, 0, 0]),
        Errno::BadFile.as_result(),
        "a target outside the descriptor space"
    );
    assert_eq!(
        fixture.call(number::DUP, [99, 0, 0, 0, 0, 0]),
        Errno::BadFile.as_result()
    );
}

#[test]
fn fcntl_reports_and_changes_only_what_it_can() {
    let mut fixture = fixture("fcntl-more");
    let fd = fixture.opened("/etc/hosts");
    // `O_LARGEFILE` is forced on for every descriptor on a 64-bit kernel,
    // and its absence is how a caller concludes it is on a 32-bit one.
    let flags = fixture.call(number::FCNTL, [fd, fcntl_command::GETFL as i64, 0, 0, 0, 0]);
    assert_eq!(flags, O_LARGEFILE as i64);

    // `F_SETFL` changes the status bits it is allowed to and silently
    // ignores the rest, which is what Linux does — the access mode and
    // `O_DIRECTORY` are properties of the open, not settings.
    assert_eq!(
        fixture.call(
            number::FCNTL,
            [
                fd,
                fcntl_command::SETFL as i64,
                (open_flags::APPEND | open_flags::WRITE_ONLY) as i64,
                0,
                0,
                0
            ]
        ),
        0
    );
    let flags = fixture.call(number::FCNTL, [fd, fcntl_command::GETFL as i64, 0, 0, 0, 0]);
    assert_eq!(flags, (O_LARGEFILE | open_flags::APPEND) as i64);
    let buffer = fixture.arena.buffer(8);
    assert_eq!(
        fixture.call(number::READ, [fd, buffer, 8, 0, 0, 0]),
        8,
        "the descriptor is still readable: `F_SETFL` cannot change that"
    );

    // A command Linux does not have either is `EINVAL`, and one it does have
    // is a named fault — the difference is the whole point.
    assert_eq!(
        fixture.call(number::FCNTL, [fd, 999, 0, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::FCNTL, [99, fcntl_command::GETFD as i64, 0, 0, 0, 0]),
        Errno::BadFile.as_result()
    );
}

#[test]
fn getcwd_answers_by_the_room_it_was_given() {
    let mut fixture = fixture("getcwd-edges");
    let buffer = fixture.arena.buffer(256);
    // At the root the answer is "/" and one byte of terminator.
    assert_eq!(fixture.call(number::GETCWD, [buffer, 256, 0, 0, 0, 0]), 2);
    assert_eq!(fixture.arena.read(buffer, 2), b"/\0");
    // Exactly enough room is enough.
    assert_eq!(fixture.call(number::GETCWD, [buffer, 2, 0, 0, 0, 0]), 2);
    // One byte short is `ERANGE`, which is what gnulib's replacement probes
    // for; `EINVAL` would make it conclude the call is unusable.
    assert_eq!(
        fixture.call(number::GETCWD, [buffer, 1, 0, 0, 0, 0]),
        Errno::Range.as_result()
    );
    // Linux's `size` is unsigned, so there is no negative case to reject:
    // zero is `ERANGE` like any other buffer too small.
    assert_eq!(
        fixture.call(number::GETCWD, [buffer, 0, 0, 0, 0, 0]),
        Errno::Range.as_result()
    );

    // Somewhere with a longer name, so the boundary is not just "/".
    let path = fixture.arena.path("/usr/lib");
    assert_eq!(fixture.call(number::CHDIR, [path, 0, 0, 0, 0, 0]), 0);
    assert_eq!(fixture.call(number::GETCWD, [buffer, 256, 0, 0, 0, 0]), 9);
    assert_eq!(fixture.arena.read(buffer, 9), b"/usr/lib\0");
    assert_eq!(fixture.call(number::GETCWD, [buffer, 9, 0, 0, 0, 0]), 9);
    assert_eq!(
        fixture.call(number::GETCWD, [buffer, 8, 0, 0, 0, 0]),
        Errno::Range.as_result()
    );
}

/// A tree deeper than the chain `getcwd` can walk is `ENAMETOOLONG`, not a
/// truncated path and not a loop. Nothing in a container image is this deep;
/// the branch exists because a corrupt index could be.
#[test]
fn a_directory_too_deep_to_name_is_refused() {
    let root = std::env::temp_dir().join(format!(
        "kisal-deep-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut deepest = root.clone();
    for _ in 0..140 {
        deepest = deepest.join("d");
    }
    std::fs::create_dir_all(&deepest).expect("mkdir");
    let tree = Tree { root };
    let baked: &'static baker::Image =
        Box::leak(Box::new(baker::bake_directory(&tree.root).expect("bake")));
    let image = kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse");
    let arena = Arena::new();
    let mut fixture = Fixture {
        kernel: Kernel::new(
            ConsoleStore::default(),
            Registers {
                segment_base: 0,
                memory_limit: arena.limit(),
                ceiling: arena.limit(),
            },
            image,
        ),
        arena,
        _tree: tree,
    };

    // A hundred levels down is still nameable.
    let shallow = "/d".repeat(100);
    let path = fixture.arena.path(&shallow);
    assert_eq!(fixture.call(number::CHDIR, [path, 0, 0, 0, 0, 0]), 0);
    let buffer = fixture.arena.buffer(512);
    assert_eq!(
        fixture.call(number::GETCWD, [buffer, 512, 0, 0, 0, 0]),
        shallow.len() as i64 + 1
    );

    // A hundred and forty is past the chain the walk can hold.
    let deep = "/d".repeat(140);
    let path = fixture.arena.path(&deep);
    assert_eq!(fixture.call(number::CHDIR, [path, 0, 0, 0, 0, 0]), 0);
    assert_eq!(
        fixture.call(number::GETCWD, [buffer, 512, 0, 0, 0, 0]),
        Errno::NameTooLong.as_result()
    );
}

// ---- device nodes ----------------------------------------------------------

/// A character device baked into the image: reported as one by `stat`,
/// `statx` and `getdents64`, and refused by name when read.
///
/// The node is made by patching a baked inode rather than by `mknod`, which
/// needs `CAP_MKNOD` and is refused in every unprivileged container this
/// could run in. That is not a shortcut around the real path: the index is
/// what kisal reads, the patch writes it with the same `Inode::write` the
/// baker uses, and without it the whole device branch — `of_mode`'s
/// `CHARACTER` arm, `st_rdev`, `stx_rdev_major`/`minor`, and `read`'s
/// refusal — is code with no test anywhere.
#[test]
fn a_character_device_in_the_image_reports_as_one() {
    // `/dev/console` is (5, 1); a minor above 255 is the case Linux's packed
    // encoding exists for, and the one a naive shift gets wrong.
    for (major, minor) in [(5u32, 1u32), (8, 300)] {
        let tree = tree(&format!("chardev-{major}-{minor}"));
        let mut baked = baker::bake_directory(&tree.root).expect("bake");
        patch_inode_to_device(&mut baked, "pipe", major, minor);

        let baked: &'static baker::Image = Box::leak(Box::new(baked));
        let image = kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse");
        let arena = Arena::new();
        let mut fixture = Fixture {
            kernel: Kernel::new(
                ConsoleStore::default(),
                Registers {
                    segment_base: 0,
                    memory_limit: arena.limit(),
                    ceiling: arena.limit(),
                },
                image,
            ),
            arena,
            _tree: tree,
        };

        let stat = fixture.stat(number::STAT, "/etc/pipe").expect("stat");
        assert_eq!(field_u32(&stat, 24) & 0o170000, 0o020000, "S_IFCHR");
        // `st_rdev` is Linux's packed encoding, which is not a plain shift:
        // major is bits 8..20, and minor is bits 0..8 *and* 20..32.
        let expected = ((major as u64 & 0xfff) << 8)
            | (minor as u64 & 0xff)
            | ((minor as u64 & 0xfff00) << 12);
        assert_eq!(field_u64(&stat, 40), expected, "st_rdev");

        // `statx` splits the same number back into two fields, and getting
        // the decomposition wrong is invisible until a minor exceeds 255.
        let path = fixture.arena.path("/etc/pipe");
        let buffer = fixture.arena.buffer(STATX_SIZE);
        assert_eq!(
            fixture.call(number::STATX, [at::FDCWD, path, 0, 0x7ff, buffer, 0]),
            0
        );
        let statx = fixture.arena.read(buffer, STATX_SIZE);
        assert_eq!(field_u32(statx, 128), major, "stx_rdev_major");
        assert_eq!(field_u32(statx, 132), minor, "stx_rdev_minor");

        // The listing says character device, precomputed at bake time.
        let directory = fixture.open("/etc", open_flags::DIRECTORY);
        let listing = fixture.arena.buffer(1024);
        let written = fixture.call(number::GETDENTS64, [directory, listing, 1024, 0, 0, 0]);
        assert!(written > 0);
        let entries = fixture.arena.read(listing, written as usize).to_vec();
        assert_eq!(
            entry_type_of(&entries, b"pipe"),
            Some(2),
            "DT_CHR"
        );

        // And reading it is refused by name: there is no driver behind a
        // node in an image, and `EINVAL` would be a plausible answer to a
        // perfectly valid call.
        let fd = fixture.opened("/etc/pipe");
        let into = fixture.arena.buffer(16);
        assert_eq!(
            fixture.call(number::READ, [fd, into, 16, 0, 0, 0]),
            Errno::NoDevice.as_result()
        );
    }
}

/// Rewrites one of `/etc`'s entries into a device node, in the index the
/// kernel reads, using the same writer the baker uses.
fn patch_inode_to_device(baked: &mut baker::Image, name: &str, major: u32, minor: u32) {
    let (number, position) = {
        let image = kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse");
        let root = image.inode(image.root()).expect("root");
        let etc = image
            .lookup(&root, b"etc")
            .expect("lookup")
            .expect("/etc exists");
        let etc = image.inode(etc.inode).expect("inode");
        let count = image.entry_count(&etc).expect("count");
        let position = (0..count)
            .find(|position| {
                let entry = image.entry(&etc, *position).expect("entry");
                image.string(entry.name_ref).expect("name") == name.as_bytes()
            })
            .expect("the entry exists");
        let entry = image.entry(&etc, position).expect("entry");
        // A directory's payload names its entry block in the dirent region:
        // a four-byte count, then the array. So the entry's own bytes are at
        // a known place.
        (
            entry.inode,
            etc.payload as usize + 4 + position as usize * kisal::image::DIRENT_SIZE,
        )
    };

    let inode_offset =
        u32::from_le_bytes(baked.index[12..16].try_into().expect("four bytes")) as usize;
    let at = inode_offset + number as usize * kisal::image::INODE_SIZE;

    let mut inode = {
        let image = kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse");
        image.inode(number).expect("inode")
    };
    inode.mode = 0o020644;
    // Linux's `dev_t`: minor's low eight bits, major's twelve above them,
    // and minor's remaining twelve above that.
    inode.payload = ((major as u64 & 0xfff) << 8)
        | (minor as u64 & 0xff)
        | ((minor as u64 & 0xfff00) << 12);
    let mut record = [0u8; kisal::image::INODE_SIZE];
    inode.write(&mut record);
    baked.index[at..at + kisal::image::INODE_SIZE].copy_from_slice(&record);

    // The dirent carries a `d_type` precomputed at bake time, so it has to
    // agree — an image where the two disagree is one the baker cannot
    // produce, and testing against one would prove nothing.
    let dirent_offset =
        u32::from_le_bytes(baked.index[16..20].try_into().expect("four bytes")) as usize;
    baked.index[dirent_offset + position + 8] =
        kisal::image::directory_entry_type::of_mode(inode.mode);
}

/// The `d_type` of a named entry in a `getdents64` buffer.
fn entry_type_of(entries: &[u8], name: &[u8]) -> Option<u8> {
    let mut at = 0;
    while at + 19 <= entries.len() {
        let length = u16::from_le_bytes(entries[at + 16..at + 18].try_into().expect("two")) as usize;
        if length == 0 {
            return None;
        }
        let bytes = &entries[at + 19..at + length];
        let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
        if &bytes[..end] == name {
            return Some(entries[at + 18]);
        }
        at += length;
    }
    None
}

/// An index whose parent pointer and entries disagree is a corrupt image, and
/// `getcwd` says so rather than looping or naming the wrong directory.
///
/// Unreachable from a bake — the baker writes both from the same walk — so
/// the index is corrupted deliberately. The branch exists because the
/// tarball path will build parents from archive records, and a claim that
/// resolution "never panics on a malformed index" is worth what its
/// demonstration is worth.
#[test]
fn a_directory_whose_parent_disowns_it_is_refused() {
    let tree = tree("orphan");
    let mut baked = baker::bake_directory(&tree.root).expect("bake");
    // Point `/usr/lib`'s parent at the root, which has no entry for it.
    let number = {
        let image = kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse");
        let root = image.inode(image.root()).expect("root");
        let usr = image.lookup(&root, b"usr").expect("lookup").expect("/usr");
        let usr = image.inode(usr.inode).expect("inode");
        image
            .lookup(&usr, b"lib")
            .expect("lookup")
            .expect("/usr/lib")
            .inode
    };
    let inode_offset =
        u32::from_le_bytes(baked.index[12..16].try_into().expect("four bytes")) as usize;
    let at = inode_offset + number as usize * kisal::image::INODE_SIZE;
    let mut inode = {
        let image = kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse");
        image.inode(number).expect("inode")
    };
    inode.parent = kisal::image::Image::parse(&baked.index, &baked.blob)
        .expect("parse")
        .root();
    let mut record = [0u8; kisal::image::INODE_SIZE];
    inode.write(&mut record);
    baked.index[at..at + kisal::image::INODE_SIZE].copy_from_slice(&record);

    let baked: &'static baker::Image = Box::leak(Box::new(baked));
    let image = kisal::image::Image::parse(&baked.index, &baked.blob).expect("parse");
    let arena = Arena::new();
    let mut fixture = Fixture {
        kernel: Kernel::new(
            ConsoleStore::default(),
            Registers {
                segment_base: 0,
                memory_limit: arena.limit(),
                ceiling: arena.limit(),
            },
            image,
        ),
        arena,
        _tree: tree,
    };

    let path = fixture.arena.path("/usr/lib");
    assert_eq!(fixture.call(number::CHDIR, [path, 0, 0, 0, 0, 0]), 0);
    let buffer = fixture.arena.buffer(256);
    assert_eq!(
        fixture.call(number::GETCWD, [buffer, 256, 0, 0, 0, 0]),
        Errno::NoEntry.as_result()
    );
}

// ---- the synthetic mounts --------------------------------------------------

/// `/dev` is a mount, not a directory in the image — which is what it is on
/// every real system, and why a base image ships an empty one.
#[test]
fn dev_is_mounted_over_the_directory_the_image_provides() {
    let mut fixture = fixture("dev-mount");
    for (name, major, minor) in [
        ("null", 1u32, 3u32),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
    ] {
        let stat = fixture
            .stat(number::STAT, &format!("/dev/{name}"))
            .unwrap_or_else(|errno| panic!("/dev/{name} is missing: {errno}"));
        assert_eq!(field_u32(&stat, 24) & 0o170000, 0o020000, "{name} is a chardev");
        assert_eq!(field_u32(&stat, 24) & 0o777, 0o666, "{name} is 0666");
        assert_eq!(
            field_u64(&stat, 40),
            ((major as u64) << 8) | minor as u64,
            "{name}'s st_rdev"
        );
        assert_eq!(field_u64(&stat, 48), 0, "{name} has no size");
    }

    // It is a different filesystem, and `st_dev` says so.
    let node = fixture.stat(number::STAT, "/dev/null").expect("stat");
    let image_file = fixture.stat(number::STAT, "/etc/hosts").expect("stat");
    assert_ne!(field_u64(&node, 0), field_u64(&image_file, 0));

    // `..` leaves it, as it leaves any mount.
    let up = fixture.stat(number::STAT, "/dev/..").expect("stat");
    let root = fixture.stat(number::STAT, "/").expect("stat");
    assert_eq!(field_u64(&up, 8), field_u64(&root, 8));

    // And the listing is the mount's, not the empty directory's.
    let directory = fixture.open("/dev", open_flags::DIRECTORY);
    assert!(directory >= 0);
    let buffer = fixture.arena.buffer(1024);
    let written = fixture.call(number::GETDENTS64, [directory, buffer, 1024, 0, 0, 0]);
    assert!(written > 0);
    let entries = fixture.arena.read(buffer, written as usize).to_vec();
    assert_eq!(entry_type_of(&entries, b"null"), Some(2), "DT_CHR");
    assert_eq!(entry_type_of(&entries, b"urandom"), Some(2));
}

/// What the devices do. Every answer here is what the real ones give,
/// checked against them.
#[test]
fn the_devices_read_and_write_as_they_do_on_linux() {
    let mut fixture = fixture("dev-drivers");

    // `/dev/null`: reads nothing, accepts everything.
    let null = fixture.open("/dev/null", open_flags::READ_WRITE);
    assert!(null >= 0, "/dev/null must open for writing on a read-only image");
    let buffer = fixture.arena.buffer(64);
    assert_eq!(fixture.call(number::READ, [null, buffer, 16, 0, 0, 0]), 0);
    assert_eq!(fixture.call(number::WRITE, [null, buffer, 16, 0, 0, 0]), 16);

    // `/dev/zero`: zeros, however much is asked for.
    let planted = fixture.arena.place(&[0xaa; 64]);
    let zero = fixture.open("/dev/zero", open_flags::READ_WRITE);
    assert_eq!(fixture.call(number::READ, [zero, planted, 64, 0, 0, 0]), 64);
    assert_eq!(fixture.arena.read(planted, 64), &[0u8; 64]);
    assert_eq!(fixture.call(number::WRITE, [zero, planted, 16, 0, 0, 0]), 16);

    // `/dev/full`: reads zeros and refuses every write, which is what it is
    // for — programs test their error handling against it.
    let full = fixture.open("/dev/full", open_flags::READ_WRITE);
    assert_eq!(fixture.call(number::READ, [full, buffer, 8, 0, 0, 0]), 8);
    assert_eq!(
        fixture.call(number::WRITE, [full, buffer, 8, 0, 0, 0]),
        Errno::NoSpace.as_result()
    );

    // A device has no position: every seek answers zero and moves nothing.
    assert_eq!(fixture.call(number::LSEEK, [zero, 100, seek::SET as i64, 0, 0, 0]), 0);
    assert_eq!(fixture.call(number::LSEEK, [zero, 0, seek::END as i64, 0, 0, 0]), 0);
    let after = fixture.call(number::READ, [zero, buffer, 8, 0, 0, 0]);
    assert_eq!(after, 8, "the seek did not move the device past its own end");

    // Opening one read-only still works, and writing to that descriptor is
    // `EBADF` — the access mode, not the filesystem, is what refuses.
    let read_only = fixture.opened("/dev/null");
    assert_eq!(
        fixture.call(number::WRITE, [read_only, buffer, 8, 0, 0, 0]),
        Errno::BadFile.as_result()
    );
}

/// `/dev/urandom` is the boot seed expanded, which is what makes a run
/// reproducible and what makes the seed the whole secret.
#[test]
fn urandom_replays_from_the_seed_and_diverges_without_it() {
    let read = |fixture: &mut Fixture, path: &str, count: i64| -> Vec<u8> {
        let fd = fixture.opened(path);
        let buffer = fixture.arena.buffer(count as usize);
        assert_eq!(fixture.call(number::READ, [fd, buffer, count, 0, 0, 0]), count);
        fixture.arena.read(buffer, count as usize).to_vec()
    };

    let mut first = fixture_seeded("urandom-a", Some([1; 32]));
    let mut again = fixture_seeded("urandom-b", Some([1; 32]));
    let mut other = fixture_seeded("urandom-c", Some([2; 32]));
    let a = read(&mut first, "/dev/urandom", 64);
    let b = read(&mut again, "/dev/urandom", 64);
    let c = read(&mut other, "/dev/urandom", 64);
    assert_eq!(a, b, "the same seed replays");
    assert_ne!(a, c, "a different seed does not");
    assert_ne!(a, vec![0u8; 64], "and it is not zeros");

    // The stream advances rather than restarting per read, and crosses the
    // 64-byte block boundary correctly.
    let more = read(&mut first, "/dev/urandom", 64);
    assert_ne!(a, more);

    // `/dev/random` is the same device. Linux stopped distinguishing them.
    let mut fresh = fixture_seeded("urandom-d", Some([1; 32]));
    assert_eq!(read(&mut fresh, "/dev/random", 64), a);

    // A container whose host mounted no `/iso/random` has no entropy, and
    // asking for some is refused rather than answered with zeros.
    let mut blind = fixture_seeded("urandom-none", None);
    let fd = blind.opened("/dev/urandom");
    let buffer = blind.arena.buffer(16);
    assert_eq!(
        blind.call(number::READ, [fd, buffer, 16, 0, 0, 0]),
        Errno::NoDevice.as_result()
    );
    assert_eq!(blind.arena.read(buffer, 16), &[0u8; 16], "and wrote nothing");
}

/// A large read is served without the kernel asking its allocator for a
/// guest-sized buffer.
#[test]
fn a_large_device_read_is_chunked_rather_than_allocated() {
    let mut fixture = fixture("dev-large");
    let zero = fixture.opened("/dev/zero");
    // Bigger than the kernel's chunk, and not a multiple of it.
    let length = 4096 * 3 + 17;
    let buffer = fixture.arena.place(&vec![0xff; length]);
    let (read, allocations) = allocations_during(|| {
        fixture.call(number::READ, [zero, buffer, length as i64, 0, 0, 0])
    });
    assert_eq!(read, length as i64);
    assert_eq!(fixture.arena.read(buffer, length), vec![0u8; length]);
    assert_eq!(allocations, 0, "a device read allocated {allocations} times");
}

/// `/proc/self/exe` is a symlink, and what it points at is a fact about the
/// running program that nothing knows until `execve` does.
#[test]
fn proc_self_exe_is_a_symlink_whose_target_arrives_with_exec() {
    let mut fixture = fixture("proc");
    let stat = fixture.stat(number::LSTAT, "/proc/self/exe").expect("lstat");
    assert_eq!(field_u32(&stat, 24) & 0o170000, 0o120000, "a symlink");

    // Reading it before anything has set the path is a named fault, not an
    // empty string — a program that believed its own executable was called
    // "" would go wrong somewhere far from here.
    let path = fixture.arena.path("/proc/self/exe");
    let buffer = fixture.arena.buffer(256);
    let outcome = fixture.kernel.dispatch(
        number::READLINK,
        Arguments::new([path, buffer, 256, 0, 0, 0]),
    );
    let Outcome::Fault(fault) = outcome else {
        panic!("reading an unset /proc/self/exe produced {outcome:?}");
    };
    let mut message = String::new();
    fault.message(&mut message);
    assert!(message.contains("running executable"), "{message}");

    // Once exec sets it, the link reads back and resolves.
    fixture.kernel.set_executable("/usr/lib/libthing.so");
    let read = fixture.call(number::READLINK, [path, buffer, 256, 0, 0, 0]);
    assert_eq!(read, 20);
    assert_eq!(fixture.arena.read(buffer, 20), b"/usr/lib/libthing.so");
    let followed = fixture.stat(number::STAT, "/proc/self/exe").expect("stat");
    let direct = fixture.stat(number::STAT, "/usr/lib/libthing.so").expect("stat");
    assert_eq!(field_u64(&followed, 48), field_u64(&direct, 48));

    // `/proc/self` is a directory whose `..` is `/proc`.
    let up = fixture.stat(number::STAT, "/proc/self/..").expect("stat");
    let proc = fixture.stat(number::STAT, "/proc").expect("stat");
    assert_eq!(field_u64(&up, 8), field_u64(&proc, 8));

    // `/proc/self/maps` is a view of the address space, and reports size
    // zero as every procfs file does: its length is not known until it is
    // read, and a program that trusted a size would size its buffer wrong.
    let maps = fixture.stat(number::STAT, "/proc/self/maps").expect("stat");
    assert_eq!(field_u32(&maps, 24) & 0o170000, 0o100000, "a regular file");
    assert_eq!(field_u64(&maps, 48), 0, "and no size");

    // Everything else in `/proc` is absent rather than invented.
    assert_eq!(
        fixture.stat(number::STAT, "/proc/self/status").unwrap_err(),
        Errno::NoEntry.as_result()
    );
}

/// Stdio is not a terminal, and every request that asks says so.
#[test]
fn the_terminal_ioctls_answer_enotty() {
    let mut fixture = fixture("ioctl");
    let buffer = fixture.arena.buffer(64);
    for fd in [0i64, 1, 2] {
        assert_eq!(
            fixture.call(number::IOCTL, [fd, 0x5401, buffer, 0, 0, 0]),
            Errno::NoTty.as_result(),
            "TCGETS on fd {fd}"
        );
    }
    let file = fixture.opened("/etc/hosts");
    for request in [0x5401i64, 0x5402, 0x5403, 0x5404, 0x540f, 0x5410, 0x5413, 0x5414] {
        assert_eq!(
            fixture.call(number::IOCTL, [file, request, buffer, 0, 0, 0]),
            Errno::NoTty.as_result(),
            "request {request:#x}"
        );
    }
    // A closed descriptor is `EBADF` whatever the request would have been.
    assert_eq!(
        fixture.call(number::IOCTL, [99, 0x5401, buffer, 0, 0, 0]),
        Errno::BadFile.as_result()
    );
    // A request with no driver is a named fault: `ioctl` is a thousand
    // unrelated calls behind one number, and `EINVAL` would be a lie about
    // this one.
    let outcome = fixture
        .kernel
        .dispatch(number::IOCTL, Arguments::new([file, 0x1234, buffer, 0, 0, 0]));
    let Outcome::Fault(fault) = outcome else {
        panic!("an unimplemented ioctl produced {outcome:?}");
    };
    let mut message = String::new();
    fault.message(&mut message);
    assert!(message.contains("no driver for"), "{message}");
}

// ---- the writable layer ----------------------------------------------------

/// A file created in the overlay is an ordinary file: it opens, it reads
/// back what was written, and it is there for the next lookup.
#[test]
fn a_created_file_reads_back_what_was_written() {
    let mut fixture = fixture("create");
    let fd = fixture.open("/fresh", open_flags::READ_WRITE | open_flags::CREATE);
    assert!(fd >= 0, "creating /fresh failed with {fd}");

    let message = fixture.arena.place(b"written into the upper layer\n");
    assert_eq!(fixture.call(number::WRITE, [fd, message, 29, 0, 0, 0]), 29);
    assert_eq!(
        fixture.call(number::LSEEK, [fd, 0, seek::SET as i64, 0, 0, 0]),
        0
    );
    let buffer = fixture.arena.buffer(64);
    assert_eq!(fixture.call(number::READ, [fd, buffer, 64, 0, 0, 0]), 29);
    assert_eq!(fixture.arena.read(buffer, 29), b"written into the upper layer\n");

    // And through a fresh path resolution, which is what says it is in the
    // filesystem rather than in the descriptor.
    let stat = fixture.stat(number::STAT, "/fresh").expect("stat");
    assert_eq!(field_u64(&stat, 48), 29, "st_size follows the contents");
    assert_eq!(field_u32(&stat, 24) & 0o170000, 0o100000, "a regular file");
    let again = fixture.opened("/fresh");
    assert_eq!(fixture.call(number::READ, [again, buffer, 64, 0, 0, 0]), 29);

    // It shows up in its directory's listing, in name order.
    let root = fixture.open("/", open_flags::DIRECTORY);
    let listing = fixture.arena.buffer(2048);
    let written = fixture.call(number::GETDENTS64, [root, listing, 2048, 0, 0, 0]);
    assert!(written > 0);
    let entries = fixture.arena.read(listing, written as usize).to_vec();
    assert_eq!(entry_type_of(&entries, b"fresh"), Some(8), "DT_REG");
    // …beside the names that were already there.
    assert_eq!(entry_type_of(&entries, b"etc"), Some(4), "DT_DIR");
}

/// Writing to a file that came from the image copies it up. The image is
/// never changed — it is shared, read-only, between every instance — and the
/// copy is what the guest sees afterwards.
#[test]
fn writing_to_an_image_file_copies_it_up() {
    let mut fixture = fixture("copy-up");
    let before = fixture.stat(number::STAT, "/etc/hosts").expect("stat");
    assert_eq!(field_u64(&before, 48), 20);

    let fd = fixture.open("/etc/hosts", open_flags::WRITE_ONLY);
    assert!(fd >= 0);
    let message = fixture.arena.place(b"CHANGED");
    assert_eq!(fixture.call(number::WRITE, [fd, message, 7, 0, 0, 0]), 7);

    // The file now reads as the change over the original — a write at
    // offset zero replaces the first bytes and leaves the rest.
    let reader = fixture.opened("/etc/hosts");
    let buffer = fixture.arena.buffer(64);
    assert_eq!(fixture.call(number::READ, [reader, buffer, 64, 0, 0, 0]), 20);
    assert_eq!(
        fixture.arena.read(buffer, 20),
        b"CHANGED.1 localhost\n",
        "the write replaced the first bytes and left the rest"
    );

    // The lower layer is untouched: a second overlay over the same image
    // sees the original, which is what makes the image shareable between
    // instances at all.
    let image = *fixture.kernel.vfs.mounts().filesystem(0).expect("root").lower();
    let root = image.inode(image.root()).expect("root");
    let etc = image.lookup(&root, b"etc").expect("lookup").expect("etc");
    let etc = image.inode(etc.inode).expect("inode");
    let hosts = image.lookup(&etc, b"hosts").expect("lookup").expect("hosts");
    let hosts = image.inode(hosts.inode).expect("inode");
    assert_eq!(
        image.contents(&hosts).expect("contents"),
        b"127.0.0.1 localhost\n",
        "the image still holds the original bytes"
    );
}

/// Appending goes to the end as it stands *now*, which is what the flag
/// exists for.
#[test]
fn o_append_writes_at_the_end_every_time() {
    let mut fixture = fixture("append");
    let fd = fixture.open("/log", open_flags::WRITE_ONLY | open_flags::CREATE);
    assert!(fd >= 0);
    let first = fixture.arena.place(b"one\n");
    assert_eq!(fixture.call(number::WRITE, [fd, first, 4, 0, 0, 0]), 4);

    // A second descriptor, opened for append, does not overwrite the first's
    // bytes even though its own offset starts at zero.
    let appender = fixture.open("/log", open_flags::WRITE_ONLY | open_flags::APPEND);
    assert!(appender >= 0);
    let second = fixture.arena.place(b"two\n");
    assert_eq!(fixture.call(number::WRITE, [appender, second, 4, 0, 0, 0]), 4);

    let reader = fixture.opened("/log");
    let buffer = fixture.arena.buffer(32);
    assert_eq!(fixture.call(number::READ, [reader, buffer, 32, 0, 0, 0]), 8);
    assert_eq!(fixture.arena.read(buffer, 8), b"one\ntwo\n");

    // And a seek before an appending write is ignored, which is the part
    // that makes concurrent appends safe.
    assert_eq!(
        fixture.call(number::LSEEK, [appender, 0, seek::SET as i64, 0, 0, 0]),
        0
    );
    let third = fixture.arena.place(b"three\n");
    assert_eq!(fixture.call(number::WRITE, [appender, third, 6, 0, 0, 0]), 6);
    let reader = fixture.opened("/log");
    assert_eq!(fixture.call(number::READ, [reader, buffer, 32, 0, 0, 0]), 14);
    assert_eq!(fixture.arena.read(buffer, 14), b"one\ntwo\nthree\n");
}

#[test]
fn truncate_shortens_and_extends_with_zeros() {
    let mut fixture = fixture("truncate");
    let fd = fixture.open("/file", open_flags::READ_WRITE | open_flags::CREATE);
    let message = fixture.arena.place(b"0123456789");
    assert_eq!(fixture.call(number::WRITE, [fd, message, 10, 0, 0, 0]), 10);

    assert_eq!(fixture.call(number::FTRUNCATE, [fd, 4, 0, 0, 0, 0]), 0);
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/file").expect("stat"), 48),
        4
    );

    // Extending fills with zeros, which is how a sparse file reads back.
    let path = fixture.arena.path("/file");
    assert_eq!(fixture.call(number::TRUNCATE, [path, 8, 0, 0, 0, 0]), 0);
    let reader = fixture.opened("/file");
    let buffer = fixture.arena.buffer(16);
    assert_eq!(fixture.call(number::READ, [reader, buffer, 16, 0, 0, 0]), 8);
    assert_eq!(fixture.arena.read(buffer, 8), b"0123\0\0\0\0");

    assert_eq!(
        fixture.call(number::FTRUNCATE, [fd, -1, 0, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    // `O_TRUNC` at open empties it before the descriptor exists.
    let emptied = fixture.open("/file", open_flags::WRITE_ONLY | open_flags::TRUNCATE);
    assert!(emptied >= 0);
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/file").expect("stat"), 48),
        0
    );
}

/// Deleting a name that came from the image cannot remove the file — the
/// image is shared and read-only — so what is recorded is that the name is
/// gone. Everything above has to agree that it is.
#[test]
fn deleting_an_image_file_whites_it_out() {
    let mut fixture = fixture("whiteout");
    assert!(fixture.stat(number::STAT, "/etc/hosts").is_ok());

    let path = fixture.arena.path("/etc/hosts");
    assert_eq!(fixture.call(number::UNLINK, [path, 0, 0, 0, 0, 0]), 0);

    assert_eq!(
        fixture.stat(number::STAT, "/etc/hosts").unwrap_err(),
        Errno::NoEntry.as_result()
    );
    assert_eq!(
        fixture.open("/etc/hosts", open_flags::READ_ONLY),
        Errno::NoEntry.as_result()
    );
    assert_eq!(
        fixture.call(number::UNLINK, [path, 0, 0, 0, 0, 0]),
        Errno::NoEntry.as_result(),
        "deleting it twice"
    );

    // It is gone from the listing too, and the whiteout itself is not a
    // name anyone can see.
    let directory = fixture.open("/etc", open_flags::DIRECTORY);
    let listing = fixture.arena.buffer(2048);
    let written = fixture.call(number::GETDENTS64, [directory, listing, 2048, 0, 0, 0]);
    let entries = fixture.arena.read(listing, written as usize).to_vec();
    assert_eq!(entry_type_of(&entries, b"hosts"), None);
    assert_eq!(entry_type_of(&entries, b"hostname"), Some(8), "and the rest remain");

    // A name created over the whiteout is a new file, not the old one.
    let fd = fixture.open("/etc/hosts", open_flags::READ_WRITE | open_flags::CREATE);
    assert!(fd >= 0);
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/etc/hosts").expect("stat"), 48),
        0,
        "the new file is empty, not the image's twenty bytes"
    );
}

#[test]
fn directories_are_made_and_removed() {
    let mut fixture = fixture("mkdir");
    let path = fixture.arena.path("/made");
    assert_eq!(fixture.call(number::MKDIR, [path, 0o755, 0, 0, 0, 0]), 0);
    let stat = fixture.stat(number::STAT, "/made").expect("stat");
    assert_eq!(field_u32(&stat, 24) & 0o170000, 0o040000);
    assert_eq!(field_u32(&stat, 24) & 0o777, 0o755);
    assert_eq!(field_u32(&stat, 16), 2, "an empty directory has two links");

    // Twice is `EEXIST`, whatever kind of thing is there.
    assert_eq!(
        fixture.call(number::MKDIR, [path, 0o755, 0, 0, 0, 0]),
        Errno::Exists.as_result()
    );

    // Something in it, so that removing it is refused.
    let inside = fixture.arena.path("/made/file");
    let fd = fixture.open("/made/file", open_flags::WRITE_ONLY | open_flags::CREATE);
    assert!(fd >= 0);
    assert_eq!(
        fixture.call(number::RMDIR, [path, 0, 0, 0, 0, 0]),
        Errno::NotEmpty.as_result()
    );
    // The two rows refuse each other's argument.
    assert_eq!(
        fixture.call(number::UNLINK, [path, 0, 0, 0, 0, 0]),
        Errno::IsDir.as_result()
    );
    assert_eq!(
        fixture.call(number::RMDIR, [inside, 0, 0, 0, 0, 0]),
        Errno::NotDir.as_result()
    );

    assert_eq!(fixture.call(number::UNLINK, [inside, 0, 0, 0, 0, 0]), 0);
    assert_eq!(fixture.call(number::RMDIR, [path, 0, 0, 0, 0, 0]), 0);
    assert_eq!(
        fixture.stat(number::STAT, "/made").unwrap_err(),
        Errno::NoEntry.as_result()
    );

    // The parent's link count followed along.
    let root = fixture.stat(number::STAT, "/").expect("stat");
    let with_directory = field_u32(&root, 16);
    let path = fixture.arena.path("/second");
    fixture.call(number::MKDIR, [path, 0o755, 0, 0, 0, 0]);
    let after = fixture.stat(number::STAT, "/").expect("stat");
    assert_eq!(field_u32(&after, 16), with_directory + 1);
}

/// A directory made in the upper layer over a name the image also has shows
/// nothing from below: it was created here, and what was there is gone.
#[test]
fn a_recreated_directory_is_opaque() {
    let mut fixture = fixture("opaque");
    assert!(fixture.stat(number::STAT, "/etc/hosts").is_ok());

    // Empty it, remove it, and make it again.
    for name in ["/etc/hosts", "/etc/hostname", "/etc/hosts-alias", "/etc/pipe"] {
        let path = fixture.arena.path(name);
        assert_eq!(fixture.call(number::UNLINK, [path, 0, 0, 0, 0, 0]), 0, "{name}");
    }
    let conf = fixture.arena.path("/etc/conf.d");
    assert_eq!(fixture.call(number::RMDIR, [conf, 0, 0, 0, 0, 0]), 0);
    let etc = fixture.arena.path("/etc");
    assert_eq!(fixture.call(number::RMDIR, [etc, 0, 0, 0, 0, 0]), 0);
    assert_eq!(fixture.call(number::MKDIR, [etc, 0o755, 0, 0, 0, 0]), 0);

    // Nothing from the image shows through.
    assert_eq!(
        fixture.stat(number::STAT, "/etc/hosts").unwrap_err(),
        Errno::NoEntry.as_result()
    );
    let directory = fixture.open("/etc", open_flags::DIRECTORY);
    let listing = fixture.arena.buffer(1024);
    let written = fixture.call(number::GETDENTS64, [directory, listing, 1024, 0, 0, 0]);
    let entries = fixture.arena.read(listing, written as usize).to_vec();
    assert_eq!(entry_type_of(&entries, b"hostname"), None);
    // Only `.` and `..`.
    assert_eq!(written, 48, "an empty directory lists two entries");
}

#[test]
fn a_symlink_can_be_created_and_followed() {
    let mut fixture = fixture("symlink");
    let target = fixture.arena.place(b"/etc/hosts\0");
    let path = fixture.arena.path("/link");
    assert_eq!(fixture.call(number::SYMLINK, [target, path, 0, 0, 0, 0]), 0);

    let stat = fixture.stat(number::LSTAT, "/link").expect("lstat");
    assert_eq!(field_u32(&stat, 24) & 0o170000, 0o120000);
    assert_eq!(field_u64(&stat, 48), 10, "a symlink's size is its target's");
    let followed = fixture.stat(number::STAT, "/link").expect("stat");
    assert_eq!(field_u64(&followed, 48), 20);

    let buffer = fixture.arena.buffer(32);
    assert_eq!(fixture.call(number::READLINK, [path, buffer, 32, 0, 0, 0]), 10);
    assert_eq!(fixture.arena.read(buffer, 10), b"/etc/hosts");

    assert_eq!(
        fixture.call(number::SYMLINK, [target, path, 0, 0, 0, 0]),
        Errno::Exists.as_result()
    );
}

#[test]
fn renaming_moves_a_name_and_refuses_what_it_cannot_do() {
    let mut fixture = fixture("rename");
    let fd = fixture.open("/source", open_flags::WRITE_ONLY | open_flags::CREATE);
    let message = fixture.arena.place(b"contents");
    fixture.call(number::WRITE, [fd, message, 8, 0, 0, 0]);

    let from = fixture.arena.path("/source");
    let to = fixture.arena.path("/destination");
    assert_eq!(fixture.call(number::RENAME, [from, to, 0, 0, 0, 0]), 0);
    assert_eq!(
        fixture.stat(number::STAT, "/source").unwrap_err(),
        Errno::NoEntry.as_result()
    );
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/destination").expect("stat"), 48),
        8
    );

    // Replacing an existing file is allowed and takes its name.
    let other = fixture.open("/other", open_flags::WRITE_ONLY | open_flags::CREATE);
    let three = fixture.arena.place(b"abc");
    fixture.call(number::WRITE, [other, three, 3, 0, 0, 0]);
    let other_path = fixture.arena.path("/other");
    assert_eq!(fixture.call(number::RENAME, [to, other_path, 0, 0, 0, 0]), 0);
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/other").expect("stat"), 48),
        8,
        "the destination is the file that was moved"
    );

    // The four cases that decide whether this is overlayfs or something
    // that merely looks like it. Measured against the real thing rather
    // than reasoned about: a Debian container's root filesystem *is* kernel
    // overlayfs with the image as its lower layer, so `perl -e rename` in
    // one answers the question directly. On this machine, today:
    //
    //     rename(/usr/share)   Invalid cross-device link
    //     rename(/fresh)       ok          (a directory made in the upper)
    //     rename(/usr/bin/env) ok          (a file from the lower)
    //     rename(/fresh2)      ok          (a non-empty upper directory)
    //
    // The first is the interesting one. What would move is the *name*, and
    // every path below it would still resolve through the image at its old
    // place — so kernel overlayfs refuses it too, with `redirect_dir` off.
    // Userspace already handles it: `mv` falls back to copying. Inventing
    // better-than-overlayfs semantics here would mean behaviour no real
    // container has ever been tested against.
    let etc = fixture.arena.path("/etc");
    let elsewhere = fixture.arena.path("/moved");
    assert_eq!(
        fixture.call(number::RENAME, [etc, elsewhere, 0, 0, 0, 0]),
        Errno::CrossDevice.as_result(),
        "a directory from the image"
    );

    // A file from the image moves: it is copied up and the old name is
    // whited out, and nothing below it can be reached by a path any more.
    let keep = fixture.arena.path("/etc/hosts");
    let moved_file = fixture.arena.path("/hosts-moved");
    assert_eq!(
        fixture.call(number::RENAME, [keep, moved_file, 0, 0, 0, 0]),
        0,
        "a file from the image"
    );
    assert_eq!(
        fixture.stat(number::STAT, "/etc/hosts").unwrap_err(),
        Errno::NoEntry.as_result()
    );
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/hosts-moved").expect("stat"), 48),
        20,
        "with its contents"
    );

    // A directory created here moves freely, empty or not.
    let made = fixture.arena.path("/made");
    assert_eq!(fixture.call(number::MKDIR, [made, 0o755, 0, 0, 0, 0]), 0);
    let inside = fixture.open("/made/inside", open_flags::WRITE_ONLY | open_flags::CREATE);
    assert!(inside >= 0);
    assert_eq!(
        fixture.call(number::RENAME, [made, elsewhere, 0, 0, 0, 0]),
        0,
        "a non-empty directory made in the upper layer"
    );
    assert!(fixture.stat(number::STAT, "/moved").is_ok());
    assert!(
        fixture.stat(number::STAT, "/moved/inside").is_ok(),
        "and what was in it came along"
    );

    // The kinds have to match.
    assert_eq!(
        fixture.call(number::RENAME, [other_path, elsewhere, 0, 0, 0, 0]),
        Errno::IsDir.as_result()
    );
    assert_eq!(
        fixture.call(number::RENAME, [elsewhere, other_path, 0, 0, 0, 0]),
        Errno::NotDir.as_result()
    );
}

/// Timestamps, which decide whether a `.pyc` is stale.
#[test]
fn utimensat_sets_the_time_a_pyc_is_compared_against() {
    let mut fixture = fixture("utimens");
    let fd = fixture.open("/source.py", open_flags::WRITE_ONLY | open_flags::CREATE);
    assert!(fd >= 0);

    // An explicit time, exactly as given: rounding it is what makes every
    // import either recompile or use a stale cache.
    let times = fixture.arena.place(&{
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1_700_000_000i64.to_le_bytes()); // atime sec
        bytes.extend_from_slice(&0i64.to_le_bytes()); // atime nsec
        bytes.extend_from_slice(&1_600_000_000i64.to_le_bytes()); // mtime sec
        bytes.extend_from_slice(&123_456_789i64.to_le_bytes()); // mtime nsec
        bytes
    });
    let path = fixture.arena.path("/source.py");
    assert_eq!(
        fixture.call(number::UTIMENSAT, [at::FDCWD, path, times, 0, 0, 0]),
        0
    );
    let stat = fixture.stat(number::STAT, "/source.py").expect("stat");
    assert_eq!(field_u64(&stat, 88), 1_600_000_000, "st_mtime");
    assert_eq!(field_u64(&stat, 96), 123_456_789, "st_mtime_nsec");

    // `UTIME_OMIT` leaves it alone.
    let omit = fixture.arena.place(&{
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&((1i64 << 30) - 2).to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&((1i64 << 30) - 2).to_le_bytes());
        bytes
    });
    assert_eq!(
        fixture.call(number::UTIMENSAT, [at::FDCWD, path, omit, 0, 0, 0]),
        0
    );
    let stat = fixture.stat(number::STAT, "/source.py").expect("stat");
    assert_eq!(field_u64(&stat, 88), 1_600_000_000, "unchanged");

    // A nanosecond field that is neither a real value nor one of the two
    // special ones is `EINVAL`.
    let bad = fixture.arena.place(&{
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&2_000_000_000i64.to_le_bytes());
        bytes
    });
    assert_eq!(
        fixture.call(number::UTIMENSAT, [at::FDCWD, path, bad, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    // An unknown flag is `EINVAL` too.
    assert_eq!(
        fixture.call(number::UTIMENSAT, [at::FDCWD, path, times, 0x4000, 0, 0]),
        Errno::Invalid.as_result()
    );

    // Writing to a file moves its timestamp forward, which is what makes a
    // cache stale in the first place.
    let before = field_u64(&fixture.stat(number::STAT, "/source.py").expect("stat"), 88);
    let message = fixture.arena.place(b"x");
    fixture.call(number::WRITE, [fd, message, 1, 0, 0, 0]);
    let after = field_u64(&fixture.stat(number::STAT, "/source.py").expect("stat"), 88);
    assert_ne!(before, after, "a write updates the modification time");
}

/// `flock` is per open file description, which is what makes `dup` share it
/// and a close release it.
#[test]
fn flock_is_held_by_the_description() {
    let mut fixture = fixture("flock");
    let fd = fixture.opened("/etc/hosts");
    assert_eq!(fixture.call(number::FLOCK, [fd, 2, 0, 0, 0, 0]), 0, "LOCK_EX");
    assert_eq!(fixture.kernel.files.lock(fd as i32).expect("lock"), 2);

    // A `dup` shares the description, so it shares the lock.
    let copy = fixture.call(number::DUP, [fd, 0, 0, 0, 0, 0]);
    assert!(copy >= 0);
    assert_eq!(fixture.kernel.files.lock(copy as i32).expect("lock"), 2);

    // A second request replaces the first rather than stacking.
    assert_eq!(fixture.call(number::FLOCK, [fd, 1, 0, 0, 0, 0]), 0, "LOCK_SH");
    assert_eq!(fixture.kernel.files.lock(copy as i32).expect("lock"), 1);

    // Unlocking releases it.
    assert_eq!(fixture.call(number::FLOCK, [fd, 8, 0, 0, 0, 0]), 0, "LOCK_UN");
    assert_eq!(fixture.kernel.files.lock(fd as i32).expect("lock"), 0);

    // A separate open is a separate description with its own lock.
    let other = fixture.opened("/etc/hosts");
    assert_eq!(fixture.kernel.files.lock(other as i32).expect("lock"), 0);

    assert_eq!(
        fixture.call(number::FLOCK, [fd, 99, 0, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    assert_eq!(
        fixture.call(number::FLOCK, [99, 2, 0, 0, 0, 0]),
        Errno::BadFile.as_result()
    );
}

// ---- what the M4 review found ---------------------------------------------

/// Every path-based change acts on what a trailing symlink *points at*, not
/// on the link.
///
/// The parent walk stops one component short by design, so it never followed
/// the last one — and the callers had already resolved the target and thrown
/// that answer away. Three of the five rows below succeeded while leaving the
/// file the caller meant untouched, which is the exact shape this project
/// says nothing may do.
#[test]
fn a_change_through_a_symlink_reaches_the_file_it_names() {
    let mut fixture = fixture("symlink-write");
    // `/hosts-link` points at `etc/hosts`, which holds twenty bytes.
    let link = fixture.arena.path("/hosts-link");

    // Writing through it changes the target.
    let fd = fixture.open("/hosts-link", open_flags::WRITE_ONLY);
    assert!(fd >= 0, "opening through a symlink failed with {fd}");
    let message = fixture.arena.place(b"CHANGED");
    assert_eq!(fixture.call(number::WRITE, [fd, message, 7, 0, 0, 0]), 7);
    let target = fixture.stat(number::STAT, "/etc/hosts").expect("stat");
    assert_eq!(field_u64(&target, 48), 20, "the target still has its bytes");
    let reader = fixture.opened("/etc/hosts");
    let buffer = fixture.arena.buffer(32);
    assert_eq!(fixture.call(number::READ, [reader, buffer, 32, 0, 0, 0]), 20);
    assert_eq!(fixture.arena.read(buffer, 7), b"CHANGED");
    // And the link is still a link.
    let itself = fixture.stat(number::LSTAT, "/hosts-link").expect("lstat");
    assert_eq!(field_u32(&itself, 24) & 0o170000, 0o120000);

    // Truncating through it truncates the target.
    assert_eq!(fixture.call(number::TRUNCATE, [link, 4, 0, 0, 0, 0]), 0);
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/etc/hosts").expect("stat"), 48),
        4
    );

    // `O_TRUNC` through it empties the target rather than doing nothing.
    let emptied = fixture.open("/hosts-link", open_flags::WRITE_ONLY | open_flags::TRUNCATE);
    assert!(emptied >= 0);
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/etc/hosts").expect("stat"), 48),
        0
    );

    // `chmod` changes the target's mode; a symlink's own mode is not a
    // thing Linux lets anyone set.
    assert_eq!(fixture.call(number::CHMOD, [link, 0o700, 0, 0, 0, 0]), 0);
    assert_eq!(
        field_u32(&fixture.stat(number::STAT, "/etc/hosts").expect("stat"), 24) & 0o777,
        0o700
    );
    assert_eq!(
        field_u32(&fixture.stat(number::LSTAT, "/hosts-link").expect("lstat"), 24) & 0o777,
        0o777,
        "the link's own mode is untouched"
    );

    // `utimensat` sets the target's time — the row M4 calls load-bearing
    // for `.pyc` staleness, and source trees are full of symlinks.
    let times = fixture.arena.place(&{
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&1_500_000_000i64.to_le_bytes());
        bytes.extend_from_slice(&7i64.to_le_bytes());
        bytes
    });
    assert_eq!(
        fixture.call(number::UTIMENSAT, [at::FDCWD, link, times, 0, 0, 0]),
        0
    );
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/etc/hosts").expect("stat"), 88),
        1_500_000_000
    );

    // With `AT_SYMLINK_NOFOLLOW` it is the link that changes.
    let before = field_u64(&fixture.stat(number::STAT, "/etc/hosts").expect("stat"), 88);
    assert_eq!(
        fixture.call(
            number::UTIMENSAT,
            [at::FDCWD, link, times, at::SYMLINK_NOFOLLOW as i64, 0, 0]
        ),
        0
    );
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/etc/hosts").expect("stat"), 88),
        before,
        "the target was not touched"
    );

    // Removing acts on the name, as it always did: the link goes and the
    // target stays.
    assert_eq!(fixture.call(number::UNLINK, [link, 0, 0, 0, 0, 0]), 0);
    assert!(fixture.stat(number::LSTAT, "/hosts-link").is_err());
    assert!(fixture.stat(number::STAT, "/etc/hosts").is_ok());
}

/// Moving a directory into its own subtree would splice it into itself and
/// detach everything below it from the root.
#[test]
fn a_directory_cannot_be_renamed_into_itself() {
    let mut fixture = fixture("rename-cycle");
    let outer = fixture.arena.path("/outer");
    assert_eq!(fixture.call(number::MKDIR, [outer, 0o755, 0, 0, 0, 0]), 0);
    let inner = fixture.arena.path("/outer/inner");
    assert_eq!(fixture.call(number::MKDIR, [inner, 0o755, 0, 0, 0, 0]), 0);
    let deeper = fixture.arena.path("/outer/inner/deeper");

    assert_eq!(
        fixture.call(number::RENAME, [outer, inner, 0, 0, 0, 0]),
        Errno::Invalid.as_result(),
        "into a child"
    );
    assert_eq!(
        fixture.call(number::RENAME, [outer, deeper, 0, 0, 0, 0]),
        Errno::Invalid.as_result(),
        "into a grandchild"
    );
    // Everything is still reachable.
    assert!(fixture.stat(number::STAT, "/outer").is_ok());
    assert!(fixture.stat(number::STAT, "/outer/inner").is_ok());
    // Onto itself is a no-op that succeeds, which is what Linux does.
    assert_eq!(fixture.call(number::RENAME, [outer, outer, 0, 0, 0, 0]), 0);
    assert!(fixture.stat(number::STAT, "/outer").is_ok());
}

/// A mount point belongs to the mount, not to the filesystem holding the
/// name. Removing it would leave the mount attached to a vnode nothing can
/// reach.
#[test]
fn a_mount_point_cannot_be_removed_or_renamed() {
    let mut fixture = fixture("mount-busy");
    let dev = fixture.arena.path("/dev");
    let elsewhere = fixture.arena.path("/moved");
    assert_eq!(
        fixture.call(number::RMDIR, [dev, 0, 0, 0, 0, 0]),
        Errno::Busy.as_result()
    );
    assert_eq!(
        fixture.call(number::RENAME, [dev, elsewhere, 0, 0, 0, 0]),
        Errno::Busy.as_result()
    );
    // And it is all still there.
    assert!(fixture.stat(number::STAT, "/dev/null").is_ok());
    assert_eq!(fixture.kernel.vfs.mounts().count(), 3);
}

/// A directory that still reads through to the image cannot be renamed —
/// including after something has been created inside it.
///
/// The earlier test asked whether the directory had been copied up, which
/// becomes true the moment anything is written inside it. Kernel overlayfs
/// refuses that case too: measured in a container, `mkdir /usr/share/zz`
/// followed by `rename("/usr/share", …)` is still `EXDEV`.
#[test]
fn a_merged_directory_cannot_be_renamed_even_once_written_to() {
    let mut fixture = fixture("rename-merged");
    let created = fixture.open("/etc/fresh", open_flags::WRITE_ONLY | open_flags::CREATE);
    assert!(created >= 0, "the copy-up did not happen");
    let etc = fixture.arena.path("/etc");
    let elsewhere = fixture.arena.path("/moved");
    assert_eq!(
        fixture.call(number::RENAME, [etc, elsewhere, 0, 0, 0, 0]),
        Errno::CrossDevice.as_result()
    );
    // A directory created here, which reads through to nothing, still moves.
    let fresh = fixture.arena.path("/fresh-dir");
    assert_eq!(fixture.call(number::MKDIR, [fresh, 0o755, 0, 0, 0, 0]), 0);
    assert_eq!(fixture.call(number::RENAME, [fresh, elsewhere, 0, 0, 0, 0]), 0);
}

/// A change that is going to fail must not copy anything up on the way.
///
/// Copying a directory up changes its identity — a new `st_ino` — and
/// `(st_dev, st_ino)` is how `find -xdev`, `du` and every cycle check decide
/// two paths are one file. A failed `unlink` renumbering a directory is a
/// wrong answer to a question nobody asked.
#[test]
fn a_refused_change_leaves_the_tree_alone() {
    let mut fixture = fixture("no-churn");
    let before_etc = field_u64(&fixture.stat(number::STAT, "/etc").expect("stat"), 8);
    let before_root = field_u64(&fixture.stat(number::STAT, "/").expect("stat"), 8);

    let missing = fixture.arena.path("/etc/not-there");
    assert_eq!(
        fixture.call(number::UNLINK, [missing, 0, 0, 0, 0, 0]),
        Errno::NoEntry.as_result()
    );
    let existing = fixture.arena.path("/etc/hosts");
    assert_eq!(
        fixture.call(number::MKDIR, [existing, 0o755, 0, 0, 0, 0]),
        Errno::Exists.as_result()
    );
    let target = fixture.arena.place(b"anywhere\0");
    assert_eq!(
        fixture.call(number::SYMLINK, [target, existing, 0, 0, 0, 0]),
        Errno::Exists.as_result()
    );

    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/etc").expect("stat"), 8),
        before_etc,
        "a refused change renumbered /etc"
    );
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/").expect("stat"), 8),
        before_root
    );

    // A change that succeeds does copy up, and the identity moves once —
    // which is what a copy-up is.
    let created = fixture.arena.path("/etc/made");
    assert_eq!(fixture.call(number::MKDIR, [created, 0o755, 0, 0, 0, 0]), 0);
    let after = field_u64(&fixture.stat(number::STAT, "/etc").expect("stat"), 8);
    assert_ne!(after, before_etc);
    // …and then stays put.
    let second = fixture.arena.path("/etc/made-again");
    assert_eq!(fixture.call(number::MKDIR, [second, 0o755, 0, 0, 0, 0]), 0);
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/etc").expect("stat"), 8),
        after
    );
}

/// A scan that outlives a change still reports every entry that survived it,
/// exactly once. `rm -r` is readdir and unlink interleaved.
#[test]
fn a_directory_scan_survives_a_change_underneath_it() {
    let mut fixture = fixture("scan");
    let directory = fixture.arena.path("/scanned");
    assert_eq!(fixture.call(number::MKDIR, [directory, 0o755, 0, 0, 0, 0]), 0);
    let names = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
    for name in names {
        let fd = fixture.open(&format!("/scanned/{name}"), open_flags::WRITE_ONLY | open_flags::CREATE);
        assert!(fd >= 0);
        fixture.call(number::CLOSE, [fd, 0, 0, 0, 0, 0]);
    }

    let scan = fixture.open("/scanned", open_flags::DIRECTORY);
    assert!(scan >= 0);
    // A buffer that holds only the first few entries, so the scan takes
    // more than one call — which is the only way the bug shows.
    let buffer = fixture.arena.buffer(1024);
    let mut seen: Vec<String> = Vec::new();
    let mut removed = false;
    // Bounded. A scan that fails to make progress is a bug, and a test that
    // expresses it as a hang is a test that stops the run instead of
    // reporting — which is what happened the first time this was written.
    for batch in 0..32 {
        assert!(batch < 31, "the scan never ended: saw {seen:?}");
        let written = fixture.call(number::GETDENTS64, [scan, buffer, 96, 0, 0, 0]);
        assert!(written >= 0, "getdents64 failed with {written}");
        if written == 0 {
            break;
        }
        let bytes = fixture.arena.read(buffer, written as usize).to_vec();
        let mut at = 0;
        while at < bytes.len() {
            let length =
                u16::from_le_bytes(bytes[at + 16..at + 18].try_into().expect("two")) as usize;
            let name = &bytes[at + 19..at + length];
            let end = name.iter().position(|byte| *byte == 0).unwrap_or(name.len());
            seen.push(String::from_utf8_lossy(&name[..end]).into_owned());
            at += length;
        }
        // Delete an entry the scan has not reached yet, once, in the
        // middle — which is what shifts every later position down one.
        if !removed && seen.iter().any(|name| name == "bravo") {
            let doomed = fixture.arena.path("/scanned/charlie");
            assert_eq!(fixture.call(number::UNLINK, [doomed, 0, 0, 0, 0, 0]), 0);
            removed = true;
        }
    }

    assert!(removed, "the test did not delete anything");
    let mut survivors: Vec<&String> = seen
        .iter()
        .filter(|name| *name != "." && *name != ".." && *name != "charlie")
        .collect();
    survivors.sort();
    survivors.dedup();
    let expected: Vec<&str> = names.iter().copied().filter(|name| *name != "charlie").collect();
    assert_eq!(
        survivors.iter().map(|name| name.as_str()).collect::<Vec<_>>(),
        expected,
        "the scan skipped an entry the deletion moved: saw {seen:?}"
    );
    // Nothing came back twice.
    let mut all = seen.clone();
    all.sort();
    let count = all.len();
    all.dedup();
    assert_eq!(all.len(), count, "an entry was reported twice: {seen:?}");
}

/// An `O_PATH` descriptor is a reference to a file, not a handle on it.
#[test]
fn an_o_path_descriptor_cannot_change_anything() {
    let mut fixture = fixture("o-path-write");
    let fd = fixture.open("/etc/hosts", open_flags::PATH | open_flags::WRITE_ONLY);
    assert!(fd >= 0);
    let buffer = fixture.arena.buffer(16);
    for (row, arguments) in [
        (number::WRITE, [fd, buffer, 4, 0, 0, 0]),
        (number::FTRUNCATE, [fd, 0, 0, 0, 0, 0]),
        (number::FCHMOD, [fd, 0o600, 0, 0, 0, 0]),
        (number::FLOCK, [fd, 2, 0, 0, 0, 0]),
        (number::FSYNC, [fd, 0, 0, 0, 0, 0]),
    ] {
        assert_eq!(
            fixture.call(row, arguments),
            Errno::BadFile.as_result(),
            "row {row}"
        );
    }
    // The file is untouched.
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/etc/hosts").expect("stat"), 48),
        20
    );
}

/// `O_CREAT` makes a regular file, and two ways of asking say the caller
/// wanted a directory. Linux refuses both rather than making the file.
#[test]
fn o_creat_refuses_a_name_that_asks_for_a_directory() {
    let mut fixture = fixture("creat-directory");
    assert_eq!(
        fixture.open("/wanted/", open_flags::WRITE_ONLY | open_flags::CREATE),
        Errno::IsDir.as_result()
    );
    assert!(
        fixture.stat(number::LSTAT, "/wanted").is_err(),
        "a file was created anyway"
    );
    assert_eq!(
        fixture.open(
            "/other",
            open_flags::WRITE_ONLY | open_flags::CREATE | open_flags::DIRECTORY
        ),
        Errno::Invalid.as_result()
    );
    assert!(fixture.stat(number::LSTAT, "/other").is_err());
}

/// `access(W_OK)` and `open(O_WRONLY)` must agree about a device node: a
/// daemonising process probes with one and then uses the other.
#[test]
fn a_device_node_is_writable_by_both_rows() {
    let mut fixture = fixture("device-access");
    let null = fixture.arena.path("/dev/null");
    assert_eq!(
        fixture.call(number::ACCESS, [null, access_mode::WRITE as i64, 0, 0, 0, 0]),
        0,
        "access says the device is writable"
    );
    assert!(
        fixture.open("/dev/null", open_flags::WRITE_ONLY) >= 0,
        "and open agrees"
    );
    // A regular file on a mount with no writable layer is `EROFS` from
    // both, which is the contrast that makes the pair meaningful.
    let in_proc = fixture.arena.path("/proc/self/maps");
    assert_eq!(
        fixture.call(number::ACCESS, [in_proc, access_mode::WRITE as i64, 0, 0, 0, 0]),
        Errno::ReadOnlyFs.as_result()
    );
    assert_eq!(
        fixture.open("/proc/self/maps", open_flags::WRITE_ONLY),
        Errno::ReadOnlyFs.as_result()
    );
}

/// `unlink(".")` is `EISDIR` and `rmdir(".")` is `EINVAL`: the two rows
/// answer differently, and a caller tells the failures apart by which.
#[test]
fn removing_dot_answers_by_which_row_asked() {
    let mut fixture = fixture("dot");
    let dot = fixture.arena.path("/etc/.");
    assert_eq!(
        fixture.call(number::UNLINK, [dot, 0, 0, 0, 0, 0]),
        Errno::IsDir.as_result()
    );
    assert_eq!(
        fixture.call(number::RMDIR, [dot, 0, 0, 0, 0, 0]),
        Errno::Invalid.as_result()
    );
    let dotdot = fixture.arena.path("/etc/..");
    assert_eq!(
        fixture.call(number::RMDIR, [dotdot, 0, 0, 0, 0, 0]),
        Errno::NotEmpty.as_result()
    );
}

/// Setting the executable path replaces the `/proc` mount rather than
/// stacking another one on it.
#[test]
fn setting_the_executable_replaces_the_proc_mount() {
    let mut fixture = fixture("exec-path");
    let before = fixture.kernel.vfs.mounts().count();
    let path = fixture.arena.path("/proc/self/exe");
    let buffer = fixture.arena.buffer(64);

    for name in ["/first", "/second", "/usr/lib/libthing.so", "/fourth", "/fifth"] {
        fixture.kernel.set_executable(name);
        assert_eq!(
            fixture.kernel.vfs.mounts().count(),
            before,
            "setting it again stacked a mount"
        );
        let read = fixture.call(number::READLINK, [path, buffer, 64, 0, 0, 0]);
        assert_eq!(read, name.len() as i64, "reading it back after {name}");
        assert_eq!(fixture.arena.read(buffer, name.len()), name.as_bytes());
    }
}

/// The writable layer stores a file's contents as bytes in the kernel's own
/// heap, so a hole has to be materialised — and a guest can ask for a hole
/// larger than the machine.
///
/// Two answers, and neither is a dead container. Past what this filesystem
/// can hold, `EFBIG`, which is what Linux says when a write would exceed a
/// filesystem's maximum file size. Within that but past what the allocator
/// can give, `ENOSPC` — because `Vec`'s ordinary growth *aborts* on failure,
/// and an abort inside a wasm module takes the whole container with it.
#[test]
fn a_write_past_what_can_be_stored_is_refused_rather_than_fatal() {
    let mut fixture = fixture("too-big");
    let fd = fixture.open("/big", open_flags::READ_WRITE | open_flags::CREATE);
    assert!(fd >= 0);
    let byte = fixture.arena.place(b"x");

    // A terabyte in. Refused by arithmetic: nothing is allocated, which is
    // the point — the machine this runs on does not have a terabyte and
    // must not be asked for one.
    assert_eq!(
        fixture.call(number::PWRITE64, [fd, byte, 1, 1 << 40, 0, 0]),
        Errno::TooBig.as_result()
    );
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/big").expect("stat"), 48),
        0,
        "and the file did not grow"
    );
    assert_eq!(
        fixture.call(number::FTRUNCATE, [fd, 1 << 40, 0, 0, 0, 0]),
        Errno::TooBig.as_result()
    );
    // An offset that would overflow the addition is the same answer, not a
    // wrapped one.
    assert_eq!(
        fixture.call(number::PWRITE64, [fd, byte, 1, i64::MAX, 0, 0]),
        Errno::TooBig.as_result()
    );

    // A hole this filesystem *can* hold is written, and reads as zeros —
    // which is what a sparse file does everywhere.
    assert_eq!(fixture.call(number::PWRITE64, [fd, byte, 1, 4096, 0, 0]), 1);
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/big").expect("stat"), 48),
        4097
    );
    let buffer = fixture.arena.buffer(64);
    assert_eq!(fixture.call(number::PREAD64, [fd, buffer, 64, 0, 0, 0]), 64);
    assert_eq!(fixture.arena.read(buffer, 64), &[0u8; 64], "the hole is zeros");
}

/// Copying a file up breaks a hard link, and the link count says so.
///
/// An image can hold one file under several names — every busybox applet is
/// that — and a copy is reached by exactly one of them. Kernel overlayfs
/// breaks the link the same way with its index feature off, which is the
/// default. Carrying the lower count across would have the copy claim names
/// that no longer reach it.
#[test]
fn copying_up_breaks_a_hard_link_and_says_so() {
    let mut fixture = fixture("hardlink-copyup");
    // `/twin-a` and `/twin-b` are one file in the image.
    let before = fixture.stat(number::STAT, "/twin-a").expect("stat");
    assert_eq!(field_u32(&before, 16), 2, "two names, one file");
    let other = fixture.stat(number::STAT, "/twin-b").expect("stat");
    assert_eq!(field_u64(&before, 8), field_u64(&other, 8), "the same inode");

    let fd = fixture.open("/twin-a", open_flags::WRITE_ONLY);
    assert!(fd >= 0);
    let message = fixture.arena.place(b"ONE");
    assert_eq!(fixture.call(number::WRITE, [fd, message, 3, 0, 0, 0]), 3);

    let copied = fixture.stat(number::STAT, "/twin-a").expect("stat");
    assert_eq!(field_u32(&copied, 16), 1, "the copy has one name");
    let untouched = fixture.stat(number::STAT, "/twin-b").expect("stat");
    assert_eq!(field_u32(&untouched, 16), 2, "the original still has two");
    assert_ne!(
        field_u64(&copied, 8),
        field_u64(&untouched, 8),
        "and they are different files now"
    );
    // The other name still reads the original bytes.
    let reader = fixture.opened("/twin-b");
    let buffer = fixture.arena.buffer(16);
    let read = fixture.call(number::READ, [reader, buffer, 16, 0, 0, 0]);
    assert_eq!(fixture.arena.read(buffer, 5), b"twins");
    assert_eq!(read, 6);
}

/// `utimensat`'s edges: a flag with nothing to apply to, and a request that
/// changes nothing but still has to find a filesystem that accepts changes.
#[test]
fn utimensat_refuses_a_flag_that_cannot_apply() {
    let mut fixture = fixture("utimens-edges");
    let fd = fixture.opened("/etc/hosts");
    let times = fixture.arena.place(&[0u8; 32]);
    // A null path names the descriptor, so there is no final component to
    // decline to follow.
    assert_eq!(
        fixture.call(
            number::UTIMENSAT,
            [fd, 0, times, at::SYMLINK_NOFOLLOW as i64, 0, 0]
        ),
        Errno::Invalid.as_result()
    );

    // `UTIME_OMIT` changes nothing — and still says `EROFS` on a mount that
    // accepts no changes, rather than reporting a success that could not
    // have happened.
    let omit = fixture.arena.place(&{
        let mut bytes = Vec::new();
        for _ in 0..2 {
            bytes.extend_from_slice(&0i64.to_le_bytes());
            bytes.extend_from_slice(&((1i64 << 30) - 2).to_le_bytes());
        }
        bytes
    });
    let in_proc = fixture.arena.path("/proc/self/maps");
    assert_eq!(
        fixture.call(number::UTIMENSAT, [at::FDCWD, in_proc, omit, 0, 0, 0]),
        Errno::ReadOnlyFs.as_result()
    );
    // On a mount that does accept them, it succeeds and changes nothing.
    let path = fixture.arena.path("/etc/hosts");
    let before = field_u64(&fixture.stat(number::STAT, "/etc/hosts").expect("stat"), 88);
    assert_eq!(
        fixture.call(number::UTIMENSAT, [at::FDCWD, path, omit, 0, 0, 0]),
        0
    );
    assert_eq!(
        field_u64(&fixture.stat(number::STAT, "/etc/hosts").expect("stat"), 88),
        before
    );
}

// ---- hard links and reclamation -------------------------------------------

/// A second name for a file: one node, two names, and `nlink` says so.
#[test]
fn link_gives_a_file_a_second_name() {
    let mut fixture = fixture("link");
    let fd = fixture.open("/original", open_flags::READ_WRITE | open_flags::CREATE);
    assert!(fd >= 0);
    let message = fixture.arena.place(b"shared bytes");
    assert_eq!(fixture.call(number::WRITE, [fd, message, 12, 0, 0, 0]), 12);

    let from = fixture.arena.path("/original");
    let to = fixture.arena.path("/second-name");
    assert_eq!(fixture.call(number::LINK, [from, to, 0, 0, 0, 0]), 0);

    // One file, two names: the same inode, and a link count of two.
    let first = fixture.stat(number::STAT, "/original").expect("stat");
    let second = fixture.stat(number::STAT, "/second-name").expect("stat");
    assert_eq!(field_u64(&first, 8), field_u64(&second, 8), "one inode");
    assert_eq!(field_u32(&first, 16), 2);
    assert_eq!(field_u32(&second, 16), 2);

    // Writing through one is visible through the other, which is the whole
    // point of a hard link.
    let writer = fixture.open("/second-name", open_flags::WRITE_ONLY);
    let changed = fixture.arena.place(b"CHANGED");
    assert_eq!(fixture.call(number::WRITE, [writer, changed, 7, 0, 0, 0]), 7);
    let reader = fixture.opened("/original");
    let buffer = fixture.arena.buffer(32);
    assert_eq!(fixture.call(number::READ, [reader, buffer, 32, 0, 0, 0]), 12);
    assert_eq!(fixture.arena.read(buffer, 7), b"CHANGED");

    // Removing one name leaves the other, with the count back to one.
    assert_eq!(fixture.call(number::UNLINK, [from, 0, 0, 0, 0, 0]), 0);
    assert!(fixture.stat(number::STAT, "/original").is_err());
    let survivor = fixture.stat(number::STAT, "/second-name").expect("stat");
    assert_eq!(field_u32(&survivor, 16), 1);
    let reader = fixture.opened("/second-name");
    assert_eq!(fixture.call(number::READ, [reader, buffer, 32, 0, 0, 0]), 12);
}

#[test]
fn link_refuses_what_it_cannot_do() {
    let mut fixture = fixture("link-refusals");
    let file = fixture.arena.path("/etc/hosts");
    let taken = fixture.arena.path("/etc/hostname");
    let fresh = fixture.arena.path("/fresh");
    let missing = fixture.arena.path("/nowhere");
    let directory = fixture.arena.path("/etc");
    let elsewhere = fixture.arena.path("/etc-link");

    // A directory: POSIX forbids it, and a filesystem that allowed one
    // would have a cycle nothing could walk.
    assert_eq!(
        fixture.call(number::LINK, [directory, elsewhere, 0, 0, 0, 0]),
        Errno::Perm.as_result()
    );
    // A name that is taken.
    assert_eq!(
        fixture.call(number::LINK, [file, taken, 0, 0, 0, 0]),
        Errno::Exists.as_result()
    );
    // A source that is not there.
    assert_eq!(
        fixture.call(number::LINK, [missing, fresh, 0, 0, 0, 0]),
        Errno::NoEntry.as_result()
    );
    // A flag this kernel does not implement is a named fault rather than a
    // link that quietly went somewhere else.
    let outcome = fixture.kernel.dispatch(
        number::LINKAT,
        Arguments::new([at::FDCWD, file, at::FDCWD, fresh, at::EMPTY_PATH as i64, 0]),
    );
    let Outcome::Fault(fault) = outcome else {
        panic!("AT_EMPTY_PATH produced {outcome:?}");
    };
    let mut message = String::new();
    fault.message(&mut message);
    assert!(message.contains("AT_EMPTY_PATH"), "{message}");
    // An unknown flag is `EINVAL`.
    assert_eq!(
        fixture.call(number::LINKAT, [at::FDCWD, file, at::FDCWD, fresh, 0x4000, 0]),
        Errno::Invalid.as_result()
    );
}

/// Linking a file that is still in the image copies it up first, so both new
/// names name the copy and the image's other names still resolve below.
#[test]
fn linking_an_image_file_copies_it_up_first() {
    let mut fixture = fixture("link-copyup");
    let from = fixture.arena.path("/twin-a");
    let to = fixture.arena.path("/twin-c");
    assert_eq!(fixture.call(number::LINK, [from, to, 0, 0, 0, 0]), 0);

    // `/twin-a` and `/twin-c` are one file with two names…
    let a = fixture.stat(number::STAT, "/twin-a").expect("stat");
    let c = fixture.stat(number::STAT, "/twin-c").expect("stat");
    assert_eq!(field_u64(&a, 8), field_u64(&c, 8));
    assert_eq!(field_u32(&a, 16), 2);
    // …and `/twin-b`, the image's other name for the original, is not one
    // of them.
    let b = fixture.stat(number::STAT, "/twin-b").expect("stat");
    assert_ne!(field_u64(&a, 8), field_u64(&b, 8));

    // `link` does not follow a trailing symlink: the link itself gains a
    // name, which is the opposite of every other row that changes something.
    let link = fixture.arena.path("/hosts-link");
    let named = fixture.arena.path("/link-twin");
    assert_eq!(fixture.call(number::LINK, [link, named, 0, 0, 0, 0]), 0);
    let stat = fixture.stat(number::LSTAT, "/link-twin").expect("lstat");
    assert_eq!(field_u32(&stat, 24) & 0o170000, 0o120000, "still a symlink");
    // With `AT_SYMLINK_FOLLOW` it is the target that gains one.
    let followed = fixture.arena.path("/followed");
    assert_eq!(
        fixture.call(
            number::LINKAT,
            [at::FDCWD, link, at::FDCWD, followed, at::SYMLINK_FOLLOW as i64, 0]
        ),
        0
    );
    let stat = fixture.stat(number::LSTAT, "/followed").expect("lstat");
    assert_eq!(field_u32(&stat, 24) & 0o170000, 0o100000, "a regular file");
}

/// An unlinked file's bytes go back when nothing can reach them — and not
/// before, because POSIX keeps it alive until the last descriptor closes.
///
/// Without this the writable layer never gives anything back: a container
/// that writes and deletes temporary files holds every byte for as long as
/// it runs.
#[test]
fn an_unlinked_file_gives_its_bytes_back_when_nothing_holds_it() {
    let mut fixture = fixture("reclaim");
    let path = fixture.arena.path("/temporary");
    let fd = fixture.open("/temporary", open_flags::READ_WRITE | open_flags::CREATE);
    assert!(fd >= 0);
    let message = fixture.arena.place(b"held open");
    assert_eq!(fixture.call(number::WRITE, [fd, message, 9, 0, 0, 0]), 9);

    // Unlinked while open: the name is gone and the file is not.
    assert_eq!(fixture.call(number::UNLINK, [path, 0, 0, 0, 0, 0]), 0);
    assert!(fixture.stat(number::STAT, "/temporary").is_err());
    let buffer = fixture.arena.buffer(32);
    assert_eq!(
        fixture.call(number::PREAD64, [fd, buffer, 32, 0, 0, 0]),
        9,
        "an unlinked file is readable through its descriptor"
    );
    assert_eq!(fixture.arena.read(buffer, 9), b"held open");
    assert_eq!(
        fixture.call(number::FSTAT, [fd, buffer, 0, 0, 0, 0]),
        0,
        "and it still has a size"
    );
    assert_eq!(field_u64(fixture.arena.read(buffer, STAT_SIZE), 48), 9);

    // Closing the last descriptor is what frees it.
    assert_eq!(fixture.call(number::CLOSE, [fd, 0, 0, 0, 0, 0]), 0);
    assert!(
        fixture.kernel.released_bytes() > 0,
        "the bytes were not given back"
    );

    // A file with no descriptor open is freed at the unlink itself.
    let path = fixture.arena.path("/immediate");
    let fd = fixture.open("/immediate", open_flags::WRITE_ONLY | open_flags::CREATE);
    fixture.call(number::WRITE, [fd, message, 9, 0, 0, 0]);
    assert_eq!(fixture.call(number::CLOSE, [fd, 0, 0, 0, 0, 0]), 0);
    let before = fixture.kernel.held_bytes();
    assert_eq!(fixture.call(number::UNLINK, [path, 0, 0, 0, 0, 0]), 0);
    assert!(
        fixture.kernel.held_bytes() < before,
        "the writable layer did not shrink: {} then {}",
        before,
        fixture.kernel.held_bytes()
    );

    // A file with a second name keeps its bytes when one name goes.
    let first = fixture.arena.path("/kept-a");
    let second = fixture.arena.path("/kept-b");
    let fd = fixture.open("/kept-a", open_flags::WRITE_ONLY | open_flags::CREATE);
    fixture.call(number::WRITE, [fd, message, 9, 0, 0, 0]);
    fixture.call(number::CLOSE, [fd, 0, 0, 0, 0, 0]);
    assert_eq!(fixture.call(number::LINK, [first, second, 0, 0, 0, 0]), 0);
    let before = fixture.kernel.held_bytes();
    assert_eq!(fixture.call(number::UNLINK, [first, 0, 0, 0, 0, 0]), 0);
    assert_eq!(
        fixture.kernel.held_bytes(),
        before,
        "a file with another name was freed"
    );
    let reader = fixture.opened("/kept-b");
    assert_eq!(fixture.call(number::READ, [reader, buffer, 32, 0, 0, 0]), 9);
}

/// The churn a real container does: write a temporary file, delete it, over
/// and over. The layer must not grow with the number of rounds.
#[test]
fn repeated_churn_does_not_grow_the_writable_layer() {
    let mut fixture = fixture("churn");
    let path = fixture.arena.path("/scratch");
    let block = fixture.arena.place(&[0x5a; 4096]);
    let mut after_first = 0;
    for round in 0..32 {
        let fd = fixture.open("/scratch", open_flags::WRITE_ONLY | open_flags::CREATE);
        assert!(fd >= 0);
        for _ in 0..4 {
            assert_eq!(fixture.call(number::WRITE, [fd, block, 4096, 0, 0, 0]), 4096);
        }
        assert_eq!(fixture.call(number::CLOSE, [fd, 0, 0, 0, 0, 0]), 0);
        assert_eq!(fixture.call(number::UNLINK, [path, 0, 0, 0, 0, 0]), 0);
        if round == 0 {
            after_first = fixture.kernel.held_bytes();
        }
    }
    assert_eq!(
        fixture.kernel.held_bytes(),
        after_first,
        "thirty-two rounds of write-and-delete grew the layer"
    );
}
