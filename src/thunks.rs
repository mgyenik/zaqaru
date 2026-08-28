//! The outgoing seam: letting translated code call wasm functions that were
//! never translated.
//!
//! A guest call to a symbol no input object defines is emitted as an import
//! of `<name>_guest` with wasm type `() -> ()`, because that is the
//! convention both sides of a transpiled-to-transpiled call speak. When the
//! definition is going to come from somewhere else — a clang-compiled wasm
//! object, a host import, a hand-written module — nothing provides that
//! symbol, and nothing could: a foreign function has a real wasm type and
//! knows nothing about an emulated register file.
//!
//! So this module *generates* the missing definition. For each foreign
//! function it emits a `<name>_guest` that reads the SysV argument registers
//! out of the machine globals, calls the foreign function under its own name
//! and real type, and puts the result back in `rax` or `xmm0`.
//!
//! **Why a separate object.** These thunks are emitted into an object of
//! their own rather than into whichever transpiled object happened to make
//! the call. Two reasons, and the first is a correctness one: a transpiled
//! object that defines `foo` gives the clean name `foo` to *its* host-entry
//! wrapper, so a thunk emitted inline would import `foo` with a foreign
//! signature into a link that also defines `foo` as a wrapper — a collision
//! manufactured out of nothing. Emitting thunks separately, from a view of
//! the whole link set, means a symbol some object in the set defines is
//! never treated as foreign in the first place. The second reason is that
//! caller-side signature inference wants exactly that whole-set view anyway.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};

use crate::abi::{Signature, SignatureTable, marshal};
use crate::emitter::code::{FunctionBody, FunctionBodyBuilder, FunctionReference};
use crate::emitter::linking::{Symbol, SymbolTarget, symbol_flags};
use crate::emitter::{
    DefinedFunction, ENVIRONMENT_MODULE, FunctionType, ImportedFunction, ValueType, WasmObject,
};
use crate::machine::{MachineState, STACK_ALIGNMENT, STACK_POINTER_REGISTER};
use crate::reader::ObjectFile;
use crate::transpile::{GUEST_SUFFIX, Transpiler};

/// The function symbols a link set calls but none of its objects define.
///
/// Being computed over the whole set is the point: a symbol one object leaves
/// undefined and another defines is an ordinary cross-object guest call, not
/// a boundary, and giving it a thunk would break it.
pub fn foreign_functions(objects: &[ObjectFile]) -> Result<Vec<String>> {
    let mut defined = BTreeSet::new();
    let mut referenced = BTreeSet::new();

    for object in objects {
        let transpiler = Transpiler::new(object);
        defined.extend(transpiler.defined_function_names());
        referenced.extend(transpiler.referenced_undefined_functions()?);
    }

    Ok(referenced.difference(&defined).cloned().collect())
}

/// Where a foreign function's signature comes from, in order of authority.
///
/// A declaration wins because it is the documented override — the way to say
/// something no object records, such as the type of a host import. Failing
/// that, a wasm object being linked against *states* the type of everything
/// it defines, which is knowledge rather than inference. Call-site inference
/// is what is left when neither exists, and it is genuinely last: a function
/// whose arguments are all passed straight through leaves no trace of them in
/// the caller at all.
pub fn foreign_signatures(
    declared: &SignatureTable,
    from_wasm: &SignatureTable,
    objects: &[ObjectFile],
) -> Result<SignatureTable> {
    let mut seed = from_wasm.clone();
    for (name, signature) in declared.iter() {
        seed.insert(name.clone(), signature.clone());
    }

    let mut sites = Vec::new();
    for object in objects {
        sites.extend(crate::abi::infer::infer(object, &seed)?.call_sites);
    }
    let inferred = crate::abi::infer::merge_call_sites(&sites);

    let mut signatures = inferred.signatures;
    for (name, signature) in from_wasm.iter() {
        signatures.insert(name.clone(), signature.clone());
    }
    for (name, signature) in declared.iter() {
        signatures.insert(name.clone(), signature.clone());
    }
    Ok(signatures)
}

