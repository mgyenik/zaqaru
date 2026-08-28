//! Baking a tree and reading it back, natively, in milliseconds.
//!
//! This is the cheapest tier: no emulation, no linking, no wasm. Everything
//! the index format claims — that a miss costs what a hit costs, that a
//! hardlink is one record with two names, that metadata survives verbatim —
//! is decidable here, so it is decided here.
//!
//! The writer and the reader share one definition of the layout, so a round
//! trip cannot catch a mistake made in that definition. What it *can* catch,
//! and what these tests are for, is everything the writer decides on its own:
//! ordering, deduplication, link resolution, alignment, `nlink`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use kisal::image::{Image, directory_entry_type, file_type, inode_flags};

// ---- a counting allocator -------------------------------------------------
//
// The design's claim about the filesystem torrent is that a `stat` is an
// index multiply and some field copies — no parsing, no allocation. That is
// an assertion about the code, so it is asserted, not narrated.

// Per thread, not global. `cargo test` runs these in parallel, so a shared
// counter measures every other test's allocations as well as this one's —
// which is how the first version of this instrument reported 587 allocations
// for a path that makes none. `const` initialisation matters too: a lazily
// initialised thread-local would allocate inside the allocator.
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

fn allocations() -> usize {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

fn allocations_during<R>(body: impl FnOnce() -> R) -> (R, usize) {
    let before = allocations();
    let result = body();
    (result, allocations() - before)
}

// ---- the fixture ----------------------------------------------------------

struct Fixture {
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A tree with one of everything the format has a case for.
fn fixture(label: &str) -> Fixture {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is before the epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("bake-{label}-{unique}"));
    std::fs::create_dir_all(root.join("bin")).expect("mkdir");
    std::fs::create_dir_all(root.join("usr/lib/empty")).expect("mkdir");
    std::fs::create_dir_all(root.join("etc")).expect("mkdir");

    write(&root.join("bin/tool"), b"the tool");
    std::fs::set_permissions(
        root.join("bin/tool"),
        std::fs::Permissions::from_mode(0o4755),
    )
    .expect("chmod");
    // A second name for the same inode, which the index must record as one
    // record with `nlink` 2 rather than as two files.
    std::fs::hard_link(root.join("bin/tool"), root.join("bin/alias")).expect("link");

    write(&root.join("etc/conf"), b"key = value\n");
    // An ELF-shaped file, which the baker page-aligns in the blob.
    write(
        &root.join("usr/lib/libthing.so"),
        b"\x7fELFand then some bytes",
    );
    // Names chosen so that directory order is not insertion order.
    write(&root.join("etc/zulu"), b"z");
    write(&root.join("etc/alpha"), b"a");

    std::os::unix::fs::symlink("usr/lib", root.join("lib")).expect("symlink");

    // Binary values with NULs and high bytes, because that is what the
    // attribute the design actually cares about looks like:
    // `security.capability` is a packed struct, and anything that treats a
    // value as text destroys it. That one cannot be set here without
    // `CAP_SETFCAP`, so its *shape* is what is exercised.
    set_xattr(
        &root.join("bin/tool"),
        b"user.packed",
        &[0x01, 0x00, 0x00, 0x02, 0xff, 0x00],
    );
    set_xattr(&root.join("bin/tool"), b"user.aardvark", b"first by name");
    set_xattr(&root.join("etc/conf"), b"user.empty", b"");

    Fixture { root }
}

fn set_xattr(path: &Path, name: &[u8], value: &[u8]) {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
    let c_name = std::ffi::CString::new(name).expect("name");
    let result = unsafe {
        libc::lsetxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            value.as_ptr() as *const libc::c_void,
            value.len(),
            0,
        )
    };
    assert_eq!(
        result,
        0,
        "the fixture filesystem cannot carry `{}`: {}. Extended attributes \
         are part of what the bake must preserve, so a filesystem that \
         cannot hold one is the wrong place for this fixture rather than a \
         reason to skip the check.",
        String::from_utf8_lossy(name),
        std::io::Error::last_os_error()
    );
}

fn write(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::File::create(path).expect("create");
    file.write_all(bytes).expect("write");
}

/// Walks a path through the index the way kisal's resolution loop will:
/// component by component, asking the store one dumb question each time.
fn resolve<'a>(image: &Image<'a>, path: &str) -> Option<kisal::image::Inode> {
    let mut inode = image.inode(image.root()).expect("root");
    for component in path.split('/').filter(|part| !part.is_empty()) {
        let entry = image
            .lookup(&inode, component.as_bytes())
            .expect("lookup")?;
        inode = image.inode(entry.inode).expect("inode");
    }
    Some(inode)
}

