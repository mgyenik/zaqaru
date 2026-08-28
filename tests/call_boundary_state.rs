//! Falsifies (or fails to falsify) the claim that makes fork conceivable: at
//! a call boundary, the complete machine state is in the register globals and
//! linear memory, and the wasm call stack carries control — never guest data.
//!
//! The claim rests on the flush discipline: `flush_written` before every
//! call, `reload` after, so no wasm local should hold a guest value across a
//! call. If that is true, a process could be checkpointed at any call
//! boundary by copying memory and globals, which is the state half of fork.
//! This test attacks the claim rather than restating it.
//!
//! The instrument is a state swap at an injected external call. The guest
//! program is branch-free, so two seeds walk the same control path through
//! different data, and it is built so that values computed *before* the call
//! are consumed *after* it — the exact values that would be lost if any of
//! them lived on the wasm stack rather than in the snapshot.
//!
//! Three runs, each in a fresh instance:
//!
//!  - **record** (seed one): at the call to `g` the host snapshots linear
//!    memory and every exported mutable global, then lets the run finish.
//!    Its result is the reference.
//!  - **plain** (seed two): no swap. Must produce a *different* result —
//!    the proof that the scramble is real and the test cannot pass
//!    vacuously.
//!  - **restore** (seed two): at the call to `g` the host replaces memory
//!    and globals wholesale with the recorded snapshot. The continuation
//!    must reproduce the reference exactly. Any guest value that survived
//!    the call in a wasm local carries seed-two data, and the results
//!    diverge.
//!
//! What this deliberately does not test: rebuilding wasm frames without
//! re-execution (the restore run re-walks the poisoned prefix to arrive at
//! the call with the right stack *shape*). That is fork-child construction
//! work; the claim under test here is about the translation that exists.
//!
//! Two stated limits. Flags are not attacked independently: a seed
//! difference that changes flag state without changing a branch outcome is
//! only partially arranged, though the flag *globals* are swapped like every
//! other. And `__stack_pointer` is outside the swap unless the linker
//! exports it — which does not matter here, because the generated thunks
//! derive it from `x86_rsp` on every crossing and nothing else reads it
//! between the swap and the end of the run.

mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};

use support::{
    ALL_COMPILERS, ALL_MODES, ALL_OPTIMISATION_LEVELS, CORPUS_COMPILE_FLAGS, CodeModel, Compiler,
    WorkingDirectory, link_wasm, run_tool, transpile_object,
};

/// The C guest: two frames (`__attribute__((noinline))` keeps `middle` a
/// real call), values of every storage kind live across the call to `g` —
/// integers the compiler will keep in callee-saved registers or spill,
/// a double, and a value threaded from the caller's frame. Branch-free, and
/// free of unspecified behaviour: internal arithmetic is unsigned, and the
/// double arithmetic is exact in binary (`1.0009765625` is `1 + 2^-10`, the
/// operands fit in 27 bits) so the truncating conversion is deterministic.
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
"#;

/// The hand-written guest: compilers choose what to keep live across a call,
/// so this one removes the choice — four callee-saved registers and a stack
/// spill, every one consumed after the call.
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

const SEED_RECORD: i64 = 7;
const SEED_POISON: i64 = 0x00c0_ffee;
/// What `g` answers, the same in every run, so the runs can only differ
/// through state — never through the reply.
const REPLY: i64 = 0x5ca1_ab1e;

/// Everything the fork claim says a process is: linear memory, and the
/// mutable globals.
struct Snapshot {
    memory: Vec<u8>,
    globals: Vec<(String, wasmtime::Val)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Record,
    Plain,
    Restore,
    /// Negative controls, for the hand-written guest only, where which state
    /// is live across the call is forced rather than the compiler's choice:
    /// a swap that withholds one thing known to matter must diverge, or the
    /// harness is not measuring anything.
    MaimRegister,
    MaimMemory,
}

/// Per-store bookkeeping for one run: what `g` should do when called, and
/// what it observed.
struct RunState {
    role: Role,
    calls: u32,
    request: i64,
}

fn exported_global(caller: &mut wasmtime::Caller<'_, RunState>, name: &str) -> wasmtime::Global {
    match caller.get_export(name) {
        Some(wasmtime::Extern::Global(global)) => global,
        _ => panic!("no exported global `{name}`"),
    }
}

