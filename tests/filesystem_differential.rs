//! M3's integration oracle: the same filesystem program, run by the real
//! kernel and by kisal, compared record for record.
//!
//! `tests/corpus/filesystem.c` issues raw `syscall` instructions, so the same
//! source runs both ways — assembled for x86-64 the Linux kernel answers it,
//! transpiled it reaches kisal. Natively it walks a directory under `/tmp`;
//! under kisal it walks the bake of that same directory, mounted at the root.
//! The only difference between the two runs is the path prefix.
//!
//! Three `struct stat` fields are excluded from the comparison, each for a
//! reason rather than because it disagreed:
//!
//! - **`st_dev`** — a real filesystem's device number is whatever the host
//!   assigned it that boot. There is nothing for an image to match.
//! - **`st_ino`** — likewise; what matters is that two paths naming one file
//!   report the *same* inode, which the equivalence checks cover by
//!   comparing paths against each other rather than against a number.
//! - **`st_blocks`** — ext4 allocates a 4 KiB block for a 20-byte file and
//!   reports eight 512-byte units; an image allocates nothing and reports
//!   what the size implies. Neither is wrong, and no program that runs in a
//!   container can depend on the answer.
//! - **`st_blksize`** — the same argument: a filesystem's preferred I/O size
//!   is a property of the device it is on, and the image is not on one.
//! - **`st_size` of a directory** — the size of the directory's own record,
//!   which every filesystem reports and each means differently: ext4 answers
//!   its block size, tmpfs a count-derived number, squashfs the length of
//!   its listing. The image answers the length of its entry block. A tar
//!   archive carries no directory size at all, so matching the host's would
//!   mean the two bake front ends disagreeing about the same tree. Regular
//!   files' and symlinks' sizes are compared exactly.
//! - **`st_atim` and `st_ctim`** — the image stores one timestamp, because an
//!   OCI layer is a tar archive and tar carries `mtime`. kisal reports it for
//!   all three, which is what a filesystem built from an archive can honestly
//!   say; the host's real access and change times have nothing to match.
//! - **`statx`'s `stx_mask`** — the real kernel advertises `STATX_BTIME` and
//!   `STATX_MNT_ID`; the image has no birth time and there is one mount, so
//!   kisal advertises neither. The fields it *does* advertise are compared.
//!
//! What is compared, exactly: `st_mode` including the file type bits,
//! `st_nlink`, `st_size`, `st_uid`, `st_gid`, `st_mtime`, the same six
//! through `statx`, every errno from every call, the bytes read at every
//! offset, the symlink targets, `access` for all of `F_OK`/`X_OK`/`W_OK`/
//! `R_OK` and a garbage mode, the `ELOOP` boundary at forty traversals, and
//! two directory listings with their `d_type`s.

mod support;

use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use support::{
    ALL_MODES, CodeModel, Compiler, TranspileOptions, WorkingDirectory, compile_corpus_object_with,
    link_container_with_image, m1_mounts, transpile_object_configured, validate_wasm,
};

/// One record the guest emits: a tag, a result, and five values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Record {
    tag: i64,
    result: i64,
    values: [i64; 5],
}

fn parse(bytes: &[u8]) -> Vec<Record> {
    assert_eq!(bytes.len() % 64, 0, "the report is whole records");
    bytes
        .chunks_exact(64)
        .map(|chunk| {
            let word = |index: usize| {
                i64::from_le_bytes(
                    chunk[index * 8..index * 8 + 8]
                        .try_into()
                        .expect("eight bytes"),
                )
            };
            Record {
                tag: word(0),
                result: word(1),
                values: [word(2), word(3), word(4), word(5), word(6)],
            }
        })
        .collect()
}

/// Directory entries have no ordering guarantee on a real filesystem and a
/// stable one in an image, so the listing is compared as a set. Everything
/// else is compared in order, because order is part of what is under test.
const DIRECTORY_ENTRY_TAGS: [i64; 2] = [701, 711];

/// `S_IFDIR`, and where the guest puts `st_mode` in a record.
const MODE: usize = 1;
const SIZE: usize = 2;

fn normalise(records: &[Record]) -> Vec<Record> {
    let is_entry = |record: &Record| DIRECTORY_ENTRY_TAGS.contains(&record.tag);
    let mut ordered: Vec<Record> = records
        .iter()
        .copied()
        .filter(|record| !is_entry(record))
        // A directory's `st_size` is per-filesystem; see the exclusion list
        // in this file's header. Dropped from both sides rather than from
        // the guest, so the guest stays the same program on both.
        .map(|record| {
            if record.values[MODE] & 0o170000 == 0o040000 {
                let mut record = record;
                record.values[SIZE] = 0;
                record
            } else {
                record
            }
        })
        .collect();
    let mut entries: Vec<Record> = records
        .iter()
        .copied()
        .filter(is_entry)
        // The index within the listing is exactly the thing that has no
        // guarantee, so it is dropped before sorting.
        .map(|record| Record {
            result: 0,
            ..record
        })
        .collect();
    entries.sort_by_key(|record| (record.tag, record.values));
    ordered.extend(entries);
    ordered
}