fn bake(fixture: &Fixture) -> baker::Image {
    baker::bake_directory(&fixture.root).expect("bake the fixture")
}

// ---- the tests ------------------------------------------------------------

#[test]
fn a_baked_tree_reads_back_as_the_tree_it_was() {
    let fixture = fixture("shape");
    let baked = bake(&fixture);
    let names = baker::describe(&baked).expect("describe");
    assert_eq!(
        names,
        vec![
            "/bin",
            "/bin/alias",
            "/bin/tool",
            "/etc",
            "/etc/alpha",
            "/etc/conf",
            "/etc/zulu",
            "/lib",
            "/usr",
            "/usr/lib",
            "/usr/lib/empty",
            "/usr/lib/libthing.so",
        ]
    );
}

#[test]
fn a_file_reads_back_byte_for_byte() {
    let fixture = fixture("contents");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    let inode = resolve(&image, "etc/conf").expect("etc/conf exists");
    assert!(inode.is_regular());
    assert_eq!(inode.size, 12);
    assert_eq!(image.contents(&inode).expect("contents"), b"key = value\n");
}

/// The case the design is tuned for: most of a real interpreter's `stat`
/// traffic is for files that are not there, so a miss must cost what a hit
/// costs and must be unambiguous.
#[test]
fn a_missing_name_is_absent_rather_than_an_error() {
    let fixture = fixture("miss");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    let etc = resolve(&image, "etc").expect("etc exists");
    assert!(image.lookup(&etc, b"conf").expect("lookup").is_some());
    assert!(image.lookup(&etc, b"nothing").expect("lookup").is_none());
    // Names that sort before the first entry and after the last, which is
    // where a binary search goes wrong if its bounds are off by one.
    assert!(image.lookup(&etc, b"aaaa").expect("lookup").is_none());
    assert!(image.lookup(&etc, b"zzzz").expect("lookup").is_none());
    // A prefix of a real name, and a real name with something appended.
    assert!(image.lookup(&etc, b"con").expect("lookup").is_none());
    assert!(image.lookup(&etc, b"confx").expect("lookup").is_none());
    assert!(resolve(&image, "etc/missing/deeper").is_none());
}

/// Every entry is findable by the search, not just the ones a hand-written
/// case happened to pick — an off-by-one in the bounds shows up as exactly
/// one unreachable entry.
#[test]
fn every_entry_is_findable_by_name() {
    let fixture = fixture("search");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    for directory_path in ["", "bin", "etc", "usr", "usr/lib"] {
        let directory = resolve(&image, directory_path).expect("directory exists");
        let count = image.entry_count(&directory).expect("count");
        for position in 0..count {
            let entry = image.entry(&directory, position).expect("entry");
            let name = image.string(entry.name_ref).expect("name");
            let found = image
                .lookup(&directory, name)
                .expect("lookup")
                .unwrap_or_else(|| {
                    panic!(
                        "/{directory_path} has an entry `{}` the search cannot find",
                        String::from_utf8_lossy(name)
                    )
                });
            assert_eq!(found.inode, entry.inode);
        }
    }
}

#[test]
fn entries_are_sorted_and_carry_their_type() {
    let fixture = fixture("dirents");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    let etc = resolve(&image, "etc").expect("etc exists");
    let count = image.entry_count(&etc).expect("count");
    let mut names = Vec::new();
    for position in 0..count {
        let entry = image.entry(&etc, position).expect("entry");
        assert_eq!(
            entry.entry_type,
            directory_entry_type::REGULAR,
            "everything in /etc is a regular file"
        );
        names.push(image.string(entry.name_ref).expect("name").to_vec());
    }
    assert_eq!(
        names,
        vec![b"alpha".to_vec(), b"conf".to_vec(), b"zulu".to_vec()]
    );

    // The type is precomputed from the target's mode, which is what lets an
    // importer skip a `stat` per entry.
    let root = image.inode(image.root()).expect("root");
    let types: Vec<(Vec<u8>, u8)> = (0..image.entry_count(&root).expect("count"))
        .map(|position| {
            let entry = image.entry(&root, position).expect("entry");
            (
                image.string(entry.name_ref).expect("name").to_vec(),
                entry.entry_type,
            )
        })
        .collect();
    assert!(types.contains(&(b"bin".to_vec(), directory_entry_type::DIRECTORY)));
    assert!(types.contains(&(b"lib".to_vec(), directory_entry_type::SYMLINK)));
}

