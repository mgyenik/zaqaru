//! Milestone 1 of [the interop plan](../docs/archive/interop-plan.md): the questions
//! the rest of that plan leans on, settled before anything is built on them.
//!
//! 1. **Does `wasm-ld` diagnose a function-signature mismatch, and how
//!    loudly?** The plan makes inference — not DWARF — the primary source of
//!    boundary signatures, and the argument for why that is safe is that a
//!    wrong signature at a seam fails loudly instead of corrupting silently.
//!    That argument is a claim about a tool, so it gets measured.
//! 2. **Do the two conventions actually meet, one call each way?** A foreign
//!    wasm object calling a transpiled export through a typed wrapper, and a
//!    transpiled guest calling a foreign wasm function through a `_guest`
//!    thunk — the two mechanisms milestone 2 generates, here hand-built, so
//!    that the marshalling is settled before any code generates it.
//!
//! The second spike is where `__stack_pointer` earns its own test. The guest
//! stack and clang's shadow stack are the same region of the same linear
//! memory, both growing down from the same place, so a foreign call made
//! without moving the linker's stack pointer allocates its frame on top of
//! live guest data. That is invisible in any test whose foreign callee is
//! small enough to reach nothing, so the callee here is deliberately not, and
//! the unsynced version is built and run alongside the correct one to prove
//! the difference is observable.

mod support;

use std::path::PathBuf;

use support::{
    LinkedModule, WorkingDirectory, compile_foreign_wasm_object, link_wasm, try_link_wasm,
    validate_wasm,
};
use zaqaru::emitter::code::{FunctionBodyBuilder, FunctionReference, GlobalReference};
use zaqaru::emitter::linking::{Symbol, SymbolTarget, symbol_flags};
use zaqaru::emitter::{
    DefinedFunction, DefinedGlobal, ENVIRONMENT_MODULE, FunctionType, ImportedFunction,
    ImportedGlobal, STACK_POINTER_IMPORT, ValueType, WasmObject,
};
use zaqaru::machine::RETURN_ADDRESS_SENTINEL;

/// A value parked on the guest stack across a foreign call. Any bit pattern
/// would do; this one is recognisable in a failure message.
const GUEST_STACK_MARKER: i64 = 0x0bad_c0de_dead_beefu64 as i64;

/// Enough shadow stack, in the foreign callee, to reach a guest frame if the
/// linker's stack pointer is not moved out of the way first.
const FOREIGN_SHADOW_STACK_WORDS: usize = 64;

// ---------------------------------------------------------------------------
// A hand-built stand-in for a transpiled object.
//
// Only as much machine state as these spikes touch, in the same shape the
// real machine model uses: weak mutable globals for the registers, an
// imported `__stack_pointer` for the linker's, and guest functions with wasm
// type `() -> ()`.
// ---------------------------------------------------------------------------

struct HandBuiltObject {
    object: WasmObject,
    stack_pointer: GlobalReference,
    rdi: GlobalReference,
    rax: GlobalReference,
    rsp: GlobalReference,
    guest_type: u32,
}

impl HandBuiltObject {
    fn new() -> Self {
        let mut object = WasmObject::new();

        // Imported globals precede defined ones in the index space, so the
        // linker's stack pointer has to be declared first — same ordering
        // constraint the real machine model observes.
        let stack_pointer_index = object.next_defined_global_index();
        object.imported_globals.push(ImportedGlobal {
            module: ENVIRONMENT_MODULE.to_string(),
            field: STACK_POINTER_IMPORT.to_string(),
            value_type: ValueType::I32,
            mutable: true,
        });
        let stack_pointer_symbol = object.add_symbol(Symbol {
            name: STACK_POINTER_IMPORT.to_string(),
            target: SymbolTarget::Global(stack_pointer_index),
            flags: symbol_flags::UNDEFINED,
        });
        let stack_pointer = GlobalReference {
            symbol_index: stack_pointer_symbol,
            global_index: stack_pointer_index,
        };

        let define = |object: &mut WasmObject, name: &str| {
            let global_index = object.next_defined_global_index();
            object.defined_globals.push(DefinedGlobal {
                value_type: ValueType::I64,
                mutable: true,
                initial_value: 0,
            });
            let symbol_index = object.add_symbol(Symbol {
                name: name.to_string(),
                target: SymbolTarget::Global(global_index),
                flags: symbol_flags::WEAK,
            });
            GlobalReference {
                symbol_index,
                global_index,
            }
        };
        let rdi = define(&mut object, "x86_rdi");
        let rax = define(&mut object, "x86_rax");
        let rsp = define(&mut object, "x86_rsp");

        let guest_type = object.intern_type(FunctionType {
            parameters: vec![],
            results: vec![],
        });

        Self {
            object,
            stack_pointer,
            rdi,
            rax,
            rsp,
            guest_type,
        }
    }