// ---- the fixture -----------------------------------------------------------

fn build_tree(root: &Path) {
    std::fs::create_dir_all(root.join("etc")).expect("mkdir");
    std::fs::create_dir_all(root.join("usr/lib")).expect("mkdir");

    write(&root.join("etc/hosts"), b"127.0.0.1 localhost\n");
    write(&root.join("etc/hostname"), b"courtyard\n");
    write(&root.join("usr/lib/libthing.so"), b"\x7fELFcontents");
    write(&root.join("script"), b"#!/bin/sh\necho hi\n");
    std::fs::set_permissions(root.join("script"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");

    std::os::unix::fs::symlink("usr/lib", root.join("lib")).expect("symlink");
    std::os::unix::fs::symlink("etc/hosts", root.join("hosts-link")).expect("symlink");
    // Absolute targets cannot agree across the two mounts — natively `/etc`
    // is the host's — so this one points at something that exists in neither
    // and is compared as a failure on both sides.
    std::os::unix::fs::symlink("/nonexistent-on-purpose", root.join("absolute-link"))
        .expect("symlink");
    std::os::unix::fs::symlink("../etc", root.join("usr/etc-link")).expect("symlink");

    // A directory, a symlink and a fifo inside the directory that gets
    // listed, so `getdents64` reports more than one `d_type` there too.
    std::fs::create_dir_all(root.join("etc/conf.d")).expect("mkdir");
    std::os::unix::fs::symlink("hosts", root.join("etc/hosts-alias")).expect("symlink");
    let fifo = std::ffi::CString::new(root.join("etc/pipe").as_os_str().as_bytes()).expect("path");
    assert_eq!(
        unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) },
        0,
        "the fixture filesystem cannot hold a fifo: {}",
        std::io::Error::last_os_error()
    );

    // Symlink chains, in their own directory so the root's listing stays
    // small: forty links resolve and forty-one are `ELOOP`, on both sides.
    std::fs::create_dir_all(root.join("chain")).expect("mkdir");
    for step in 0..45 {
        let target = if step == 0 {
            "../etc/hosts".to_string()
        } else {
            format!("link{}", step - 1)
        };
        std::os::unix::fs::symlink(target, root.join(format!("chain/link{step}")))
            .expect("symlink");
    }
    // A genuine cycle, which is what `ELOOP` is named for.
    std::os::unix::fs::symlink("loop-b", root.join("loop-a")).expect("symlink");
    std::os::unix::fs::symlink("loop-a", root.join("loop-b")).expect("symlink");
}

fn write(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::File::create(path).expect("create");
    file.write_all(bytes).expect("write");
}

/// What the real kernel does with the program.
fn native_report(workspace: &WorkingDirectory, root: &Path) -> Vec<Record> {
    let library = support::compile_corpus_shared_library(workspace, &["filesystem.c"]);
    let native = unsafe { libloading::Library::new(&library) }.expect("load the native guest");
    let guest: libloading::Symbol<unsafe extern "C" fn(i64, *const u8) -> i64> =
        unsafe { support::native_function(&native, "guest_filesystem") };

    let target = workspace.path().join("native-report");
    let file = std::fs::File::create(&target).expect("create the report");
    let mut prefix = root.to_string_lossy().into_owned().into_bytes();
    prefix.push(0);
    unsafe { guest(file.as_raw_fd() as i64, prefix.as_ptr()) };
    drop(file);
    parse(&std::fs::read(&target).expect("read the report"))
}