fn read_register(caller: &mut wasmtime::Caller<'_, RunState>, name: &str) -> i64 {
    let global = exported_global(caller, name);
    match global.get(&mut *caller) {
        wasmtime::Val::I64(value) => value,
        other => panic!("`{name}` is not an i64 global: {other:?}"),
    }
}

fn write_register(caller: &mut wasmtime::Caller<'_, RunState>, name: &str, value: i64) {
    let global = exported_global(caller, name);
    global
        .set(&mut *caller, wasmtime::Val::I64(value))
        .unwrap_or_else(|error| panic!("setting `{name}`: {error}"));
}

fn exported_memory(caller: &mut wasmtime::Caller<'_, RunState>) -> wasmtime::Memory {
    match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(memory)) => memory,
        _ => panic!("no exported memory"),
    }
}

/// The outcome of one run: the guest's result, and what `g` saw.
struct RunOutcome {
    result: i64,
    request: i64,
    calls: u32,
}

/// Instantiates the module with `g` provided by the host and runs `entry`
/// once. `g` reads its argument from `rdi` and answers in `rax` — it stands
/// exactly where a syscall rewritten into an imported call would stand, with
/// the flush discipline guaranteeing the machine state is (claimed to be)
/// entirely in globals and memory at the moment it runs.
fn run(
    engine: &wasmtime::Engine,
    module: &wasmtime::Module,
    role: Role,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
    entry: &str,
    seed: i64,
) -> RunOutcome {
    // The swap covers every mutable global the module exports — registers,
    // flags, XMM halves, and whatever else the link surfaced — so nothing
    // rests on this test knowing the register file's layout.
    let mutable_globals: Vec<String> = module
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
        mutable_globals.iter().any(|name| name == "x86_rsp"),
        "the register file is not exported; the swap has nothing to swap"
    );

    let mut store = wasmtime::Store::new(
        engine,
        RunState {
            role,
            calls: 0,
            request: 0,
        },
    );

    let mut linker: wasmtime::Linker<RunState> = wasmtime::Linker::new(engine);
    let names = mutable_globals.clone();
    linker
        .func_wrap(
            "env",
            "g_guest",
            move |mut caller: wasmtime::Caller<'_, RunState>| {
                caller.data_mut().calls += 1;
                assert_eq!(caller.data().calls, 1, "`g` called more than once");
                let request = read_register(&mut caller, "x86_rdi");
                caller.data_mut().request = request;

                match caller.data().role {
                    Role::Record => {
                        let memory = exported_memory(&mut caller);
                        let bytes = memory.data(&caller).to_vec();
                        let globals = names
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
                    Role::Plain => {}
                    Role::Restore | Role::MaimRegister | Role::MaimMemory => {
                        let role = caller.data().role;
                        let recorded = snapshot.lock().unwrap();
                        let recorded = recorded
                            .as_ref()
                            .expect("the record run must happen before the restore run");
                        if role != Role::MaimMemory {
                            let memory = exported_memory(&mut caller);
                            let data = memory.data_mut(&mut caller);
                            assert_eq!(
                                data.len(),
                                recorded.memory.len(),
                                "the two instances' memories differ in size"
                            );
                            data.copy_from_slice(&recorded.memory);
                        }
                        for (name, value) in &recorded.globals {
                            if role == Role::MaimRegister && name == "x86_rbx" {
                                continue;
                            }
                            let global = exported_global(&mut caller, name);
                            global
                                .set(&mut caller, value.clone())
                                .unwrap_or_else(|error| panic!("restoring `{name}`: {error}"));
                        }
                    }
                }

                // The reply, and the return-address slot the translated call
                // reserved: a translated callee's `ret` would pop it, so a
                // host-provided callee owes the same pop. Both happen after
                // the snapshot or swap, so record and restore stay
                // symmetric.
                write_register(&mut caller, "x86_rax", REPLY);
                let stack_pointer = read_register(&mut caller, "x86_rsp");
                write_register(&mut caller, "x86_rsp", stack_pointer + 8);
            },
        )
        .expect("define `g`");

    let instance = linker
        .instantiate(&mut store, module)
        .unwrap_or_else(|error| panic!("instantiation failed: {error}"));

    // The uniform host-entry wrapper: six integer argument registers and
    // eight floating-point ones in, `rax` and `xmm0` out.
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
            (seed, 0, 0, 0, 0, 0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
        )
        .unwrap_or_else(|error| panic!("call to `{entry}` trapped: {error}"));

    RunOutcome {
        result,
        request: store.data().request,
        calls: store.data().calls,
    }
}

