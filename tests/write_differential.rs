//! M4's oracle: the same program writing to a real directory and to the
//! overlay, compared record for record.
//!
//! `tests/corpus/write.c` issues raw `syscall` instructions, so one source
//! runs both ways — assembled for x86-64 the Linux kernel answers it,
//! transpiled it reaches kisal. Natively it writes into a directory under
//! `/tmp`; under kisal it writes into the overlay over the bake of that same
//! directory. The only difference between the two runs is the path prefix.
//!
//! What is excluded, and why:
//!
//! - **Timestamps nothing set.** A container has no clock of its own, so a
//!   file written here carries a counter rather than a wall time. Comparing
//!   that would compare the two clocks. Times a caller *sets* are compared
//!   exactly, which is the case that matters: a `.pyc` is stale or not by
//!   the number `utimensat` put there, and that number is asserted to the
//!   nanosecond.
//! - **A directory's `st_size` and `st_nlink` beyond the ones under test.**
//!   The size is per-filesystem, as the read differential's header argues.
//!   Link counts *are* compared for directories, because 2-plus-subdirectories
//!   is a rule every filesystem follows.
//! - **`st_dev`, `st_ino`, `st_blocks`, `st_blksize`**, for the reasons in
//!   `tests/filesystem_differential.rs`.
//!
//! Everything else is compared exactly: every errno, every byte read back,
//! every size, every mode, and the merged directory listing with its
//! `d_type`s.

mod support;

use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use support::{
    ALL_MODES, CodeModel, Compiler, TranspileOptions, WorkingDirectory, compile_corpus_object_with,
    link_container_with_image, m1_mounts, transpile_object_configured, validate_wasm,
};

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
                i64::from_le_bytes(chunk[index * 8..index * 8 + 8].try_into().expect("eight"))
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
/// stable one here, so the listing is compared as a set.
const LISTING: i64 = 700;

/// Where `report_stat` puts each field.
const MODE: usize = 1;
const SIZE: usize = 2;

fn normalise(records: &[Record]) -> Vec<Record> {
    let mut ordered: Vec<Record> = records
        .iter()
        .copied()
        .filter(|record| record.tag != LISTING)
        // A directory's size is the size of its own on-disk record, which
        // every filesystem means differently. Link counts stay: 2 plus
        // subdirectories is a rule they all follow.
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
        .filter(|record| record.tag == LISTING)
        .collect();
    entries.sort_by_key(|record| record.values);
    ordered.extend(entries);
    ordered
}

fn build_tree(root: &Path) {
    std::fs::create_dir_all(root.join("etc")).expect("mkdir");
    write(&root.join("etc/hosts"), b"127.0.0.1 localhost\n");
    write(&root.join("etc/hostname"), b"courtyard\n");
    write(&root.join("etc/keep"), b"kept\n");
    std::fs::create_dir_all(root.join("etc/conf.d")).expect("mkdir");
    write(&root.join("etc/conf.d/one"), b"one\n");
}

fn write(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::File::create(path).expect("create");
    file.write_all(bytes).expect("write");
}

fn native_report(workspace: &WorkingDirectory, root: &Path) -> Vec<Record> {
    let library = support::compile_corpus_shared_library(workspace, &["write.c"]);
    let native = unsafe { libloading::Library::new(&library) }.expect("load the native guest");
    let guest: libloading::Symbol<unsafe extern "C" fn(i64, *const u8) -> i64> =
        unsafe { support::native_function(&native, "guest_write") };

    let target = workspace.path().join("native-report");
    let file = std::fs::File::create(&target).expect("create the report");
    let mut prefix = root.to_string_lossy().into_owned().into_bytes();
    prefix.push(0);
    unsafe { guest(file.as_raw_fd() as i64, prefix.as_ptr()) };
    drop(file);
    parse(&std::fs::read(&target).expect("read the report"))
}

#[test]
fn the_writable_layer_matches_the_real_kernel() {
    let workspace = WorkingDirectory::new("m4-differential");
    let root = workspace.path().join("tree");
    build_tree(&root);

    // The native run changes the tree, so the bake is taken first.
    let image = baker::object::emit(&baker::bake_directory(&root).expect("bake"))
        .expect("emit the image object");

    let native = native_report(&workspace, &root);
    assert!(
        native.len() > 40,
        "the oracle produced only {} records, so it did not run",
        native.len()
    );
    let native = normalise(&native);

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
                "write.c",
                Compiler::Gcc,
                CodeModel::PositionIndependent,
                "-O1",
            );
            let object = workspace.path().join(format!("write.{label}.wasm.o"));
            transpile_object_configured(&native_object, &object, options);
            let module = link_container_with_image(&workspace, &[object], &image, &label);
            let bytes = std::fs::read(&module).expect("read the container");
            validate_wasm(&bytes);
            let mut container =
                runner::Container::instantiate(&bytes, m1_mounts()).expect("instantiate");
            container
                .call_guest("guest_write", [1, 0, 0, 0, 0, 0])
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