    /// Declares an import of a *foreign* function under its own clean name
    /// and real wasm type — the typed call an outgoing thunk makes.
    fn import_foreign(&mut self, name: &str, function_type: FunctionType) -> FunctionReference {
        let type_index = self.object.intern_type(function_type);
        let function_index = self.object.imported_functions.len() as u32;
        self.object.imported_functions.push(ImportedFunction {
            module: ENVIRONMENT_MODULE.to_string(),
            field: name.to_string(),
            type_index,
        });
        let symbol_index = self.object.add_symbol(Symbol {
            name: name.to_string(),
            target: SymbolTarget::Function(function_index),
            flags: symbol_flags::UNDEFINED,
        });
        FunctionReference {
            symbol_index,
            function_index,
        }
    }

    fn define_function(
        &mut self,
        name: &str,
        type_index: u32,
        body: zaqaru::emitter::code::FunctionBody,
        flags: u32,
    ) -> FunctionReference {
        let function_index = self.object.next_defined_function_index();
        self.object
            .defined_functions
            .push(DefinedFunction { type_index, body });
        let symbol_index = self.object.add_symbol(Symbol {
            name: name.to_string(),
            target: SymbolTarget::Function(function_index),
            flags,
        });
        FunctionReference {
            symbol_index,
            function_index,
        }
    }

    fn define_guest(
        &mut self,
        name: &str,
        body: zaqaru::emitter::code::FunctionBody,
    ) -> FunctionReference {
        // Guest-convention functions are hidden: the clean name belongs to the
        // wrapper that carries a usable type.
        self.define_function(name, self.guest_type, body, symbol_flags::HIDDEN)
    }

    fn define_export(
        &mut self,
        name: &str,
        function_type: FunctionType,
        body: zaqaru::emitter::code::FunctionBody,
    ) -> FunctionReference {
        let type_index = self.object.intern_type(function_type);
        self.define_function(name, type_index, body, symbol_flags::EXPORTED)
    }

    fn finish(self) -> Vec<u8> {
        let bytes = self.object.serialize();
        validate_wasm(&bytes);
        bytes
    }
}

/// Starts the guest stack from the linker's stack pointer, exactly as the
/// real host-entry wrapper does: align down to 16, then make room for the
/// return-address slot SysV expects to have been pushed.
fn begin_guest_stack(body: &mut FunctionBodyBuilder, machine: &HandBuiltObject) {
    body.global_get(machine.stack_pointer);
    body.i64_extend_i32_unsigned();
    body.i64_const(-16);
    body.i64_and();
    body.i64_const(8);
    body.i64_sub();
    body.global_set(machine.rsp);

    body.global_get(machine.rsp);
    body.i32_wrap_i64();
    body.i64_const(RETURN_ADDRESS_SENTINEL);
    body.i64_store(3, 0);
}

/// Adjusts the guest stack pointer by a constant.
fn move_guest_stack(body: &mut FunctionBodyBuilder, machine: &HandBuiltObject, delta: i64) {
    body.global_get(machine.rsp);
    body.i64_const(delta);
    body.i64_add();
    body.global_set(machine.rsp);
}

// ---------------------------------------------------------------------------
// Spike 1: the linker as a type checker.
// ---------------------------------------------------------------------------

/// Two foreign objects that disagree about one function's type. Using clang
/// for both sides keeps the question about the *linker*: if the disagreement
/// were expressed through a hand-built object, a failure could always be
/// blamed on the object.
fn build_mismatched_pair(workspace: &WorkingDirectory) -> (PathBuf, PathBuf) {
    let definition = compile_foreign_wasm_object(
        workspace,
        "mismatch_definition",
        "int shared(int value) { return value + 1; }\n",
    );
    let use_site = compile_foreign_wasm_object(
        workspace,
        "mismatch_use",
        "long long shared(long long value);\n\
         long long call_shared(long long value) { return shared(value); }\n",
    );
    (definition, use_site)
}