/// The three-run comparison for one linked module, plus the negative
/// controls where the guest makes them provable.
fn attack(linked: &Path, entry: &str, variant: &str, negative_controls: bool) {
    let engine = wasmtime::Engine::default();
    let bytes = std::fs::read(linked).expect("read linked module");
    let module = wasmtime::Module::new(&engine, &bytes)
        .unwrap_or_else(|error| panic!("wasmtime rejected the linked module: {error}"));
    let snapshot = Arc::new(Mutex::new(None));

    let record = run(
        &engine,
        &module,
        Role::Record,
        snapshot.clone(),
        entry,
        SEED_RECORD,
    );
    let plain = run(
        &engine,
        &module,
        Role::Plain,
        snapshot.clone(),
        entry,
        SEED_POISON,
    );
    let restore = run(
        &engine,
        &module,
        Role::Restore,
        snapshot.clone(),
        entry,
        SEED_POISON,
    );

    assert_eq!(
        record.calls, 1,
        "{variant}: `g` never ran in the record run"
    );
    assert_eq!(plain.calls, 1, "{variant}: `g` never ran in the plain run");
    assert_eq!(
        restore.calls, 1,
        "{variant}: `g` never ran in the restore run"
    );

    // The scramble must be real at the moment of the swap and at the end:
    // a test whose two seeds converge before `g` would prove nothing.
    assert_ne!(
        record.request, plain.request,
        "{variant}: the two seeds agreed at the call site; the scramble is vacuous"
    );
    assert_ne!(
        record.result, plain.result,
        "{variant}: the two seeds agreed at the end; the scramble is vacuous"
    );

    // The claim itself: the swapped continuation is run one's continuation.
    assert_eq!(
        restore.result, record.result,
        "{variant}: state survived the call outside memory and globals — \
         the swapped run diverged from the recorded one"
    );

    // Negative controls, on the guest whose live-across-the-call state is
    // forced by hand: a swap that withholds one register the guest provably
    // consumes after the call, and one that withholds the memory holding its
    // spill, must each diverge. If either converges, the positive result
    // above was not evidence.
    if negative_controls {
        let maimed_register = run(
            &engine,
            &module,
            Role::MaimRegister,
            snapshot.clone(),
            entry,
            SEED_POISON,
        );
        assert_ne!(
            maimed_register.result, record.result,
            "{variant}: withholding `x86_rbx` from the swap changed nothing; \
             the instrument cannot detect a lost register"
        );
        let maimed_memory = run(
            &engine,
            &module,
            Role::MaimMemory,
            snapshot.clone(),
            entry,
            SEED_POISON,
        );
        assert_ne!(
            maimed_memory.result, record.result,
            "{variant}: withholding memory from the swap changed nothing; \
             the instrument cannot detect a lost spill"
        );
    }
}

/// Compiles one written source, transpiles it, links it with `g` left as an
/// import and everything exported, and runs the attack.
fn build_and_attack(
    workspace: &WorkingDirectory,
    source: &Path,
    entry: &str,
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
    transpile_object(&object, &wasm_object, mode);

    let linked = workspace.path().join(format!("{flat}.wasm"));
    link_wasm(
        &[wasm_object],
        &linked,
        &["--export-all", "--import-undefined"],
    );

    attack(&linked, entry, &variant, is_assembly);
}

#[test]
fn state_is_complete_at_a_call_boundary() {
    let workspace = WorkingDirectory::new("call-boundary-state");
    let c_source = workspace.write("snap_guest.c", C_SOURCE);
    let asm_source = workspace.write("snap_forced.s", ASM_SOURCE);

    for compiler in ALL_COMPILERS {
        for model in [CodeModel::PositionIndependent, CodeModel::Absolute] {
            for optimisation in ALL_OPTIMISATION_LEVELS {
                for mode in ALL_MODES {
                    build_and_attack(
                        &workspace,
                        &c_source,
                        "snap_entry",
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
            build_and_attack(
                &workspace,
                &asm_source,
                "snap_forced",
                compiler,
                CodeModel::PositionIndependent,
                "-O0",
                mode,
            );
        }
    }
}
