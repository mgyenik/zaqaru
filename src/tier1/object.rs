//! The compiled blocks as one relocatable object: the functions, their
//! table slots, and the table the engine looks bytes up in.
//!
//! The object imports the three engine helpers by name, which the linker
//! resolves against `libtargum.a` exactly as the AOT tier's thunks resolve
//! kisal's entry points; puts every compiled function into the module's
//! indirect table, because a table index is what the engine calls; and
//! defines `targum_tier1_table`, a data symbol the engine references, whose
//! function fields are table-index relocations the linker fills in. The
//! table is always emitted, empty or not, because the engine's reference to
//! it is unconditional.
//!
//! The table's layout is `targum::tier1`'s: a header of magic, entry count
//! and the offset of the bytes region; entries of hash, length, offset of
//! the bytes, function, padding, sorted by hash then length; then the bytes
//! of every block, for the comparison a hit is confirmed by.

use std::collections::BTreeMap;

use targum::tier1::{TABLE_ENTRY, TABLE_HEADER, TABLE_MAGIC, hash};

use super::compile::{Helpers, check_body, compile};
use super::sweep::Candidate;
use crate::emitter::data::DataSegment;
use crate::emitter::linking::{Relocation, RelocationKind, Symbol, SymbolTarget, symbol_flags};
use crate::emitter::code::FunctionReference;
use crate::emitter::{
    DefinedFunction, ENVIRONMENT_MODULE, FIRST_TABLE_INDEX, FunctionType, ImportedFunction,
    ValueType, WasmObject,
};

/// The table's symbol, which `targum::tier1::lookup` references.
pub const TABLE_SYMBOL: &str = "targum_tier1_table";

/// What a build produced, and what it cost.
pub struct Built {
    pub object: Vec<u8>,
    /// Distinct blocks compiled.
    pub functions: usize,
    /// Candidates the budget stopped short of.
    pub left_out: usize,
    /// Candidates left interpreted because most of their instructions
    /// would have gone through the helper anyway.
    pub mostly_deferred: usize,
    /// Bytes of compiled code, before linking.
    pub code_bytes: usize,
    /// Instructions compiled, and of those, deferred to the helper.
    pub instructions: usize,
    pub deferred: usize,
}