/// The spike the plan schedules first, and the kill-point for its safety
/// story: a boundary signature that is simply wrong must not link into a
/// working call.
///
/// The measured answer is better than the plan assumed. `wasm-ld` reports a
/// mismatch as a *warning* by default, so the recipe promotes it with
/// `--fatal-warnings` — but even unpromoted the linker refuses to connect the
/// call, routing it through a generated stub whose body is `unreachable`. A
/// mis-inferred signature therefore fails twice over: at link time if the
/// warning is read, and at the call itself if it is not. Neither failure is
/// silent corruption, which is the property the inference-first design rests
/// on.
#[test]
fn a_signature_mismatch_is_diagnosed_and_never_silently_linked() {
    let workspace = WorkingDirectory::new("signature-mismatch");
    let (definition, use_site) = build_mismatched_pair(&workspace);
    let objects = [definition, use_site];

    let permissive = workspace.path().join("permissive.wasm");
    let outcome = try_link_wasm(&objects, &permissive, &["--export-all"]);
    assert!(
        outcome.mentions("signature mismatch"),
        "wasm-ld linked disagreeing signatures without saying so. The whole \
         inference-first design rests on this being loud.\n{}",
        outcome.report()
    );

    let strict = workspace.path().join("strict.wasm");
    let promoted = try_link_wasm(&objects, &strict, &["--export-all", "--fatal-warnings"]);
    assert!(
        !promoted.succeeded,
        "`--fatal-warnings` did not turn a signature mismatch into a link \
         failure, so the documented link recipe cannot make one fatal.\n{}",
        promoted.report()
    );

    // And the belt to that braces: whatever the linker said, what it built
    // must not quietly call across the mismatch.
    let mut module = LinkedModule::instantiate(&permissive);
    let result: Result<i64, wasmtime::Error> = module.try_call("call_shared", (7i64,));
    assert!(
        result.is_err(),
        "a call across a signature mismatch ran and returned {:?} instead of \
         trapping; the mismatch was linked into working code after all",
        result.ok()
    );
}

// ---------------------------------------------------------------------------
// Spike 2a: incoming — foreign wasm calls a transpiled export.
// ---------------------------------------------------------------------------

/// A transpiled-style object exporting `add` with a *typed* face — the shape
/// milestone 2 generates once a signature is known — over a guest function
/// that speaks only the emulated convention.
fn build_typed_export_object() -> Vec<u8> {
    let mut machine = HandBuiltObject::new();

    // The guest: `rax = rdi + rdi`, in registers, knowing nothing about wasm
    // types. Doubling rather than adding two arguments keeps the spike to one
    // argument register while still proving the value arrived.
    let mut guest = FunctionBodyBuilder::new(0);
    guest.global_get(machine.rdi);
    guest.global_get(machine.rdi);
    guest.i64_add();
    guest.global_set(machine.rax);
    let guest = machine.define_guest("add_guest", guest.finish());

    // The typed wrapper, carrying the clean name: an ordinary
    // `(i32) -> i32` as far as any other wasm module can tell.
    let mut wrapper = FunctionBodyBuilder::new(1);
    wrapper.local_get(0);
    wrapper.i64_extend_i32_unsigned();
    wrapper.global_set(machine.rdi);
    begin_guest_stack(&mut wrapper, &machine);
    wrapper.call(guest);
    wrapper.global_get(machine.rax);
    wrapper.i32_wrap_i64();

    machine.define_export(
        "add",
        FunctionType {
            parameters: vec![ValueType::I32],
            results: vec![ValueType::I32],
        },
        wrapper.finish(),
    );
    machine.finish()
}

