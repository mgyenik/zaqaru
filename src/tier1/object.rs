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
//! The table's layout is `targum::tier1`'s: a header; entries of hash,
//! length, offset of the bytes, function, member and region, sorted by hash
//! then length; one row per region naming its member rows; one row per
//! member with its delta from the region's base, its length and its bytes'
//! offset; then the bytes of every block, for the comparison a hit is
//! confirmed by and the verification of a region's other members.

use std::collections::BTreeMap;

use targum::tier1::{TABLE_ENTRY, TABLE_HEADER, TABLE_MAGIC, TABLE_MEMBER, TABLE_REGION, hash};

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
    /// Regions compiled, each one function.
    pub functions: usize,
    /// Blocks in them.
    pub members: usize,
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

/// Compiles the candidates — as regions, formed by `super::region` from
/// whatever survives the policy — in order, until the code exceeds
/// `budget` bytes, and answers the object.
///
/// Identical bytes are one entry: the same block in two libraries, or
/// found twice by the sweep, is compiled once and looked up once.
pub fn build(candidates: &[Candidate], budget: usize) -> Built {
    use targum::quick::{Op, Quick};

    let mut wasm = WasmObject::new();
    let compiled_type = wasm.intern_type(FunctionType {
        parameters: vec![
            ValueType::I32,
            ValueType::I32,
            ValueType::I64,
            ValueType::I64,
            ValueType::I32,
        ],
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
    // The two permission checks, defined once and called by every region.
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

    // The policy, per block, before regions are formed: a block that is
    // mostly instructions the lowering declines runs mostly through the
    // helper, which is slower than interpreting it and costs a great deal
    // of code. And each distinct byte sequence once.
    let mut mostly_deferred = 0usize;
    let mut seen: std::collections::HashSet<(u64, usize)> = std::collections::HashSet::new();
    let mut kept: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        if !seen.insert((hash(&candidate.bytes), candidate.bytes.len())) {
            continue;
        }
        let instructions = super::sweep::decode_instructions(candidate);
        let deferred = instructions
            .iter()
            .filter(|instruction| Quick::lower(instruction).op == Op::General)
            .count();
        if deferred * 4 > instructions.len() {
            mostly_deferred += 1;
            continue;
        }
        kept.push(candidate.clone());
    }
    let regions = super::region::form(&kept);

    // One function per region, in order, until the budget; one table
    // entry per member.
    struct Row {
        hash: u64,
        bytes: Vec<u8>,
        symbol_index: u32,
        slot: u32,
        which: u32,
        region: u32,
    }
    let mut rows: Vec<Row> = Vec::new();
    // Per region: its member rows (delta, bytes) in dispatch order.
    let mut region_rows: Vec<Vec<(i32, Vec<u8>)>> = Vec::new();
    let mut code_bytes = 0usize;
    let mut instructions = 0usize;
    let mut deferred = 0usize;
    let mut left_out = 0usize;
    let mut next_slot = FIRST_TABLE_INDEX;
    for region in &regions {
        if code_bytes >= budget {
            left_out += region.members.len();
            continue;
        }
        let Some(compiled) = compile(region, helpers) else {
            continue;
        };
        code_bytes += compiled.body.bytes.len();
        instructions += compiled.instructions;
        deferred += compiled.deferred;
        let function_index = wasm.next_defined_function_index();
        wasm.defined_functions.push(DefinedFunction {
            type_index: compiled_type,
            body: compiled.body,
        });
        let symbol_index = wasm.add_symbol(Symbol {
            name: format!("tier1_region_{:x}", region.base()),
            target: SymbolTarget::Function(function_index),
            flags: symbol_flags::LOCAL,
        });
        wasm.table_functions.push(function_index);
        wasm.uses_function_table = true;
        let slot = next_slot;
        next_slot += 1;
        let region_index = region_rows.len() as u32;
        let base = region.base();
        region_rows.push(
            region
                .members
                .iter()
                .map(|member| ((member.address.wrapping_sub(base)) as i32, member.bytes.clone()))
                .collect(),
        );
        for (which, member) in region.members.iter().enumerate() {
            rows.push(Row {
                hash: hash(&member.bytes),
                bytes: member.bytes.clone(),
                symbol_index,
                slot,
                which: which as u32,
                region: region_index,
            });
        }
    }
    let functions = region_rows.len();
    let members: usize = region_rows.iter().map(Vec::len).sum();

    // The table. Entries sorted by (hash, length); the bytes region holds
    // every member's bytes once, at an offset the entry and the member row
    // both name.
    rows.sort_by(|left, right| (left.hash, left.bytes.len()).cmp(&(right.hash, right.bytes.len())));
    let entries_bytes = rows.len() * TABLE_ENTRY;
    let regions_bytes = region_rows.len() * TABLE_REGION;
    let members_bytes = members * TABLE_MEMBER;
    let regions_at = TABLE_HEADER + entries_bytes;
    let members_at = regions_at + regions_bytes;
    let bytes_at = members_at + members_bytes;
    // Each member's bytes are placed once; the offsets are handed to both
    // the entry and the member row.
    let mut blob: Vec<u8> = Vec::new();
    let mut placed: BTreeMap<(u64, Vec<u8>), u32> = BTreeMap::new();
    let mut place = |bytes: &[u8]| -> u32 {
        let key = (hash(bytes), bytes.to_vec());
        if let Some(at) = placed.get(&key) {
            return *at;
        }
        let at = blob.len() as u32;
        blob.extend_from_slice(bytes);
        placed.insert(key, at);
        at
    };
    let mut bytes = Vec::new();
    let mut relocations = Vec::new();
    bytes.extend_from_slice(&TABLE_MAGIC.to_le_bytes());
    bytes.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(bytes_at as u32).to_le_bytes());
    bytes.extend_from_slice(&(regions_at as u32).to_le_bytes());
    bytes.extend_from_slice(&(members_at as u32).to_le_bytes());
    bytes.extend_from_slice(&(region_rows.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    debug_assert_eq!(bytes.len(), TABLE_HEADER);
    for row in &rows {
        let offset = place(&row.bytes);
        bytes.extend_from_slice(&row.hash.to_le_bytes());
        bytes.extend_from_slice(&(row.bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
        relocations.push(Relocation {
            kind: RelocationKind::TableIndexI32,
            offset: bytes.len() as u32,
            symbol_index: row.symbol_index,
            addend: 0,
        });
        bytes.extend_from_slice(&row.slot.to_le_bytes());
        bytes.extend_from_slice(&row.which.to_le_bytes());
        bytes.extend_from_slice(&row.region.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
    }
    debug_assert_eq!(bytes.len(), regions_at);
    let mut first = 0u32;
    for members in &region_rows {
        bytes.extend_from_slice(&(members.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&first.to_le_bytes());
        first += members.len() as u32;
    }
    debug_assert_eq!(bytes.len(), members_at);
    for members in &region_rows {
        for (delta, member_bytes) in members {
            let offset = place(member_bytes);
            bytes.extend_from_slice(&delta.to_le_bytes());
            bytes.extend_from_slice(&(member_bytes.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&offset.to_le_bytes());
        }
    }
    debug_assert_eq!(bytes.len(), bytes_at);
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
        members,
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
