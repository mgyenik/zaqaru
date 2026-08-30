//! A container that carries no translated code at all, and runs anyway.
//!
//! Every other container in this suite is a bake's output: the translator
//! turned a program's functions into wasm functions, the linker welded them
//! together, and the module *is* the program. This one carries the program
//! as **data** — the same bytes a distribution shipped, in the image, with
//! nothing having read them — and an interpreter that decodes at the program
//! counter.
//!
//! What that changes about the artifact is the whole point of `docs/vm.md`:
//! the module is the engine, the image is the program, and a bake is
//! assembly plus a link rather than a translation.

mod support;

use std::path::Path;

use support::WorkingDirectory;

/// Builds the engine's staticlib for wasm32.
fn targum_staticlib() -> std::path::PathBuf {
    support::wasm_staticlib("targum", "libtargum.a")
}

/// Links engine + kernel + FPU + image into one module.
///
/// Shorter than the ahead-of-time link by everything that made that one
/// hard: no seam object, because a syscall is a Rust call; no translated
/// guest objects, because there is no translation; no exec map and no
/// resume bodies, because nothing holds guest state on the wasm stack.
fn link_engine(workspace: &WorkingDirectory, image: &[u8], label: &str) -> std::path::PathBuf {
    let image = workspace.write(&format!("image.{label}.wasm.o"), image);
    let objects = vec![
        image,
        support::kisal_staticlib(),
        targum_staticlib(),
        support::x87_staticlib(),
    ];
    let linked = workspace.path().join(format!("engine.{label}.wasm"));
    support::link_wasm(
        &objects,
        &linked,
        &[
            "--fatal-warnings",
            "--export=cabi_realloc",
            "--export=targum_boot",
            // The guest's program is loaded low, so the module's own data
            // has to be above it. Sixty-four megabytes of room, which is
            // what the native side reserves for the same reason.
            "--global-base=67108864",
        ],
    );
    linked
}

/// Copies a program's shared libraries into the tree at the absolute paths
/// it will ask for them by — which is what `PT_INTERP` and every
/// `DT_NEEDED` entry holds, and what the guest's loader resolves through the
/// guest's own filesystem.
fn copy_libraries(root: &Path, program: &Path) {
    let listed = std::process::Command::new("ldd")
        .arg(program)
        .output()
        .expect("run ldd");
    assert!(listed.status.success(), "ldd failed on {}", program.display());
    let text = String::from_utf8_lossy(&listed.stdout).into_owned();
    let mut copied = 0;
    for line in text.lines() {
        let path = match line.split_whitespace().collect::<Vec<_>>()[..] {
            [_, "=>", path, ..] => path,
            [path, ..] if path.starts_with('/') => path,
            _ => continue,
        };
        let source = Path::new(path);
        if !source.is_file() {
            continue;
        }
        let destination = root.join(path.trim_start_matches('/'));
        std::fs::create_dir_all(destination.parent().expect("a parent")).expect("mkdir");
        std::fs::copy(source, &destination).expect("copy a library");
        copied += 1;
    }
    assert!(copied > 0, "no libraries were copied");
}

/// A program, baked with the engine, run under wasmtime.
fn run(label: &str, source: &str) -> (i32, String) {
    run_linked(label, source, &["-static", "-no-pie"])
}

fn run_linked(label: &str, source: &str, linkage: &[&str]) -> (i32, String) {
    let workspace = WorkingDirectory::new(label);
    let root = workspace.path().join("root");
    std::fs::create_dir_all(&root).expect("mkdir");
    let file = root.join("program.c");
    std::fs::write(&file, source).expect("write the source");
    let program = root.join("init");
    let built = std::process::Command::new("gcc")
        .arg(&file)
        .args(linkage)
        .args([
            "-fcf-protection=none",
            "-fno-stack-protector",
            "-fno-asynchronous-unwind-tables",
        ])
        .arg("-o")
        .arg(&program)
        .output()
        .expect("run gcc");
    assert!(
        built.status.success(),
        "compiling {label} failed:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    std::fs::remove_file(&file).expect("the source is not part of the image");
    if !linkage.contains(&"-static") {
        copy_libraries(&root, &program);
    }

    let baked = baker::bake_directory(&root).expect("bake");
    let object = baker::object::emit(&baked).expect("emit the image object");
    let module = link_engine(&workspace, &object, label);

    let mut container = runner::Container::instantiate(
        &std::fs::read(&module).expect("read the container"),
        support::mounts_seeded(&[0x33; 32]),
    )
    .expect("instantiate the container");

    let status = container
        .call::<(), i32>("targum_boot", ())
        .unwrap_or_else(|error| {
            let log = container
                .mounts()
                .read(&[b"iso".to_vec(), b"log".to_vec(), b"error".to_vec()])
                .ok()
                .flatten()
                .unwrap_or_default();
            panic!(
                "the container did not finish: {error:?}\nkernel log: {}",
                String::from_utf8_lossy(&log)
            )
        });

    let written = container
        .mounts()
        .read(&[b"iso".to_vec(), b"console".to_vec(), b"stdout".to_vec()])
        .ok()
        .flatten()
        .unwrap_or_default();
    (status, String::from_utf8(written).expect("utf-8"))
}

/// The artifact the design is about: engine plus image, and a program the
/// bake never looked at.
#[test]
fn a_program_the_bake_never_translated_runs_in_a_module() {
    let (status, out) = run(
        "hello",
        r#"
#include <stdio.h>
int main(void) {
    printf("%s %d\n", "interpreted", 6 * 7);
    return 0;
}
"#,
    );
    assert_eq!(status, 0, "the container did not exit cleanly");
    assert_eq!(out, "interpreted 42\n");
}

/// The same, for a program nothing placed and nothing translated: the
/// dynamic loader runs *inside* the module, interpreted, and maps `libc` for
/// itself.
///
/// Two things the ahead-of-time path cannot do are in this one test. There
/// is no prelink — no bases assigned at bake, no modules region, no forced
/// placement — because the address space answers where a shared object goes
/// at load time, the way a kernel does. And the loader writes relocations
/// into pages it is about to execute, which is the case the block cache's
/// invalidation exists for.
#[test]
fn a_dynamic_program_and_its_loader_run_in_a_module() {
    let (status, out) = run_linked(
        "dynamic",
        r#"
#include <stdio.h>
#include <string.h>
int main(void) {
    char buffer[64];
    snprintf(buffer, sizeof buffer, "%s %d", "loaded", 6 * 7);
    printf("%s %zu\n", buffer, strlen(buffer));
    return 0;
}
"#,
        &[],
    );
    assert_eq!(status, 0, "the container did not exit cleanly");
    assert_eq!(out, "loaded 42 9\n");
}