/// Incoming direction: a clang-compiled wasm object calls a transpiled export
/// as if it were any other wasm function, because through the typed wrapper
/// it is.
#[test]
fn foreign_wasm_calls_a_transpiled_export_through_a_typed_wrapper() {
    let workspace = WorkingDirectory::new("typed-export");

    let transpiled = workspace.write("transpiled.wasm.o", build_typed_export_object());
    let foreign = compile_foreign_wasm_object(
        &workspace,
        "caller",
        "int add(int value);\n\
         int use_add(int value) { return add(value) + 1; }\n",
    );

    let linked = workspace.path().join("linked.wasm");
    // `--fatal-warnings` is part of the recipe, not decoration: it is what
    // makes a boundary type disagreement stop the build. The explicit export
    // is only how clang objects say what to expose — ours carry that in their
    // linking metadata already.
    link_wasm(
        &[transpiled, foreign],
        &linked,
        &["--fatal-warnings", "--export=use_add"],
    );
    validate_wasm(&std::fs::read(&linked).expect("read linked module"));

    let mut module = LinkedModule::instantiate(&linked);
    assert_eq!(
        module.call::<(i32,), i32>("use_add", (20,)),
        41,
        "foreign wasm did not get the right answer out of a transpiled export"
    );
}

// ---------------------------------------------------------------------------
// Spike 2b: outgoing — a transpiled guest calls foreign wasm.
// ---------------------------------------------------------------------------

/// Whether the outgoing thunk hands the linker's stack pointer over to the
/// foreign callee before calling it.
///
/// The unsynced variant is not dead weight: it is how this spike proves it
/// can see the bug it exists to rule out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StackPointerSync {
    Handed,
    Withheld,
}

/// The object under test: a guest that parks a value on the guest stack,
/// calls a foreign wasm function through a `_guest` thunk, and reads the
/// value back.
fn build_outgoing_thunk_object(sync: StackPointerSync) -> Vec<u8> {
    let mut machine = HandBuiltObject::new();

    let foreign = machine.import_foreign(
        "foreign_sum",
        FunctionType {
            parameters: vec![ValueType::I32],
            results: vec![ValueType::I32],
        },
    );

    // ---- the thunk: `foreign_sum_guest`, wasm type `() -> ()` ----
    //
    // It stands where a translated callee would, so it owes the caller
    // everything that callee's `ret` would have done.
    let mut thunk = FunctionBodyBuilder::new(0);
    let saved_stack_pointer = thunk.declare_local(ValueType::I32);

    thunk.global_get(machine.stack_pointer);
    thunk.local_set(saved_stack_pointer);

    if sync == StackPointerSync::Handed {
        // Hand the foreign callee a stack pointer below every live guest
        // frame. Without this its shadow stack starts wherever the linker
        // left it — which is inside the guest stack, because they are the
        // same region growing down from the same place.
        thunk.global_get(machine.rsp);
        thunk.i32_wrap_i64();
        thunk.i32_const(-16);
        thunk.i32_and();
        thunk.global_set(machine.stack_pointer);
    }

    // Marshal: the argument register in, the result register out.
    thunk.global_get(machine.rdi);
    thunk.i32_wrap_i64();
    thunk.call(foreign);
    thunk.i64_extend_i32_unsigned();
    thunk.global_set(machine.rax);

    thunk.local_get(saved_stack_pointer);
    thunk.global_set(machine.stack_pointer);

    // The caller pushed a return-address slot expecting a `ret` to pop it.
    // There is no `ret` here, so the thunk pops it.
    move_guest_stack(&mut thunk, &machine, 8);
    let thunk = machine.define_guest("foreign_sum_guest", thunk.finish());

    // ---- the guest doing the calling ----
    let mut guest = FunctionBodyBuilder::new(0);
    // A frame holding one live value across the call.
    move_guest_stack(&mut guest, &machine, -16);
    guest.global_get(machine.rsp);
    guest.i32_wrap_i64();
    guest.i64_const(GUEST_STACK_MARKER);
    guest.i64_store(3, 0);

    // A call, spelled the way the translator spells one: push the return
    // address slot, then transfer.
    move_guest_stack(&mut guest, &machine, -8);
    guest.global_get(machine.rsp);
    guest.i32_wrap_i64();
    guest.i64_const(RETURN_ADDRESS_SENTINEL);
    guest.i64_store(3, 0);
    guest.call(thunk);

    // Read the parked value back and fold it into the result, so that a
    // smashed frame cannot pass unnoticed.
    guest.global_get(machine.rax);
    guest.global_get(machine.rsp);
    guest.i32_wrap_i64();
    guest.i64_load(3, 0);
    guest.i64_add();
    guest.global_set(machine.rax);
    move_guest_stack(&mut guest, &machine, 16);
    let guest = machine.define_guest("probe_guest", guest.finish());

    // ---- the typed face, so a test can call it ----
    let mut wrapper = FunctionBodyBuilder::new(1);
    wrapper.local_get(0);
    wrapper.i64_extend_i32_unsigned();
    wrapper.global_set(machine.rdi);
    begin_guest_stack(&mut wrapper, &machine);
    wrapper.call(guest);
    wrapper.global_get(machine.rax);

    machine.define_export(
        "probe",
        FunctionType {
            parameters: vec![ValueType::I32],
            results: vec![ValueType::I64],
        },
        wrapper.finish(),
    );
    machine.finish()
}

