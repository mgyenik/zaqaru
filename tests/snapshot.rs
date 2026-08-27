//! Snapshots of emitted modules, rendered as WebAssembly text.
//!
//! These are the cheap tier: they run in milliseconds and turn a change in
//! the emitter into a readable diff rather than a differential failure three
//! layers down. They cover deliberately small inputs, because the value is in
//! being able to read the whole thing.
//!
//! Set `ZAQARU_UPDATE_SNAPSHOTS=1` to rewrite the expectations after an
//! intended change; the diff is then reviewed as part of the change.

mod support;

use std::path::{Path, PathBuf};

use support::{WorkingDirectory, compile_corpus_object, print_wasm};
use zaqaru::structurer::Mode;

fn snapshot_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join(format!("{name}.wat"))
}

fn transpile_to_text(source: &str, mode: Mode) -> String {
    let workspace = WorkingDirectory::new("snapshot");
    let object_path = compile_corpus_object(&workspace, source);
    let bytes = std::fs::read(&object_path).expect("read compiled object");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse object");
    let wasm = zaqaru::transpile::Transpiler::new(&object)
        .with_mode(mode)
        .transpile()
        .unwrap_or_else(|error| panic!("transpiling {source}: {error:?}"));
    support::validate_wasm(&wasm);
    print_wasm(&wasm)
}

fn compare_snapshot(name: &str, rendered: &str) {
    let path = snapshot_path(name);
    if std::env::var_os("ZAQARU_UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().expect("snapshots directory"))
            .expect("create snapshots directory");
        std::fs::write(&path, rendered).expect("write snapshot");
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "no snapshot at {}: {error}\nrun with ZAQARU_UPDATE_SNAPSHOTS=1 to \
             create it",
            path.display()
        )
    });
    if expected != rendered {
        panic!(
            "snapshot {} is out of date.\n\
             Re-run with ZAQARU_UPDATE_SNAPSHOTS=1 and review the diff.\n\
             \n--- expected ---\n{expected}\n--- emitted ---\n{rendered}",
            path.display()
        );
    }
}

/// The canonical minimal case, whole: imports, the machine-state globals, the
/// translated body, and the host-entry wrapper.
#[test]
fn minimal_function_snapshot() {
    compare_snapshot("add", &transpile_to_text("add.c", Mode::Structured));
}

/// The same input through the dispatcher, which is what an irreducible graph
/// would fall back to; keeping a snapshot of it makes the two shapes easy to
/// compare side by side.
#[test]
fn minimal_function_dispatcher_snapshot() {
    compare_snapshot(
        "add.dispatcher",
        &transpile_to_text("add.c", Mode::Dispatcher),
    );
}
