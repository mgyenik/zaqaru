//! The register-promotion benchmark: the four kernels from
//! `docs/archive/promotion-plan.md`, each timed under wasmtime in three
//! builds —
//! transpiled from gcc's x86-64 output, transpiled from clang's, and compiled
//! for wasm32 by clang's own backend as the ceiling.
//!
//! What this does and does not measure, said once here rather than implied:
//! wall time under one engine (wasmtime/Cranelift) is one engine's answer;
//! the linked module's size and its static count of `global.get`/`global.set`
//! are printed as diagnostics, and a static count says nothing about
//! execution frequency. The kernels disagree with each other by design —
//! `bench_integer` is all register traffic, `bench_memory` is bounded by
//! linear memory, `bench_float` is XMM traffic, `bench_calls` is the
//! flush-and-reload worst case — and no single column is the verdict.
//!
//! Transpiled kernels are called through the uniform host-entry wrapper; the
//! wasm-native ones through their own typed exports. The workloads are sized
//! so that a call runs for milliseconds and the entry overhead disappears
//! into it. Before any timing, every build's result is checked against the
//! wasm-native build's, so a build that computes the wrong thing cannot post
//! a time.

#[path = "../tests/support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::time::Duration;

use criterion::Criterion;
use support::{
    CodeModel, Compiler, LinkedModule, WorkingDirectory, compile_corpus_object_with, corpus_source,
    link_wasm, run_tool, transpile_object,
};

const SOURCE: &str = "bench_kernels.c";
const KERNELS: [&str; 4] = [
    "bench_integer",
    "bench_memory",
    "bench_float",
    "bench_calls",
];

const INTEGER_ITERATIONS: i32 = 1_000_000;
const INTEGER_SEED: i64 = 0x1234_5678;
const MEMORY_PASSES: i32 = 100;
const MEMORY_SEED: i64 = 42;
const FLOAT_ITERATIONS: i32 = 1_000_000;
const FLOAT_INPUT: f64 = 0.3;
const CALLS_DEPTH: i32 = 27;

/// One linked module ready to run, and how to call into it: the transpiled
/// builds export uniform host-entry wrappers, the wasm-native build exports
/// ordinary typed functions.
struct Build {
    label: &'static str,
    module: LinkedModule,
    typed_exports: bool,
}

impl Build {
    fn run_integer(&mut self) -> i64 {
        if self.typed_exports {
            self.module
                .call::<(i32, i64), i64>("bench_integer", (INTEGER_ITERATIONS, INTEGER_SEED))
        } else {
            self.module.call_guest(
                "bench_integer",
                [i64::from(INTEGER_ITERATIONS), INTEGER_SEED, 0, 0, 0, 0],
            )
        }
    }

    fn run_memory(&mut self) -> i64 {
        if self.typed_exports {
            self.module
                .call::<(i32, i64), i64>("bench_memory", (MEMORY_PASSES, MEMORY_SEED))
        } else {
            self.module.call_guest(
                "bench_memory",
                [i64::from(MEMORY_PASSES), MEMORY_SEED, 0, 0, 0, 0],
            )
        }
    }

