//! The fork technique, end to end: a checkpoint taken at a call boundary is
//! brought back to life in a fresh instance **without re-executing anything**.
//!
//! `tests/call_boundary_state.rs` established the state half: at a call
//! boundary the whole machine is in the register globals and linear memory.
//! This test adds the control half — the `--resume` machinery. With it on,
//! every call site stores a *resume ID* in the return-address slot it
//! reserves (table slot of the enclosing function's resume body low, entry
//! index high), so the guest stack carries a serialization of the suspended
//! frames. The `x86_resume` driver walks that chain: each resume body enters
//! at its post-call block, runs its frame to the `ret`, and yields the ID of
//! the frame above, until the sentinel at the bottom.
//!
//! The shape of each scenario is fork itself:
//!
//!  - **record** (the parent): runs to completion; at the fork call, the
//!    host snapshots memory and globals; `g` answers `REPLY_PARENT`.
//!  - **child oracle**: a full ordinary run in which `g` answers
//!    `REPLY_CHILD` from the fork call onwards — what the child *should*
//!    compute, produced without any resume machinery. Must differ from the
//!    parent's result, or the scenario proves nothing.
//!  - **fork**: a fresh instance whose entry wrapper is never called. The
//!    snapshot is written into its memory and globals, `rax` is set to
//!    `REPLY_CHILD` — fork's "returns 0 in the child" — and `x86_resume`
//!    runs. Its result must equal the child oracle's exactly, and `g` must
//!    not have been called more times than the child's own fresh calls: the
//!    prefix provably never re-ran.
//!
//! The entries are chosen for the resume shapes they force. `snap_entry`
//! suspends two straight-line frames. `tail_entry` reaches `g` through a
//! sibling call at `-O2`, so its suspended frame resumes at the epilogue
//! arm — the synthetic entry for a frame with nothing left but its return.
//! `loop_entry` forks at the third of eight calls made through a volatile
//! function pointer: the resume lands mid-loop in a frame suspended at a
//! `call_indirect` site, and the resumed child makes five fresh calls
//! through the ordinary fast path. The assembly guest pins four
//! callee-saved registers and a spill across the fork, removing the
//! compiler's discretion.

mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};

use support::{
    ALL_COMPILERS, ALL_MODES, ALL_OPTIMISATION_LEVELS, CORPUS_COMPILE_FLAGS, CodeModel, Compiler,
    WorkingDirectory, link_wasm, run_tool, transpile_object_resumable,
};

const C_SOURCE: &str = r#"
extern long g(long request);

static unsigned long stir(unsigned long value) {
    value ^= value >> 13;
    value *= 0x9e3779b97f4a7c15UL;
    value ^= value << 7;
    return value;
}

__attribute__((noinline)) long middle(long seed) {
    unsigned long s = (unsigned long)seed;
    unsigned long a = stir(s + 0x1111);
    unsigned long b = stir(s ^ 0x2222);
    unsigned long c = a * 33 + b;
    double d = (double)(s & 0xffff) * 1.0009765625;
    long r = g((long)(a ^ b));
    unsigned long post = stir((unsigned long)r + c);
    return (long)(post ^ a ^ (b << 1) ^ (unsigned long)(long)(d * 4.0));
}

long snap_entry(long seed) {
    unsigned long s = (unsigned long)seed;
    unsigned long p = stir(s * 3 + 7);
    unsigned long q = (unsigned long)middle(seed);
    return (long)(p * 2654435761UL ^ q);
}

__attribute__((noinline)) long tail_mid(long x) {
    unsigned long a = stir((unsigned long)x);
    long r = g((long)(a & 0xffff));
    return (long)(a ^ (unsigned long)r * 5);
}

long tail_entry(long seed) {
    /* A sibling call at -O2: the caller's frame suspends at a tail-jump
       site, whose resume point is the epilogue arm. */
    return tail_mid(seed ^ 0x1357);
}

long (*volatile middle_pointer)(long) = middle;

long loop_entry(long seed) {
    unsigned long acc = (unsigned long)seed;
    for (int i = 0; i < 8; i++) {
        acc = acc * 0x100000001b3UL ^ (unsigned long)middle_pointer((long)(acc & 0xffffff));
    }
    return (long)acc;
}
"#;

const ASM_SOURCE: &str = r#"
    .text
    .globl  snap_forced
    .type   snap_forced, @function