/// Builds the thunk object for a set of foreign functions.
///
/// Every name needs a signature: a thunk *is* a signature made executable,
/// and there is nothing useful to emit without one. A name with no signature
/// is an error naming the name, because the alternative — quietly emitting
/// nothing — produces an unresolved symbol at link time whose message points
/// at the wrong thing.
pub fn build_thunk_object(names: &[String], signatures: &SignatureTable) -> Result<Vec<u8>> {
    let mut wasm = WasmObject::new();
    let machine = MachineState::define(&mut wasm);
    let guest_type = wasm.intern_type(FunctionType {
        parameters: vec![],
        results: vec![],
    });

    // Function imports occupy the low end of the index space, so every
    // foreign function has to be declared before any thunk takes an index.
    let mut plans = Vec::new();
    for name in names {
        let Some(signature) = signatures.get(name) else {
            bail!(
                "no signature for `{name}`, which is called but defined nowhere \
                 in the link set. Declare it, or link an object that defines it."
            );
        };
        let foreign = declare_foreign_function(&mut wasm, name, signature)
            .with_context(|| format!("declaring foreign function `{name}`"))?;
        plans.push((name, signature, foreign));
    }

    for (name, signature, foreign) in plans {
        let body = build_thunk(&machine, signature, foreign)
            .with_context(|| format!("building the thunk for `{name}`"))?;
        let function_index = wasm.next_defined_function_index();
        wasm.defined_functions.push(DefinedFunction {
            type_index: guest_type,
            body,
        });
        wasm.add_symbol(Symbol {
            name: format!("{name}{GUEST_SUFFIX}"),
            target: SymbolTarget::Function(function_index),
            // Hidden, like every other guest entry point: the thunk is a seam
            // inside the link, not part of the module's public face.
            flags: symbol_flags::HIDDEN,
        });
    }

    Ok(wasm.serialize())
}

/// Imports a foreign function under its own name and real wasm type.
fn declare_foreign_function(
    wasm: &mut WasmObject,
    name: &str,
    signature: &Signature,
) -> Result<FunctionReference> {
    let function_type = FunctionType {
        parameters: signature
            .parameters
            .iter()
            .map(|parameter| parameter.value_type())
            .collect(),
        results: signature
            .result
            .map(|result| vec![result.value_type()])
            .unwrap_or_default(),
    };
    let type_index = wasm.intern_type(function_type);
    let function_index = wasm.imported_functions.len() as u32;
    wasm.imported_functions.push(ImportedFunction {
        module: ENVIRONMENT_MODULE.to_string(),
        field: name.to_string(),
        type_index,
    });
    let symbol_index = wasm.add_symbol(Symbol {
        name: name.to_string(),
        target: SymbolTarget::Function(function_index),
        flags: symbol_flags::UNDEFINED,
    });
    Ok(FunctionReference {
        symbol_index,
        function_index,
    })
}

/// One thunk: the emulated convention on the outside, an ordinary wasm call
/// on the inside.
fn build_thunk(
    machine: &MachineState,
    signature: &Signature,
    foreign: FunctionReference,
) -> Result<FunctionBody> {
    let locations = signature.argument_locations()?;
    let mut body = FunctionBodyBuilder::new(0);
    let saved_stack_pointer = body.declare_local(ValueType::I32);

    // Hand the callee a stack pointer below every live guest frame.
    //
    // The guest stack and the shadow stack a wasm-native callee uses are the
    // same region of the same linear memory, both growing down, and the
    // linker's `__stack_pointer` is where a callee starts from. The guest
    // moves only `x86_rsp`, so without this the callee would allocate its
    // frame from the top of the stack — on top of the frames of every guest
    // function currently running.
    body.global_get(machine.linker_stack_pointer);
    body.local_set(saved_stack_pointer);

    body.global_get(machine.register(STACK_POINTER_REGISTER));
    body.i32_wrap_i64();
    body.i32_const(-STACK_ALIGNMENT);
    body.i32_and();
    body.global_set(machine.linker_stack_pointer);

    for (parameter, location) in signature.parameters.iter().zip(&locations) {
        marshal::load_argument(&mut body, machine, *parameter, *location);
    }
    body.call(foreign);
    if let Some(result) = signature.result {
        marshal::store_result(&mut body, machine, result);
    }

    body.local_get(saved_stack_pointer);
    body.global_set(machine.linker_stack_pointer);

    // A translated `call` pushes a return-address slot and leaves popping it
    // to the callee's `ret`. This thunk stands where that callee would, so it
    // owes the caller that pop.
    body.global_get(machine.register(STACK_POINTER_REGISTER));
    body.i64_const(8);
    body.i64_add();
    body.global_set(machine.register(STACK_POINTER_REGISTER));

    Ok(body.finish())
}