    fn run_float(&mut self) -> f64 {
        if self.typed_exports {
            self.module
                .call::<(i32, f64), f64>("bench_float", (FLOAT_ITERATIONS, FLOAT_INPUT))
        } else {
            self.module
                .call_guest_fully(
                    "bench_float",
                    [i64::from(FLOAT_ITERATIONS), 0, 0, 0, 0, 0],
                    [FLOAT_INPUT, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                )
                .1
        }
    }

    fn run_calls(&mut self) -> i64 {
        if self.typed_exports {
            self.module.call::<i32, i64>("bench_calls", CALLS_DEPTH)
        } else {
            self.module
                .call_guest("bench_calls", [i64::from(CALLS_DEPTH), 0, 0, 0, 0, 0])
        }
    }

    fn run(&mut self, kernel: &str) -> u64 {
        match kernel {
            "bench_integer" => self.run_integer() as u64,
            "bench_memory" => self.run_memory() as u64,
            "bench_float" => self.run_float().to_bits(),
            "bench_calls" => self.run_calls() as u64,
            other => panic!("no kernel named `{other}`"),
        }
    }
}

/// Transpiles the kernels from one compiler's x86-64 output and links them.
fn transpiled_build(
    workspace: &WorkingDirectory,
    compiler: Compiler,
    label: &'static str,
    mode: zaqaru::structurer::Mode,
) -> Build {
    let object = compile_corpus_object_with(
        workspace,
        SOURCE,
        compiler,
        CodeModel::PositionIndependent,
        "-O2",
    );
    let wasm_object = workspace.path().join(format!("{label}.wasm.o"));
    transpile_object(&object, &wasm_object, mode);
    let linked = workspace.path().join(format!("{label}.wasm"));
    link_wasm(&[wasm_object], &linked, &[]);
    report_module(label, &linked);
    Build {
        label,
        module: LinkedModule::instantiate(&linked),
        typed_exports: false,
    }
}

/// Compiles the kernels with clang's own wasm backend at the same `-O2` and
/// links them: the ceiling the transpiled builds are measured against.
fn wasm_native_build(workspace: &WorkingDirectory) -> Build {
    let source = corpus_source(SOURCE).to_string_lossy().into_owned();
    let object = workspace.path().join("native.wasm.o");
    let object_text = object.to_string_lossy().into_owned();
    run_tool(
        "clang",
        &["--target=wasm32", "-O2", "-c", &source, "-o", &object_text],
    );
    let exports: Vec<String> = KERNELS
        .iter()
        .map(|kernel| format!("--export={kernel}"))
        .collect();
    let export_arguments: Vec<&str> = exports.iter().map(String::as_str).collect();
    let linked = workspace.path().join("native.wasm");
    link_wasm(&[object], &linked, &export_arguments);
    report_module("wasm-native", &linked);
    Build {
        label: "wasm-native",
        module: LinkedModule::instantiate(&linked),
        typed_exports: true,
    }
}

/// Prints the two static diagnostics for a linked module: its size, and how
/// many `global.get`/`global.set` instructions its text form contains. The
/// count weighs every site the same whether it runs a million times or never,
/// which is why it is a diagnostic and not a result.
fn report_module(label: &str, linked: &PathBuf) {
    let bytes = std::fs::read(linked).expect("read linked module");
    let text = wasmprinter::print_bytes(&bytes).expect("render linked module");
    let global_accesses = text
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            line.starts_with("global.get") || line.starts_with("global.set")
        })
        .count();
    eprintln!(
        "{label}: {} bytes, {global_accesses} static global accesses",
        bytes.len()
    );
}

fn main() {
    let workspace = WorkingDirectory::new("bench-kernels");
    let mut builds = vec![
        transpiled_build(
            &workspace,
            Compiler::Gcc,
            "transpiled-gcc",
            zaqaru::structurer::Mode::Structured,
        ),
        transpiled_build(
            &workspace,
            Compiler::Clang,
            "transpiled-clang",
            zaqaru::structurer::Mode::Structured,
        ),
        // The dispatcher is what a resumed frame's remainder runs in, so its
        // distance from the structured translation is the cost of having
        // been suspended — a number the scheduler design needs.
        transpiled_build(
            &workspace,
            Compiler::Gcc,
            "transpiled-gcc-dispatcher",
            zaqaru::structurer::Mode::Dispatcher,
        ),
        transpiled_build(
            &workspace,
            Compiler::Clang,
            "transpiled-clang-dispatcher",
            zaqaru::structurer::Mode::Dispatcher,
        ),
        wasm_native_build(&workspace),
    ];

    // Correctness gate: every build must agree with the wasm-native build on
    // every kernel before any of them is worth timing.
    for kernel in KERNELS {
        let (transpiled, native) = builds.split_at_mut(4);
        let expected = native[0].run(kernel);
        for build in transpiled {
            let actual = build.run(kernel);
            assert_eq!(
                actual, expected,
                "`{kernel}` disagrees: {} returned {actual:#x}, wasm-native returned {expected:#x}",
                build.label,
            );
        }
    }

    let mut criterion = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
        .sample_size(20)
        .configure_from_args();

    for kernel in KERNELS {
        let mut group = criterion.benchmark_group(kernel);
        for build in &mut builds {
            group.bench_function(build.label, |bencher| bencher.iter(|| build.run(kernel)));
        }
        group.finish();
    }

    criterion.final_summary();
}
