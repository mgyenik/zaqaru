//! Acceptance tests for the relocatable-object emitter, against stock
//! `wasm-ld`.
//!
//! These are the scheduled kill-points: the questions the design leans on
//! that no amount of reading the specification settles, because what matters
//! is what LLD's object reader actually does.
//!
//! 1. **Does `wasm-ld` deduplicate weakly-defined wasm globals across
//!    objects?** The machine model puts the emulated x86 register file in
//!    globals defined weakly by *every* transpiled object, so that two
//!    separately transpiled objects share one register file after linking.
//!    The fallback if not: a companion machine-state object that defines the
//!    globals while translated objects import them.
//! 2. **Does it accept a function type with more than one result?** The
//!    host-entry wrapper returns both `rax` and `xmm0`, because SysV returns
//!    floats in a register file the integer one cannot express. The fallback
//!    if not: keep a single integer result and read float returns through a
//!    separate exported accessor.
//!
//! The second is the one the earlier `v128` experiment makes worth asking:
//! LLD had no case for a `v128.const` initializer, which is exactly the shape
//! of hole that reading the format would never have revealed.

mod support;

use support::{LinkedModule, WorkingDirectory, link_wasm, validate_wasm};
use zaqaru::emitter::code::{FunctionBodyBuilder, FunctionReference, GlobalReference};
use zaqaru::emitter::linking::{Symbol, SymbolTarget, symbol_flags};
use zaqaru::emitter::{DefinedFunction, DefinedGlobal, FunctionType, ValueType, WasmObject};

/// Adds a weakly-defined mutable `i64` global with the given symbol name,
/// exactly as the machine model will.
fn define_weak_global(object: &mut WasmObject, name: &str) -> GlobalReference {
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
}

fn define_exported_function(
    object: &mut WasmObject,
    name: &str,
    function_type: FunctionType,
    body: zaqaru::emitter::code::FunctionBody,
) -> FunctionReference {
    let type_index = object.intern_type(function_type);
    let function_index = object.next_defined_function_index();
    object
        .defined_functions
        .push(DefinedFunction { type_index, body });
    let symbol_index = object.add_symbol(Symbol {
        name: name.to_string(),
        target: SymbolTarget::Function(function_index),
        flags: symbol_flags::EXPORTED,
    });
    FunctionReference {
        symbol_index,
        function_index,
    }
}

/// `fn(i64)` storing its argument into the shared global.
fn build_writer_object() -> Vec<u8> {
    let mut object = WasmObject::new();
    let shared = define_weak_global(&mut object, "x86_rax");

    let mut body = FunctionBodyBuilder::new(1);
    body.local_get(0);
    body.global_set(shared);

    define_exported_function(
        &mut object,
        "store_shared",
        FunctionType {
            parameters: vec![ValueType::I64],
            results: vec![],
        },
        body.finish(),
    );
    object.serialize()
}

/// `fn() -> i64` reading the shared global back.
fn build_reader_object() -> Vec<u8> {
    let mut object = WasmObject::new();
    let shared = define_weak_global(&mut object, "x86_rax");

    let mut body = FunctionBodyBuilder::new(0);
    body.global_get(shared);

    define_exported_function(
        &mut object,
        "load_shared",
        FunctionType {
            parameters: vec![],
            results: vec![ValueType::I64],
        },
        body.finish(),
    );
    object.serialize()
}

#[test]
fn weakly_defined_globals_are_shared_across_objects() {
    let workspace = WorkingDirectory::new("weak-globals");

    let writer_bytes = build_writer_object();
    let reader_bytes = build_reader_object();
    validate_wasm(&writer_bytes);
    validate_wasm(&reader_bytes);

    let writer = workspace.write("writer.wasm.o", &writer_bytes);
    let reader = workspace.write("reader.wasm.o", &reader_bytes);
    let linked = workspace.path().join("linked.wasm");
    link_wasm(&[writer, reader], &linked, &[]);

    let linked_bytes = std::fs::read(&linked).expect("read linked module");
    validate_wasm(&linked_bytes);

    let mut module = LinkedModule::instantiate(&linked);
    module.call::<(i64,), ()>("store_shared", (0x0123_4567_89ab_cdef,));
    let observed: i64 = module.call::<(), i64>("load_shared", ());

    assert_eq!(
        observed,
        0x0123_4567_89ab_cdef,
        "the two objects did not end up sharing one global; \
         linked module was:\n{}",
        support::print_wasm(&linked_bytes)
    );
}

/// `fn(i64, f64) -> (i64, f64)`, the shape the host-entry wrapper grows into
/// once it has to carry floats: both register files filled on the way in,
/// both result registers handed back on the way out.
fn build_multiple_result_object() -> Vec<u8> {
    let mut object = WasmObject::new();
    let integer = define_weak_global(&mut object, "x86_rax");

    let mut body = FunctionBodyBuilder::new(2);
    // Park the integer argument in the shared global and read it straight
    // back, so the result really does come through the machine state rather
    // than straight off the parameter.
    body.local_get(0);
    body.global_set(integer);
    body.global_get(integer);
    body.local_get(1);
    body.local_get(1);
    body.f64_mul();

    define_exported_function(
        &mut object,
        "both_files",
        FunctionType {
            parameters: vec![ValueType::I64, ValueType::F64],
            results: vec![ValueType::I64, ValueType::F64],
        },
        body.finish(),
    );
    object.serialize()
}

/// The multiple-result spike the float plan schedules before any wrapper code
/// is written: stock `wasm-ld` must link an object whose function type has
/// two results, and wasmtime must call it.
#[test]
fn a_function_type_with_two_results_links_and_runs() {
    let workspace = WorkingDirectory::new("multiple-results");

    let bytes = build_multiple_result_object();
    validate_wasm(&bytes);

    let object = workspace.write("pair.wasm.o", &bytes);
    let linked = workspace.path().join("linked.wasm");
    link_wasm(&[object], &linked, &[]);

    let linked_bytes = std::fs::read(&linked).expect("read linked module");
    validate_wasm(&linked_bytes);

    let mut module = LinkedModule::instantiate(&linked);
    let (integer, float) = module.call::<(i64, f64), (i64, f64)>("both_files", (7, 1.5));
    assert_eq!(
        (integer, float),
        (7, 2.25),
        "a two-result function did not survive the round trip; \
         linked module was:\n{}",
        support::print_wasm(&linked_bytes)
    );
}

/// The single-object case must also stand on its own: a weak definition with
/// no companion is a complete definition.
#[test]
fn a_lone_object_links_and_runs() {
    let workspace = WorkingDirectory::new("lone-object");
    let bytes = build_reader_object();
    validate_wasm(&bytes);

    let object = workspace.write("reader.wasm.o", &bytes);
    let linked = workspace.path().join("linked.wasm");
    link_wasm(&[object], &linked, &[]);

    let mut module = LinkedModule::instantiate(&linked);
    assert_eq!(module.call::<(), i64>("load_shared", ()), 0);
}