/// Compiles the candidates, in order, until the code exceeds `budget`
/// bytes, and answers the object.
///
/// Identical bytes are one function: the same routine in two libraries,
/// or found twice by the sweep, is compiled once and looked up once.
pub fn build(candidates: &[Candidate], budget: usize) -> Built {
    let mut wasm = WasmObject::new();
    let compiled_type = wasm.intern_type(FunctionType {
        parameters: vec![ValueType::I32, ValueType::I32, ValueType::I64, ValueType::I64],
        results: vec![ValueType::I64],
    });
    let step_type = wasm.intern_type(FunctionType {
        parameters: vec![ValueType::I32, ValueType::I32],
        results: vec![ValueType::I32],
    });
    let code_write_type = wasm.intern_type(FunctionType {
        parameters: vec![ValueType::I64, ValueType::I32],
        results: vec![],
    });
    let import = |wasm: &mut WasmObject, name: &str, type_index: u32| -> FunctionReference {
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
        FunctionReference {
            symbol_index,
            function_index,
        }
    };
    let check_type = wasm.intern_type(FunctionType {
        parameters: vec![ValueType::I32, ValueType::I64, ValueType::I32],
        results: vec![ValueType::I32],
    });
    let step = import(&mut wasm, "targum_step", step_type);
    let condition = import(&mut wasm, "targum_condition", step_type);
    let code_write = import(&mut wasm, "targum_code_write", code_write_type);
    // The two permission checks, defined once and called by every block.
    let define_check = |wasm: &mut WasmObject, name: &str, write: bool| -> FunctionReference {
        let function_index = wasm.next_defined_function_index();
        wasm.defined_functions.push(DefinedFunction {
            type_index: check_type,
            body: check_body(write),
        });
        let symbol_index = wasm.add_symbol(Symbol {
            name: name.to_string(),
            target: SymbolTarget::Function(function_index),
            flags: symbol_flags::LOCAL,
        });
        FunctionReference {
            symbol_index,
            function_index,
        }
    };
    let helpers = Helpers {
        step,
        condition,
        code_write,
        check_read: define_check(&mut wasm, "tier1_check_read", false),
        check_write: define_check(&mut wasm, "tier1_check_write", true),
    };

    // One function per distinct byte sequence, keyed for the table.
    let mut entries: BTreeMap<(u64, Vec<u8>), (u32, u32)> = BTreeMap::new();
    let mut code_bytes = 0usize;
    let mut instructions = 0usize;
    let mut deferred = 0usize;
    let mut left_out = 0usize;
    let mut mostly_deferred = 0usize;
    let mut next_slot = FIRST_TABLE_INDEX;
    for candidate in candidates {
        let key = (hash(&candidate.bytes), candidate.bytes.clone());
        if entries.contains_key(&key) {
            continue;
        }
        if code_bytes >= budget {
            left_out += 1;
            continue;
        }
        let Some(compiled) = compile(candidate, helpers) else {
            continue;
        };
        // A block that is mostly instructions the lowering declines runs
        // mostly through the helper, which is slower than interpreting it
        // and costs a great deal of code: not worth compiling.
        if compiled.deferred * 4 > compiled.instructions {
            mostly_deferred += 1;
            continue;
        }
        code_bytes += compiled.body.bytes.len();
        instructions += compiled.instructions;
        deferred += compiled.deferred;
        let function_index = wasm.next_defined_function_index();
        wasm.defined_functions.push(DefinedFunction {
            type_index: compiled_type,
            body: compiled.body,
        });
        let symbol_index = wasm.add_symbol(Symbol {
            name: format!("tier1_{:016x}_{}", key.0, candidate.bytes.len()),
            target: SymbolTarget::Function(function_index),
            flags: symbol_flags::LOCAL,
        });
        wasm.table_functions.push(function_index);
        wasm.uses_function_table = true;
        let slot = next_slot;
        next_slot += 1;
        entries.insert(key, (symbol_index, slot));
    }
    let functions = entries.len();

    // The table: sorted by (hash, length), which the map's key order gives
    // once the bytes' length is what the second component compares as —
    // it is not, so sort explicitly.
    let mut rows: Vec<(&(u64, Vec<u8>), &(u32, u32))> = entries.iter().collect();
    rows.sort_by(|left, right| {
        (left.0.0, left.0.1.len()).cmp(&(right.0.0, right.0.1.len()))
    });
    let mut bytes = Vec::new();
    let mut relocations = Vec::new();
    let bytes_region = TABLE_HEADER + rows.len() * TABLE_ENTRY;
    bytes.extend_from_slice(&TABLE_MAGIC.to_le_bytes());
    bytes.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(bytes_region as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    let mut blob: Vec<u8> = Vec::new();
    for ((hash, block), (symbol_index, slot)) in &rows {
        bytes.extend_from_slice(&hash.to_le_bytes());
        bytes.extend_from_slice(&(block.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        relocations.push(Relocation {
            kind: RelocationKind::TableIndexI32,
            offset: bytes.len() as u32,
            symbol_index: *symbol_index,
            addend: 0,
        });
        bytes.extend_from_slice(&slot.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        blob.extend_from_slice(block);
    }
    debug_assert_eq!(bytes.len(), bytes_region);
    bytes.extend_from_slice(&blob);
    let size = bytes.len() as u32;
    let segment_index = wasm.data_segments.len() as u32;
    wasm.data_segments.push(DataSegment {
        name: format!(".rodata.{TABLE_SYMBOL}"),
        alignment_log2: 3,
        bytes,
        relocations,
    });
    wasm.add_symbol(Symbol {
        name: TABLE_SYMBOL.to_string(),
        target: SymbolTarget::Data(Some(crate::emitter::linking::DataSymbolLocation {
            segment_index,
            offset: 0,
            size,
        })),
        flags: 0,
    });

    Built {
        object: wasm.serialize(),
        functions,
        left_out,
        mostly_deferred,
        code_bytes,
        instructions,
        deferred,
    }
}

/// An object with no compiled blocks: the table the engine references,
/// empty. What a bake links when tier 1 is off.
pub fn empty() -> Vec<u8> {
    build(&[], 0).object
}