#[test]
fn the_filesystem_matches_the_real_kernel() {
    let workspace = WorkingDirectory::new("m3-differential");
    let root = workspace.path().join("tree");
    build_tree(&root);

    let native = native_report(&workspace, &root);
    assert!(
        native.len() > 40,
        "the oracle produced only {} records, so it did not run",
        native.len()
    );
    let native = normalise(&native);

    let image = baker::object::emit(&baker::bake_directory(&root).expect("bake"))
        .expect("emit the image object");

    for mode in ALL_MODES {
        for promote in [true, false] {
            let options = TranspileOptions {
                mode,
                promote,
                resume: false,
            };
            let label = options.label().replace('/', ".");
            let native_object = compile_corpus_object_with(
                &workspace,
                "filesystem.c",
                Compiler::Gcc,
                CodeModel::PositionIndependent,
                "-O1",
            );
            let object = workspace.path().join(format!("filesystem.{label}.wasm.o"));
            transpile_object_configured(&native_object, &object, options);
            let module = link_container_with_image(&workspace, &[object], &image, &label);

            let bytes = std::fs::read(&module).expect("read the container");
            validate_wasm(&bytes);
            let mut container =
                runner::Container::instantiate(&bytes, m1_mounts()).expect("instantiate");

            // The image is mounted at the root, so the guest's prefix is
            // empty — the one difference between the two runs, and spelled
            // as a null pointer so that nothing has to be placed in guest
            // memory for it.
            container
                .call_guest("guest_filesystem", [1, 0, 0, 0, 0, 0])
                .unwrap_or_else(|error| panic!("[{label}] the guest trapped: {error:?}"));

            let report = container
                .mounts()
                .read(&[b"iso".to_vec(), b"console".to_vec(), b"stdout".to_vec()])
                .expect("the console mount failed")
                .unwrap_or_default();
            let emulated = normalise(&parse(&report));

            assert_eq!(
                emulated.len(),
                native.len(),
                "[{label}] the two runs made different numbers of calls"
            );
            for (index, (left, right)) in emulated.iter().zip(&native).enumerate() {
                assert_eq!(
                    left, right,
                    "[{label}] record {index} (tag {}) differs: kisal {left:?}, kernel {right:?}",
                    left.tag
                );
            }
        }
    }
}

/// The oracle has to be able to fail. A tree the image does not match must
/// produce a difference, or the comparison above proves nothing.
#[test]
fn the_comparison_notices_a_divergent_image() {
    let workspace = WorkingDirectory::new("m3-control");
    let root = workspace.path().join("tree");
    build_tree(&root);
    let native = normalise(&native_report(&workspace, &root));

    // Bake a tree that differs in one byte of one file's contents, which
    // changes a size and the bytes read back.
    let divergent = workspace.path().join("divergent");
    build_tree(&divergent);
    write(&divergent.join("etc/hosts"), b"127.0.0.1 elsewhere!\n");
    let image =
        baker::object::emit(&baker::bake_directory(&divergent).expect("bake")).expect("emit");

    let native_object = compile_corpus_object_with(
        &workspace,
        "filesystem.c",
        Compiler::Gcc,
        CodeModel::PositionIndependent,
        "-O1",
    );
    let object = workspace.path().join("filesystem.control.wasm.o");
    transpile_object_configured(
        &native_object,
        &object,
        TranspileOptions::new(zaqaru::structurer::Mode::Structured),
    );
    let module = link_container_with_image(&workspace, &[object], &image, "control");
    let bytes = std::fs::read(&module).expect("read");
    let mut container = runner::Container::instantiate(&bytes, m1_mounts()).expect("instantiate");
    container
        .call_guest("guest_filesystem", [1, 0, 0, 0, 0, 0])
        .expect("the guest trapped");
    let report = container
        .mounts()
        .read(&[b"iso".to_vec(), b"console".to_vec(), b"stdout".to_vec()])
        .expect("mount")
        .unwrap_or_default();
    let emulated = normalise(&parse(&report));

    assert_ne!(
        emulated, native,
        "the comparison cannot tell a different filesystem from the right one"
    );
}

/// A container whose image is the bake of an empty tree finds nothing, which
/// is a different answer from finding the wrong thing.
#[test]
fn an_empty_image_reports_every_path_missing() {
    let workspace = WorkingDirectory::new("m3-empty");
    let native_object = compile_corpus_object_with(
        &workspace,
        "filesystem.c",
        Compiler::Gcc,
        CodeModel::PositionIndependent,
        "-O1",
    );
    let object = workspace.path().join("filesystem.empty.wasm.o");
    transpile_object_configured(
        &native_object,
        &object,
        TranspileOptions::new(zaqaru::structurer::Mode::Structured),
    );
    let module = link_container_with_image(
        &workspace,
        &[object],
        &baker::object::empty().expect("empty image"),
        "empty",
    );
    let bytes = std::fs::read(&module).expect("read");
    let mut container = runner::Container::instantiate(&bytes, m1_mounts()).expect("instantiate");
    container
        .call_guest("guest_filesystem", [1, 0, 0, 0, 0, 0])
        .expect("the guest trapped");
    let report = container
        .mounts()
        .read(&[b"iso".to_vec(), b"console".to_vec(), b"stdout".to_vec()])
        .expect("mount")
        .unwrap_or_default();
    let records = parse(&report);

    // Every `stat` of a named path is `ENOENT`, and the root itself is still
    // a directory — an empty image is a filesystem, not an absence of one.
    let stats: Vec<&Record> = records
        .iter()
        .filter(|record| (100..112).contains(&record.tag))
        .collect();
    assert!(!stats.is_empty());
    for record in stats {
        if record.tag == 105 {
            assert_eq!(record.result, 0, "the root exists");
        } else {
            assert_eq!(record.result, -2, "tag {} is ENOENT", record.tag);
        }
    }
}