snap_forced:
    pushq   %rbx
    pushq   %r12
    pushq   %r13
    pushq   %r14
    movq    %rdi, %rbx
    leaq    7(%rdi,%rdi,2), %r12
    movq    %rdi, %r13
    xorq    $0x1234, %r13
    movq    %rdi, %r14
    notq    %r14
    subq    $8, %rsp
    movq    %r13, (%rsp)
    movq    %r12, %rdi
    call    g
    addq    (%rsp), %rax
    addq    $8, %rsp
    xorq    %rbx, %rax
    addq    %r12, %rax
    xorq    %r13, %rax
    addq    %r14, %rax
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %rbx
    retq
    .size   snap_forced, .-snap_forced
"#;

const SEED: i64 = 0x00c0_ffee;
const REPLY_PARENT: i64 = 0x5ca1_ab1e;
/// Fork returns zero in the child; the child's identity is its reply.
const REPLY_CHILD: i64 = 0;

/// One entry point and where its fork happens: on which of its `g` calls the
/// snapshot is taken, and how many `g` calls the resumed child itself makes.
struct Scenario {
    entry: &'static str,
    fork_at: u32,
    child_calls: u32,
}

const C_SCENARIOS: [Scenario; 3] = [
    Scenario {
        entry: "snap_entry",
        fork_at: 1,
        child_calls: 0,
    },
    Scenario {
        entry: "tail_entry",
        fork_at: 1,
        child_calls: 0,
    },
    Scenario {
        entry: "loop_entry",
        fork_at: 3,
        child_calls: 5,
    },
];

const ASM_SCENARIO: Scenario = Scenario {
    entry: "snap_forced",
    fork_at: 1,
    child_calls: 0,
};

struct Snapshot {
    memory: Vec<u8>,
    globals: Vec<(String, wasmtime::Val)>,
}

/// What `g` does in one run, and what it saw.
struct RunState {
    calls: u32,
    /// Take the snapshot when the call count reaches this.
    snapshot_on: Option<u32>,
    /// Answer `REPLY_CHILD` from this call number on; `REPLY_PARENT` before.
    child_from: Option<u32>,
}

fn exported_global(caller: &mut wasmtime::Caller<'_, RunState>, name: &str) -> wasmtime::Global {
    match caller.get_export(name) {
        Some(wasmtime::Extern::Global(global)) => global,
        _ => panic!("no exported global `{name}`"),
    }
}

fn write_register(caller: &mut wasmtime::Caller<'_, RunState>, name: &str, value: i64) {
    let global = exported_global(caller, name);
    global
        .set(&mut *caller, wasmtime::Val::I64(value))
        .unwrap_or_else(|error| panic!("setting `{name}`: {error}"));
}

fn read_register(caller: &mut wasmtime::Caller<'_, RunState>, name: &str) -> i64 {
    let global = exported_global(caller, name);
    match global.get(&mut *caller) {
        wasmtime::Val::I64(value) => value,
        other => panic!("`{name}` is not an i64 global: {other:?}"),
    }
}

/// The mutable globals a module exports — everything the fork must carry.
fn mutable_globals(module: &wasmtime::Module) -> Vec<String> {
    let names: Vec<String> = module
        .exports()
        .filter_map(|export| match export.ty() {
            wasmtime::ExternType::Global(global)
                if global.mutability() == wasmtime::Mutability::Var =>
            {
                Some(export.name().to_string())
            }
            _ => None,
        })
        .collect();
    assert!(
        names.iter().any(|name| name == "x86_rsp"),
        "the register file is not exported"
    );
    names
}

fn instantiate(
    engine: &wasmtime::Engine,
    module: &wasmtime::Module,
    state: RunState,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
    global_names: Vec<String>,
) -> (wasmtime::Store<RunState>, wasmtime::Instance) {
    let mut store = wasmtime::Store::new(engine, state);
    let mut linker: wasmtime::Linker<RunState> = wasmtime::Linker::new(engine);
    linker
        .func_wrap(
            "env",
            "g_guest",
            move |mut caller: wasmtime::Caller<'_, RunState>| {
                caller.data_mut().calls += 1;
                let calls = caller.data().calls;

                if caller.data().snapshot_on == Some(calls) {
                    let memory = match caller.get_export("memory") {
                        Some(wasmtime::Extern::Memory(memory)) => memory,
                        _ => panic!("no exported memory"),
                    };
                    let bytes = memory.data(&caller).to_vec();
                    let globals = global_names
                        .iter()
                        .map(|name| {
                            let global = exported_global(&mut caller, name);
                            (name.clone(), global.get(&mut caller))
                        })
                        .collect();
                    *snapshot.lock().unwrap() = Some(Snapshot {
                        memory: bytes,
                        globals,
                    });
                }

                let reply = match caller.data().child_from {
                    Some(from) if calls >= from => REPLY_CHILD,
                    _ => REPLY_PARENT,
                };
                write_register(&mut caller, "x86_rax", reply);
                let stack_pointer = read_register(&mut caller, "x86_rsp");
                write_register(&mut caller, "x86_rsp", stack_pointer + 8);
            },
        )
        .expect("define `g`");
    let instance = linker
        .instantiate(&mut store, module)
        .unwrap_or_else(|error| panic!("instantiation failed: {error}"));
    (store, instance)
}

