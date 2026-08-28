//! M5's oracle: the same memory program, run by the real kernel and by
//! kisal, compared record for record.
//!
//! `tests/corpus/memory.c` issues raw `syscall` instructions, so one source
//! runs both ways. What it reports is deliberately free of addresses: the
//! two sides lay out their address spaces differently and always will — one
//! is a real kernel's virtual memory, the other a stretch of wasm linear
//! memory — so what is compared is the *rules*. Whether a fresh mapping
//! reads as zeros. Whether it keeps what was written. Whether replacing part
//! of it leaves the rest. Which arguments are refused, and with which errno.
//! Whether a file mapping holds the same bytes `read(2)` returns.
//!
//! One rule is deliberately not compared, because it cannot be: a page
//! *entirely* past a file's end is not backed on Linux, and touching one
//! raises SIGBUS. Wasm has no faults, so kisal answers zeros there and no
//! implementation of it could do otherwise. What the corpus checks instead
//! is the zero-fill of the file's last, partial page, which is the portable
//! guarantee and the one every program relies on. Found by the native run
//! taking the signal.
//!
//! `brk` is not in it. The native side runs inside this test process, whose
//! heap *is* the break, and moving it would move glibc's allocator out from
//! under the harness. It is covered in `kisal/tests/memory.rs`, where the
//! arena belongs to nobody else.

mod support;

use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use support::{
    ALL_MODES, CodeModel, Compiler, TranspileOptions, WorkingDirectory,
    compile_corpus_object_with, link_container_with_image, m1_mounts, transpile_object_configured,
    validate_wasm,
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

/// The fixture the corpus maps: four pages and a bit, so a mapping can cover
/// whole pages of it and still run off the end.
const PATTERNED: usize = 4 * 4096 + 1000;

fn build_tree(root: &Path) {
    std::fs::create_dir_all(root).expect("mkdir");
    let patterned: Vec<u8> = (0..PATTERNED as u32).map(|index| (index % 251) as u8).collect();
    let mut file = std::fs::File::create(root.join("patterned")).expect("create");
    file.write_all(&patterned).expect("write");
}

fn native_report(workspace: &WorkingDirectory, root: &Path) -> Vec<Record> {
    let library = support::compile_corpus_shared_library(workspace, &["memory.c"]);
    let native = unsafe { libloading::Library::new(&library) }.expect("load the native guest");
    let guest: libloading::Symbol<unsafe extern "C" fn(i64, *const u8) -> i64> =
        unsafe { support::native_function(&native, "guest_memory") };

    let target = workspace.path().join("native-report");
    let file = std::fs::File::create(&target).expect("create the report");
    let mut prefix = root.to_string_lossy().into_owned().into_bytes();
    prefix.push(0);
    unsafe { guest(file.as_raw_fd() as i64, prefix.as_ptr()) };
    drop(file);
    parse(&std::fs::read(&target).expect("read the report"))
}

#[test]
fn the_memory_rows_match_the_real_kernel() {
    let workspace = WorkingDirectory::new("m5-differential");
    let root = workspace.path().join("tree");
    build_tree(&root);

    let native = native_report(&workspace, &root);
    assert!(
        native.len() > 30,
        "the oracle produced only {} records, so it did not run",
        native.len()
    );

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
                "memory.c",
                Compiler::Gcc,
                CodeModel::PositionIndependent,
                "-O1",
            );
            let object = workspace.path().join(format!("memory.{label}.wasm.o"));
            transpile_object_configured(&native_object, &object, options);
            let module = link_container_with_image(&workspace, &[object], &image, &label);
            let bytes = std::fs::read(&module).expect("read the container");
            validate_wasm(&bytes);
            let mut container =
                runner::Container::instantiate(&bytes, m1_mounts()).expect("instantiate");
            container
                .call_guest("guest_memory", [1, 0, 0, 0, 0, 0])
                .unwrap_or_else(|error| panic!("[{label}] the guest trapped: {error:?}"));

            let report = container
                .mounts()
                .read(&[b"iso".to_vec(), b"console".to_vec(), b"stdout".to_vec()])
                .expect("the console mount failed")
                .unwrap_or_default();
            let emulated = parse(&report);

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