/// Two names, one record. Getting this wrong makes `python3` and
/// `python3.11` two files with `nlink` 1 apiece.
#[test]
fn a_hardlink_is_one_inode_with_two_names() {
    let fixture = fixture("hardlink");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    let bin = resolve(&image, "bin").expect("bin exists");
    let tool = image.lookup(&bin, b"tool").expect("lookup").expect("tool");
    let alias = image
        .lookup(&bin, b"alias")
        .expect("lookup")
        .expect("alias");
    assert_eq!(tool.inode, alias.inode, "two names, one inode");

    let inode = image.inode(tool.inode).expect("inode");
    assert_eq!(inode.nlink, 2);
    assert_eq!(image.contents(&inode).expect("contents"), b"the tool");
}

/// A directory's link count is two plus its subdirectories, exactly as POSIX
/// counts `.` and `..`. Real code reads it — an `nlink` of 2 is how `find`
/// knows a directory has no subdirectories worth descending into.
#[test]
fn a_directory_counts_its_subdirectories() {
    let fixture = fixture("nlink");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    // `/` holds bin, etc, usr — three subdirectories — plus the `lib` symlink.
    assert_eq!(image.inode(image.root()).expect("root").nlink, 5);
    assert_eq!(resolve(&image, "usr").expect("usr").nlink, 3);
    assert_eq!(
        resolve(&image, "usr/lib/empty").expect("empty").nlink,
        2,
        "a directory with no subdirectories still has `.` and its parent's entry"
    );
}

#[test]
fn a_symlink_keeps_its_target() {
    let fixture = fixture("symlink");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    let link = resolve(&image, "lib").expect("lib exists");
    assert!(link.is_symlink());
    assert_eq!(image.symlink_target(&link).expect("target"), b"usr/lib");
    assert_eq!(link.size, 7, "a symlink's size is its target's length");
    // Resolution is kisal's job, not the store's: the index reports the
    // target and says nothing about where it points.
    assert!(image.contents(&link).is_err());
}

/// Metadata survives verbatim, including bits kisal does not act on. The
/// preservation is what keeps the enforcement question open.
#[test]
fn metadata_survives_the_bake() {
    let fixture = fixture("metadata");
    let native = std::fs::symlink_metadata(fixture.root.join("bin/tool")).expect("stat");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    let inode = resolve(&image, "bin/tool").expect("bin/tool exists");
    assert_eq!(inode.mode, native.mode(), "mode, setuid bit included");
    assert_eq!(
        inode.mode & 0o4000,
        0o4000,
        "the setuid bit is really there"
    );
    assert_eq!(inode.uid, native.uid());
    assert_eq!(inode.gid, native.gid());
    assert_eq!(inode.mtime_sec, native.mtime());
    assert_eq!(inode.mtime_nsec, native.mtime_nsec() as u32);
    assert_eq!(inode.file_type(), file_type::REGULAR);
}

/// An ELF is page-aligned in the blob so that its offset is congruent with a
/// mapping address. The optimisation that needs it is off; the alignment is
/// cheap now and would mean re-baking every image later.
#[test]
fn an_elf_is_page_aligned_and_flagged() {
    let fixture = fixture("align");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    let elf = resolve(&image, "usr/lib/libthing.so").expect("the library exists");
    assert_ne!(elf.flags & inode_flags::MMAP_ALIGNED, 0);
    assert_eq!(elf.payload % 4096, 0, "the blob offset is page aligned");

    let plain = resolve(&image, "etc/conf").expect("etc/conf exists");
    assert_eq!(plain.flags & inode_flags::MMAP_ALIGNED, 0);
}

/// Extended attributes survive as the bytes they are, in a stable order.
///
/// Nothing interprets them at bake time and nothing may: the attribute this
/// exists for is `security.capability`, a packed binary struct that is the
/// reason `ping` can work without being setuid. If kisal ever honours file
/// capabilities the bits have to be sitting there unmangled.
#[test]
fn extended_attributes_survive_byte_for_byte() {
    let fixture = fixture("xattr");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    let tool = resolve(&image, "bin/tool").expect("bin/tool exists");
    assert_eq!(image.xattr_count(&tool).expect("count"), 2);
    // Sorted by name, so two bakes of one tree are the same bytes.
    let attributes: Vec<(Vec<u8>, Vec<u8>)> = (0..2)
        .map(|position| {
            let (name, value) = image.xattr(&tool, position).expect("xattr");
            (name.to_vec(), value.to_vec())
        })
        .collect();
    assert_eq!(
        attributes,
        vec![
            (b"user.aardvark".to_vec(), b"first by name".to_vec()),
            (
                b"user.packed".to_vec(),
                vec![0x01, 0x00, 0x00, 0x02, 0xff, 0x00]
            ),
        ]
    );

    // An empty value is a real value, distinct from having no attribute.
    let conf = resolve(&image, "etc/conf").expect("etc/conf exists");
    assert_eq!(image.xattr_count(&conf).expect("count"), 1);
    let (name, value) = image.xattr(&conf, 0).expect("xattr");
    assert_eq!(name, b"user.empty");
    assert_eq!(value, b"");

    // A file with none says none, and `xattr_ref` zero is what says it.
    let alpha = resolve(&image, "etc/alpha").expect("etc/alpha exists");
    assert_eq!(alpha.xattr_ref, 0);
    assert_eq!(image.xattr_count(&alpha).expect("count"), 0);
}