/// The foreign callee, written to use enough shadow stack to reach a guest
/// frame. `volatile` is what makes the array real memory rather than
/// registers, which is the whole point of it being here.
fn foreign_shadow_stack_source() -> String {
    format!(
        "int foreign_sum(int value) {{\n\
        \x20   volatile int buffer[{words}];\n\
        \x20   for (int index = 0; index < {words}; index++) buffer[index] = value + index;\n\
        \x20   int total = 0;\n\
        \x20   for (int index = 0; index < {words}; index++) total += buffer[index];\n\
        \x20   return total;\n\
         }}\n",
        words = FOREIGN_SHADOW_STACK_WORDS
    )
}

/// What `foreign_sum` computes, for the assertion to check against.
fn expected_foreign_sum(value: i32) -> i32 {
    let words = FOREIGN_SHADOW_STACK_WORDS as i32;
    words * value + (words * (words - 1)) / 2
}

fn link_outgoing_thunk_module(
    workspace: &WorkingDirectory,
    label: &str,
    sync: StackPointerSync,
) -> LinkedModule {
    let transpiled = workspace.write(
        &format!("{label}.transpiled.wasm.o"),
        build_outgoing_thunk_object(sync),
    );
    let foreign = compile_foreign_wasm_object(
        workspace,
        &format!("{label}_callee"),
        &foreign_shadow_stack_source(),
    );

    let linked = workspace.path().join(format!("{label}.wasm"));
    link_wasm(&[transpiled, foreign], &linked, &["--fatal-warnings"]);
    validate_wasm(&std::fs::read(&linked).expect("read linked module"));
    LinkedModule::instantiate(&linked)
}

/// Outgoing direction: a transpiled guest calls foreign wasm through a
/// `_guest` thunk, and the guest stack survives the trip.
#[test]
fn a_transpiled_guest_calls_foreign_wasm_through_a_thunk() {
    let workspace = WorkingDirectory::new("outgoing-thunk");
    let mut module = link_outgoing_thunk_module(&workspace, "synced", StackPointerSync::Handed);

    let argument = 3;
    let expected = i64::from(expected_foreign_sum(argument)) + GUEST_STACK_MARKER;
    assert_eq!(
        module.call::<(i32,), i64>("probe", (argument,)),
        expected,
        "a guest calling foreign wasm through a thunk either got the wrong \
         answer back or lost the value it had parked on the guest stack"
    );
}

/// The negative control for the test above. A spike that cannot fail proves
/// nothing, and the failure it has to be able to see — a foreign callee
/// allocating its frame on top of live guest data — is invisible unless
/// something is actually parked there to be destroyed.
#[test]
fn withholding_the_stack_pointer_corrupts_the_guest_stack() {
    let workspace = WorkingDirectory::new("outgoing-thunk-unsynced");
    let mut module = link_outgoing_thunk_module(&workspace, "unsynced", StackPointerSync::Withheld);

    let argument = 3;
    let correct = i64::from(expected_foreign_sum(argument)) + GUEST_STACK_MARKER;
    let observed = module.call::<(i32,), i64>("probe", (argument,));
    assert_ne!(
        observed, correct,
        "a thunk that never moved the linker's stack pointer still produced \
         the right answer, so the synced test above is not actually testing \
         the sync. Either the foreign callee stopped using its shadow stack \
         or the guest frame moved out of its reach."
    );
}