/// An ordinary full run through the entry wrapper.
fn full_run(
    engine: &wasmtime::Engine,
    module: &wasmtime::Module,
    state: RunState,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
    global_names: Vec<String>,
    entry: &str,
) -> (i64, u32) {
    let (mut store, instance) = instantiate(engine, module, state, snapshot, global_names);
    let wrapper = instance
        .get_typed_func::<(
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
            f64,
        ), (i64, f64)>(&mut store, entry)
        .unwrap_or_else(|error| panic!("export `{entry}` not usable: {error}"));
    let (result, _) = wrapper
        .call(
            &mut store,
            (SEED, 0, 0, 0, 0, 0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
        )
        .unwrap_or_else(|error| panic!("call to `{entry}` trapped: {error}"));
    (result, store.data().calls)
}

/// The child: restore the snapshot into a fresh instance and resume. The
/// entry wrapper is never called; if the result is right, the driver rebuilt
/// the suspended frames from the restored chain alone.
fn fork_run(
    engine: &wasmtime::Engine,
    module: &wasmtime::Module,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
    global_names: Vec<String>,
) -> (i64, u32) {
    let state = RunState {
        calls: 0,
        snapshot_on: None,
        child_from: Some(1),
    };
    let (mut store, instance) = instantiate(engine, module, state, snapshot.clone(), global_names);

    {
        let recorded = snapshot.lock().unwrap();
        let recorded = recorded.as_ref().expect("the record run must come first");
        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("no exported memory");
        let data = memory.data_mut(&mut store);
        assert_eq!(
            data.len(),
            recorded.memory.len(),
            "the two instances' memories differ in size"
        );
        data.copy_from_slice(&recorded.memory);
        for (name, value) in &recorded.globals {
            let global = instance
                .get_global(&mut store, name)
                .unwrap_or_else(|| panic!("no exported global `{name}`"));
            global
                .set(&mut store, value.clone())
                .unwrap_or_else(|error| panic!("restoring `{name}`: {error}"));
        }
    }

    // Fork's answer in the child. The driver performs the pop the suspended
    // callee's return owes, so `rsp` stays as restored.
    let rax = instance
        .get_global(&mut store, "x86_rax")
        .expect("no exported `x86_rax`");
    rax.set(&mut store, wasmtime::Val::I64(REPLY_CHILD))
        .expect("set `x86_rax`");

    let driver = instance
        .get_typed_func::<(), ()>(&mut store, "x86_resume")
        .unwrap_or_else(|error| panic!("export `x86_resume` not usable: {error}"));
    driver
        .call(&mut store, ())
        .unwrap_or_else(|error| panic!("resume trapped: {error}"));

    let result = match rax.get(&mut store) {
        wasmtime::Val::I64(value) => value,
        other => panic!("`x86_rax` is not an i64 global: {other:?}"),
    };
    (result, store.data().calls)
}

fn fork(linked: &Path, plain_linked: &Path, scenario: &Scenario, variant: &str) {
    let engine = wasmtime::Engine::default();
    let bytes = std::fs::read(linked).expect("read linked module");
    let module = wasmtime::Module::new(&engine, &bytes)
        .unwrap_or_else(|error| panic!("wasmtime rejected the linked module: {error}"));
    let names = mutable_globals(&module);
    let snapshot = Arc::new(Mutex::new(None));

    let (parent_result, parent_calls) = full_run(
        &engine,
        &module,
        RunState {
            calls: 0,
            snapshot_on: Some(scenario.fork_at),
            child_from: None,
        },
        snapshot.clone(),
        names.clone(),
        scenario.entry,
    );
    assert!(
        parent_calls >= scenario.fork_at,
        "{variant}/{}: the fork call never happened",
        scenario.entry
    );

    // The machinery must be inert in ordinary running: the same build
    // without `--resume` computes the same thing. Without this, a
    // miscompile that shifted every run equally could hide inside the
    // parent/oracle self-consistency.
    let plain_bytes = std::fs::read(plain_linked).expect("read plain linked module");
    let plain_module = wasmtime::Module::new(&engine, &plain_bytes)
        .unwrap_or_else(|error| panic!("wasmtime rejected the plain module: {error}"));
    let plain_names = mutable_globals(&plain_module);
    let (plain_result, plain_calls) = full_run(
        &engine,
        &plain_module,
        RunState {
            calls: 0,
            snapshot_on: None,
            child_from: None,
        },
        Arc::new(Mutex::new(None)),
        plain_names,
        scenario.entry,
    );
    assert_eq!(
        (plain_result, plain_calls),
        (parent_result, parent_calls),
        "{variant}/{}: the resume machinery changed ordinary execution",
        scenario.entry
    );

    let (expected_child, oracle_calls) = full_run(
        &engine,
        &module,
        RunState {
            calls: 0,
            snapshot_on: None,
            child_from: Some(scenario.fork_at),
        },
        snapshot.clone(),
        names.clone(),
        scenario.entry,
    );
    assert_eq!(
        oracle_calls, parent_calls,
        "{variant}/{}: the oracle's call count drifted from the parent's",
        scenario.entry
    );
    assert_ne!(
        expected_child, parent_result,
        "{variant}/{}: the two replies agree at the end; the fork is vacuous",
        scenario.entry
    );

    let (child_result, child_calls) = fork_run(&engine, &module, snapshot, names);
    assert_eq!(
        child_calls, scenario.child_calls,
        "{variant}/{}: the child's `g` calls are not its own fresh ones — \
         the prefix re-ran, or the continuation lost calls",
        scenario.entry
    );
    assert_eq!(
        child_result, expected_child,
        "{variant}/{}: the resumed child diverged from the child oracle",
        scenario.entry
    );
}

fn build_and_fork(
    workspace: &WorkingDirectory,
    source: &Path,
    scenarios: &[&Scenario],
    compiler: Compiler,
    model: CodeModel,
    optimisation: &str,
    mode: zaqaru::structurer::Mode,
) {
    let variant = format!(
        "{}/{}/{}{optimisation}/{mode:?}",
        source.file_name().unwrap().to_string_lossy(),
        compiler.label(),
        model.label()
    );
    let flat = variant.replace('/', ".");

    let is_assembly = source.extension().is_some_and(|extension| extension == "s");
    let object = workspace.path().join(format!("{flat}.o"));
    let source_text = source.to_string_lossy().into_owned();
    let object_text = object.to_string_lossy().into_owned();
    let mut arguments: Vec<&str> = if is_assembly {
        Vec::new()
    } else {
        let mut flags = CORPUS_COMPILE_FLAGS.to_vec();
        flags.push(optimisation);
        flags.push(model.flag());
        flags
    };
    arguments.extend(["-c", &source_text, "-o", &object_text]);
    run_tool(compiler.program(), &arguments);

    let wasm_object = workspace.path().join(format!("{flat}.wasm.o"));
    transpile_object_resumable(&object, &wasm_object, mode);
    let plain_wasm_object = workspace.path().join(format!("{flat}.plain.wasm.o"));
    support::transpile_object(&object, &plain_wasm_object, mode);

    let linked = workspace.path().join(format!("{flat}.wasm"));
    link_wasm(
        &[wasm_object],
        &linked,
        &["--export-all", "--import-undefined"],
    );
    let plain_linked = workspace.path().join(format!("{flat}.plain.wasm"));
    link_wasm(
        &[plain_wasm_object],
        &plain_linked,
        &["--export-all", "--import-undefined"],
    );

    for scenario in scenarios {
        fork(&linked, &plain_linked, scenario, &variant);
    }
}

#[test]
fn a_restored_checkpoint_resumes_without_reexecution() {
    let workspace = WorkingDirectory::new("fork-resume");
    let c_source = workspace.write("fork_guest.c", C_SOURCE);
    let asm_source = workspace.write("fork_forced.s", ASM_SOURCE);
    let c_scenarios: Vec<&Scenario> = C_SCENARIOS.iter().collect();

    for compiler in ALL_COMPILERS {
        for model in [CodeModel::PositionIndependent, CodeModel::Absolute] {
            for optimisation in ALL_OPTIMISATION_LEVELS {
                for mode in ALL_MODES {
                    build_and_fork(
                        &workspace,
                        &c_source,
                        &c_scenarios,
                        compiler,
                        model,
                        optimisation,
                        mode,
                    );
                }
            }
        }
        // The assembly ignores optimisation and code-model flags, so one
        // build per compiler and control-flow mode covers it.
        for mode in ALL_MODES {
            build_and_fork(
                &workspace,
                &asm_source,
                &[&ASM_SCENARIO],
                compiler,
                CodeModel::PositionIndependent,
                "-O0",
                mode,
            );
        }
    }
}