/// The same tree bakes to the same bytes. Record and replay rests on it, and
/// so does any hope of diffing two images.
#[test]
fn a_bake_is_reproducible() {
    let fixture = fixture("reproducible");
    let first = bake(&fixture);
    let second = bake(&fixture);
    assert_eq!(first.index, second.index);
    assert_eq!(first.blob, second.blob);
}

/// The index's own accessors allocate nothing: `inode`, `lookup`, and the
/// binary search inside them.
///
/// This measures the *index*, walked by this file's own helper — not kisal's
/// resolution loop and not `stat`, which have their own measurement in
/// `kisal/tests/filesystem.rs` against the real syscall path. The earlier
/// name claimed both and the body did neither, which made the design's
/// load-bearing claim rest on a test of a copy of the code.
#[test]
fn the_index_accessors_allocate_nothing() {
    let fixture = fixture("allocation");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    // Warm anything lazy before measuring, so the count is the work itself.
    let _ = resolve(&image, "usr/lib/libthing.so");

    let (found, allocations) = allocations_during(|| {
        let mut hits = 0;
        for _ in 0..100 {
            if resolve(&image, "usr/lib/libthing.so").is_some() {
                hits += 1;
            }
            // The miss path matters more than the hit: it is most of the
            // torrent, and it is where a naive implementation would build a
            // string to report what it did not find.
            if resolve(&image, "usr/lib/nothing-here").is_none() {
                hits += 1;
            }
        }
        hits
    });
    assert_eq!(found, 200);
    assert_eq!(
        allocations, 0,
        "the index allocated {allocations} times over 200 walks"
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

/// A malformed *header* is a named error, never a panic and never an
/// out-of-bounds read: bad magic, an unsupported version, and a truncation
/// at every length up to the end of the header.
///
/// Narrower than "at every step", which is what this used to claim. The
/// header is where a corrupt index is caught cheaply, and it is what this
/// covers. A structurally valid index carrying impossible *contents* — a
/// dirent name longer than `NAME_MAX`, a parent pointer its parent disowns,
/// an inode count whose byte extent overflows — is checked in
/// `kisal/tests/filesystem.rs`, driven through the syscall rows that would
/// meet it. The image is data we baked ourselves, which is exactly why a
/// corrupt one must say where it broke.
#[test]
fn a_damaged_index_is_refused_by_name() {
    let fixture = fixture("damaged");
    let baked = bake(&fixture);

    assert!(Image::parse(&[], &baked.blob).is_err(), "an empty index");

    let mut wrong_magic = baked.index.clone();
    wrong_magic[0] ^= 0xff;
    assert_eq!(
        Image::parse(&wrong_magic, &baked.blob).err(),
        Some(kisal::image::ImageError::BadMagic)
    );

    let mut wrong_version = baked.index.clone();
    wrong_version[4] = 99;
    assert!(matches!(
        Image::parse(&wrong_version, &baked.blob).err(),
        Some(kisal::image::ImageError::UnsupportedVersion(_))
    ));

    // Every truncation of the index is refused or reads without panicking.
    for length in 0..baked.index.len().min(512) {
        let truncated = &baked.index[..length];
        if let Ok(image) = Image::parse(truncated, &baked.blob) {
            let _ = resolve(&image, "usr/lib/libthing.so");
        }
    }
}

/// `..` has to work from a descriptor the walk did not reach through the
/// root, so a directory's parent is stored rather than tracked.
#[test]
fn every_directory_knows_its_parent() {
    let fixture = fixture("parents");
    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");

    let root = image.root();
    assert_eq!(
        image.inode(root).expect("root").parent,
        root,
        "`/..` is `/`"
    );

    let usr = resolve(&image, "usr").expect("usr exists");
    let lib = resolve(&image, "usr/lib").expect("usr/lib exists");
    let empty = resolve(&image, "usr/lib/empty").expect("empty exists");

    assert_eq!(empty.parent, inode_number(&image, "usr/lib"));
    assert_eq!(lib.parent, inode_number(&image, "usr"));
    assert_eq!(usr.parent, root);

    // A regular file has no parent to speak of, and the field says nothing
    // rather than something that looks meaningful.
    assert_eq!(resolve(&image, "etc/conf").expect("conf").parent, 0);
}

fn inode_number(image: &Image<'_>, path: &str) -> u32 {
    let mut number = image.root();
    for component in path.split('/').filter(|part| !part.is_empty()) {
        let inode = image.inode(number).expect("inode");
        number = image
            .lookup(&inode, component.as_bytes())
            .expect("lookup")
            .expect("exists")
            .inode;
    }
    number
}

/// An empty image is a filesystem with a root, not an absence of one. Every
/// container links one, so a broken empty bake breaks every container that
/// carries no files of its own.
#[test]
fn an_empty_image_has_a_usable_root() {
    let baked = baker::bake_empty();
    let image = Image::parse(&baked.index, &baked.blob).expect("parse an empty image");
    let root = image.inode(image.root()).expect("root inode");
    assert!(
        root.is_directory(),
        "the root of an empty image is a directory"
    );
    assert_eq!(root.parent, image.root(), "and it is its own parent");
    assert_eq!(image.entry_count(&root).expect("count"), 0);
    assert!(image.lookup(&root, b"anything").expect("lookup").is_none());
    assert_eq!(root.nlink, 2);
}

/// Attributes that do not fit the first buffer the reader guesses.
///
/// Both `llistxattr` and `lgetxattr` answer `ERANGE` when the caller's
/// buffer is too small, and both loops here double and retry. A fixture
/// whose longest name is thirteen bytes never reaches either, so the growth
/// is written and never run — this is a tree that needs both.
#[test]
fn attributes_larger_than_the_first_guess_are_read_whole() {
    let fixture = fixture("xattr-growth");
    let target = fixture.root.join("etc/alpha");

    // Forty names of forty-four bytes: 1760 bytes of listing against a
    // 1024-byte first guess.
    let mut expected: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for index in 0..40 {
        let name = format!("user.name{index:02}_padding_padding_padding_padding");
        set_xattr(&target, name.as_bytes(), b"x");
        expected.push((name.into_bytes(), b"x".to_vec()));
    }
    // And one value of three hundred bytes against a 256-byte first guess.
    let long = vec![b'v'; 300];
    set_xattr(&target, b"user.long", &long);
    expected.push((b"user.long".to_vec(), long));
    expected.sort();

    let baked = bake(&fixture);
    let image = Image::parse(&baked.index, &baked.blob).expect("parse");
    let alpha = resolve(&image, "etc/alpha").expect("etc/alpha exists");
    let count = image.xattr_count(&alpha).expect("count") as usize;
    assert_eq!(count, expected.len());
    let read: Vec<(Vec<u8>, Vec<u8>)> = (0..count as u32)
        .map(|position| {
            let (name, value) = image.xattr(&alpha, position).expect("xattr");
            (name.to_vec(), value.to_vec())
        })
        .collect();
    assert_eq!(read, expected);
}

/// A path with no attributes at all, and one on a filesystem that has none.
/// Both are "no attributes", and neither is an error.
#[test]
fn a_path_without_attributes_is_not_an_error() {
    let fixture = fixture("xattr-none");
    assert_eq!(
        baker::xattr::read(&fixture.root.join("etc/alpha")).expect("read"),
        Vec::new()
    );
    // `/proc` answers `ENOTSUP`, which is the other way a path has none.
    assert_eq!(
        baker::xattr::read(std::path::Path::new("/proc/self/status")).expect("read"),
        Vec::new()
    );
}

/// The guard on a value too large for the index to record.
///
/// No filesystem this test can run on will accept a 64 KiB attribute — ext4
/// keeps a file's whole attribute block inside one filesystem block — so the
/// input it exists for is the tarball path, whose PAX records have no such
/// limit. The arithmetic is checked directly rather than left as a branch
/// nobody has run.
#[test]
fn a_value_too_large_for_the_index_is_refused() {
    let path = std::path::Path::new("/somewhere");
    assert!(baker::xattr::refuse_oversize(b"user.x", 0, path).is_ok());
    assert!(baker::xattr::refuse_oversize(b"user.x", u16::MAX as usize, path).is_ok());
    let refusal = baker::xattr::refuse_oversize(b"user.x", u16::MAX as usize + 1, path)
        .expect_err("65536 bytes does not fit a sixteen-bit length");
    let text = format!("{refusal}");
    assert!(text.contains("user.x") && text.contains("65536"), "{text}");
}
