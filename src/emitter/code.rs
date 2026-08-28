//! Function-body construction: WebAssembly instruction encoding with
//! linker-patchable immediates.
//!
//! Every index a linker may renumber — function indices, global indices — and
//! every linear-memory address is emitted as a fixed-width LEB128 with a
//! matching relocation entry. The value written at emission time is the
//! object-local one, so an unlinked object still validates; `wasm-ld`
//! overwrites it in place.

use super::ValueType;
use super::binary::{
    RELOCATABLE_LEB128_LENGTH, write_relocatable_signed_leb128, write_relocatable_unsigned_leb128,
    write_signed_leb128, write_unsigned_leb128,
};
use super::linking::{Relocation, RelocationKind};

/// A reference to a function by symbol, with the index it has in *this*
/// object's function index space.
#[derive(Clone, Copy, Debug)]
pub struct FunctionReference {
    pub symbol_index: u32,
    pub function_index: u32,
}

/// A reference to a global by symbol, with the index it has in *this*
/// object's global index space.
#[derive(Clone, Copy, Debug)]
pub struct GlobalReference {
    pub symbol_index: u32,
    pub global_index: u32,
}

/// A reference to a linear-memory address: a data symbol plus a byte offset
/// from it.
#[derive(Clone, Copy, Debug)]
pub struct DataReference {
    pub symbol_index: u32,
    pub addend: i32,
}

/// A reference to a function's slot in the indirect function table — what a
/// function pointer's *value* is once translated.
#[derive(Clone, Copy, Debug)]
pub struct TableReference {
    pub symbol_index: u32,
    pub table_index: u32,
}

/// A reference to a tag by symbol, with the index it has in *this* object's
/// tag index space.
#[derive(Clone, Copy, Debug)]
pub struct TagReference {
    pub symbol_index: u32,
    pub tag_index: u32,
}

/// A finished function body: the bytes the code section carries, plus
/// relocations at offsets relative to the start of those bytes.
#[derive(Clone, Debug)]
pub struct FunctionBody {
    pub bytes: Vec<u8>,
    pub relocations: Vec<Relocation>,
    /// Whether the body contains SIMD instructions or `v128` locals, which
    /// obliges the object to declare the `simd128` target feature.
    pub uses_simd: bool,
    /// Whether the body throws or catches, which obliges the object to
    /// declare the `exception-handling` target feature.
    pub uses_exceptions: bool,
}

/// The block type of a `block`/`loop`/`if`. Everything the transpiler emits
/// is empty-typed: all machine state lives in globals and locals, so no
/// values ever cross a block boundary on the operand stack.
const EMPTY_BLOCK_TYPE: u8 = 0x40;

/// Defines one no-immediate instruction method per `name = opcode` pair.
macro_rules! no_immediate_operations {
    ($($name:ident = $opcode:literal),+ $(,)?) => {
        $(
            pub fn $name(&mut self) {
                self.opcode($opcode);
            }
        )+
    };
}

/// The same, for the `0xfd`-prefixed SIMD instructions.
macro_rules! simd_operations {
    ($($name:ident = $opcode:literal),+ $(,)?) => {
        $(
            pub fn $name(&mut self) {
                self.simd_opcode($opcode);
            }
        )+
    };
}

pub struct FunctionBodyBuilder {
    parameter_count: u32,
    local_types: Vec<ValueType>,
    code: Vec<u8>,
    relocations: Vec<Relocation>,
    uses_simd: bool,
    uses_exceptions: bool,
}

impl FunctionBodyBuilder {
    pub fn new(parameter_count: u32) -> Self {
        Self {
            parameter_count,
            local_types: Vec::new(),
            code: Vec::new(),
            relocations: Vec::new(),
            uses_simd: false,
            uses_exceptions: false,
        }
    }

    /// Declares a new local and returns its index in the local index space
    /// (which begins with the function's parameters).
    pub fn declare_local(&mut self, value_type: ValueType) -> u32 {
        let index = self.parameter_count + self.local_types.len() as u32;
        self.local_types.push(value_type);
        index
    }

    /// Finishes the body: prepends the local declarations, appends the
    /// terminating `end`, and rebases relocation offsets onto the body start.
    pub fn finish(mut self) -> FunctionBody {
        self.code.push(0x0b); // end

        let mut bytes = Vec::new();
        let groups = group_consecutive_local_types(&self.local_types);
        write_unsigned_leb128(&mut bytes, groups.len() as u64);
        for (count, value_type) in groups {
            write_unsigned_leb128(&mut bytes, count as u64);
            bytes.push(value_type.encoding());
        }

        let declaration_length = bytes.len() as u32;
        bytes.extend_from_slice(&self.code);
        for relocation in &mut self.relocations {
            relocation.offset += declaration_length;
        }

        FunctionBody {
            bytes,
            relocations: self.relocations,
            uses_simd: self.uses_simd || self.local_types.contains(&ValueType::V128),
            uses_exceptions: self.uses_exceptions,
        }
    }

    fn opcode(&mut self, opcode: u8) {
        self.code.push(opcode);
    }

    fn unsigned_immediate(&mut self, value: u32) {
        write_unsigned_leb128(&mut self.code, value as u64);
    }

    /// Emits a five-byte immediate and records a relocation against it.
    fn relocatable_unsigned_immediate(
        &mut self,
        kind: RelocationKind,
        symbol_index: u32,
        provisional_value: u32,
    ) {
        self.relocations.push(Relocation {
            kind,
            offset: self.code.len() as u32,
            symbol_index,
            addend: 0,
        });
        write_relocatable_unsigned_leb128(&mut self.code, provisional_value);
    }

    // ---- control flow ----------------------------------------------------

    pub fn unreachable(&mut self) {
        self.opcode(0x00);
    }

    pub fn block(&mut self) {
        self.opcode(0x02);
        self.code.push(EMPTY_BLOCK_TYPE);
    }

    pub fn loop_(&mut self) {
        self.opcode(0x03);
        self.code.push(EMPTY_BLOCK_TYPE);
    }

    pub fn if_(&mut self) {
        self.opcode(0x04);
        self.code.push(EMPTY_BLOCK_TYPE);
    }

    pub fn else_(&mut self) {
        self.opcode(0x05);
    }

    pub fn end(&mut self) {
        self.opcode(0x0b);
    }

    pub fn branch(&mut self, depth: u32) {
        self.opcode(0x0c);
        self.unsigned_immediate(depth);
    }

    pub fn branch_table(&mut self, depths: &[u32], default_depth: u32) {
        self.opcode(0x0e);
        self.unsigned_immediate(depths.len() as u32);
        for depth in depths {
            self.unsigned_immediate(*depth);
        }
        self.unsigned_immediate(default_depth);
    }

    pub fn return_(&mut self) {
        self.opcode(0x0f);
    }

    /// `throw <tag>`: raise the tag, taking its payload from the operand
    /// stack. The standardized spelling from the exception-handling
    /// proposal, not the legacy `try`/`catch` pair — M0's gate 4 found
    /// wasmtime accepts only this one without an engine flag.
    pub fn throw(&mut self, tag: TagReference) {
        self.uses_exceptions = true;
        self.opcode(0x08);
        self.relocatable_unsigned_immediate(
            RelocationKind::TagIndexLeb,
            tag.symbol_index,
            tag.tag_index,
        );
    }

    /// `try_table` with one `catch` clause, empty block type: everything in
    /// the enclosed body runs with the tag handled, and a throw of it
    /// branches to `depth` with the tag's payload on the stack.
    ///
    /// Terminated by [`end`](Self::end) like any other block, and it counts
    /// as one level of branch depth, exactly as `block` does.
    pub fn try_table_catch(&mut self, tag: TagReference, depth: u32) {
        self.uses_exceptions = true;
        self.opcode(0x1f);
        self.code.push(EMPTY_BLOCK_TYPE);
        self.unsigned_immediate(1); // one catch clause
        self.code.push(0x00); // `catch`: payload on the stack, no exnref
        self.relocatable_unsigned_immediate(
            RelocationKind::TagIndexLeb,
            tag.symbol_index,
            tag.tag_index,
        );
        self.unsigned_immediate(depth);
    }

    pub fn call(&mut self, function: FunctionReference) {
        self.opcode(0x10);
        self.relocatable_unsigned_immediate(
            RelocationKind::FunctionIndexLeb,
            function.symbol_index,
            function.function_index,
        );
    }

    /// `call_indirect` through table 0, taking the slot number from the stack.
    ///
    /// The type immediate carries a relocation of its own: the linker merges
    /// type sections and renumbers them. There is only ever one type here —
    /// every translated function is `() -> ()` — so the signature agreement
    /// that normally makes indirect calls hard does not arise.
    pub fn call_indirect(&mut self, type_index: u32) {
        self.opcode(0x11);
        // A type index is not a symbol, so the relocation names the type
        // directly; the linking format spells that with symbol index = type
        // index.
        self.relocations.push(Relocation {
            kind: RelocationKind::TypeIndexLeb,
            offset: self.code.len() as u32,
            symbol_index: type_index,
            addend: 0,
        });
        write_relocatable_unsigned_leb128(&mut self.code, type_index);
        self.code.push(0x00); // table 0
    }

    // ---- variables -------------------------------------------------------

    pub fn select(&mut self) {
        self.opcode(0x1b);
    }

    pub fn local_get(&mut self, index: u32) {
        self.opcode(0x20);
        self.unsigned_immediate(index);
    }

    pub fn local_set(&mut self, index: u32) {
        self.opcode(0x21);
        self.unsigned_immediate(index);
    }

    pub fn local_tee(&mut self, index: u32) {
        self.opcode(0x22);
        self.unsigned_immediate(index);
    }

    pub fn global_get(&mut self, global: GlobalReference) {
        self.opcode(0x23);
        self.relocatable_unsigned_immediate(
            RelocationKind::GlobalIndexLeb,
            global.symbol_index,
            global.global_index,
        );
    }

    pub fn global_set(&mut self, global: GlobalReference) {
        self.opcode(0x24);
        self.relocatable_unsigned_immediate(
            RelocationKind::GlobalIndexLeb,
            global.symbol_index,
            global.global_index,
        );
    }

    // ---- memory ----------------------------------------------------------

    fn memory_access(&mut self, opcode: u8, alignment_log2: u32, offset: u32) {
        self.opcode(opcode);
        self.unsigned_immediate(alignment_log2);
        self.unsigned_immediate(offset);
    }

    pub fn i32_load(&mut self, alignment_log2: u32, offset: u32) {
        self.memory_access(0x28, alignment_log2, offset);
    }

    pub fn i64_load(&mut self, alignment_log2: u32, offset: u32) {
        self.memory_access(0x29, alignment_log2, offset);
    }

    pub fn i32_load8_unsigned(&mut self, offset: u32) {
        self.memory_access(0x2d, 0, offset);
    }

    pub fn i32_load16_unsigned(&mut self, offset: u32) {
        self.memory_access(0x2f, 1, offset);
    }

    pub fn i32_store(&mut self, alignment_log2: u32, offset: u32) {
        self.memory_access(0x36, alignment_log2, offset);
    }

    pub fn i64_store(&mut self, alignment_log2: u32, offset: u32) {
        self.memory_access(0x37, alignment_log2, offset);
    }

    pub fn i32_store8(&mut self, offset: u32) {
        self.memory_access(0x3a, 0, offset);
    }

    pub fn i32_store16(&mut self, offset: u32) {
        self.memory_access(0x3b, 1, offset);
    }

    // ---- constants -------------------------------------------------------

    pub fn i32_const(&mut self, value: i32) {
        self.opcode(0x41);
        write_signed_leb128(&mut self.code, value as i64);
    }

    pub fn i64_const(&mut self, value: i64) {
        self.opcode(0x42);
        write_signed_leb128(&mut self.code, value);
    }

    /// A `f32.const` named by its bit pattern rather than by a decimal
    /// literal: every float the transpiler emits is either the guest's own
    /// bits or an exact power-of-two bound, and bits say so unambiguously.
    pub fn f32_const_bits(&mut self, bits: u32) {
        self.opcode(0x43);
        self.code.extend_from_slice(&bits.to_le_bytes());
    }

    pub fn f64_const_bits(&mut self, bits: u64) {
        self.opcode(0x44);
        self.code.extend_from_slice(&bits.to_le_bytes());
    }

    /// `i32.const <indirect function table slot>`, patched by the linker.
    pub fn i32_const_table_index(&mut self, function: TableReference) {
        self.opcode(0x41);
        self.relocations.push(Relocation {
            kind: RelocationKind::TableIndexSleb,
            offset: self.code.len() as u32,
            symbol_index: function.symbol_index,
            addend: 0,
        });
        write_relocatable_signed_leb128(&mut self.code, function.table_index as i32);
    }

    /// `i32.const <address of a data symbol + addend>`, patched by the linker.
    pub fn i32_const_data_address(&mut self, data: DataReference) {
        self.opcode(0x41);
        self.relocations.push(Relocation {
            kind: RelocationKind::MemoryAddressSleb,
            offset: self.code.len() as u32,
            symbol_index: data.symbol_index,
            addend: data.addend,
        });
        write_relocatable_signed_leb128(&mut self.code, data.addend);
    }

    // ---- integer operations ---------------------------------------------
    //
    // A table of the opcodes with no immediate. It is kept complete for the
    // integer instruction set rather than trimmed to today's callers, so that
    // adding a translation never means also looking up an encoding.

    no_immediate_operations! {
        i32_eqz = 0x45, i32_eq = 0x46, i32_ne = 0x47,
        i32_lt_signed = 0x48, i32_lt_unsigned = 0x49,
        i32_gt_signed = 0x4a, i32_gt_unsigned = 0x4b,
        i32_le_signed = 0x4c, i32_le_unsigned = 0x4d,
        i32_ge_signed = 0x4e, i32_ge_unsigned = 0x4f,
        i64_eqz = 0x50, i64_eq = 0x51, i64_ne = 0x52,
        i64_lt_signed = 0x53, i64_lt_unsigned = 0x54,
        i64_gt_signed = 0x55, i64_gt_unsigned = 0x56,
        i64_le_signed = 0x57, i64_le_unsigned = 0x58,
        i64_ge_signed = 0x59, i64_ge_unsigned = 0x5a,
        i32_add = 0x6a, i32_sub = 0x6b, i32_mul = 0x6c,
        i32_div_signed = 0x6d, i32_div_unsigned = 0x6e,
        i32_rem_signed = 0x6f, i32_rem_unsigned = 0x70,
        i32_and = 0x71, i32_or = 0x72, i32_xor = 0x73,
        i32_shl = 0x74, i32_shr_signed = 0x75, i32_shr_unsigned = 0x76,
        i32_rotate_left = 0x77, i32_rotate_right = 0x78,
        i64_add = 0x7c, i64_sub = 0x7d, i64_mul = 0x7e,
        i64_div_signed = 0x7f, i64_div_unsigned = 0x80,
        i64_rem_signed = 0x81, i64_rem_unsigned = 0x82,
        i64_and = 0x83, i64_or = 0x84, i64_xor = 0x85,
        i64_shl = 0x86, i64_shr_signed = 0x87, i64_shr_unsigned = 0x88,
        i64_rotate_left = 0x89, i64_rotate_right = 0x8a,
        i32_wrap_i64 = 0xa7,
        i64_extend_i32_signed = 0xac, i64_extend_i32_unsigned = 0xad,
        i32_popcnt = 0x69, i64_popcnt = 0x7b,
        i32_extend8_signed = 0xc0, i32_extend16_signed = 0xc1,
        i64_extend8_signed = 0xc2, i64_extend16_signed = 0xc3,
        i64_extend32_signed = 0xc4,
    }

    // ---- floating-point operations ---------------------------------------
    //
    // SSE and wasm are both IEEE-754 round-to-nearest-even, so these map one
    // for one — with the exceptions the design names (`min`/`max` and the
    // truncating conversions), which the translator emulates rather than
    // spelling with the wasm instruction of the same name. The wasm `min`,
    // `max` and saturating-conversion opcodes are deliberately absent from
    // this table so that nothing can reach for them by accident.

    no_immediate_operations! {
        f32_eq = 0x5b, f32_ne = 0x5c,
        f32_lt = 0x5d, f32_gt = 0x5e, f32_le = 0x5f, f32_ge = 0x60,
        f64_eq = 0x61, f64_ne = 0x62,
        f64_lt = 0x63, f64_gt = 0x64, f64_le = 0x65, f64_ge = 0x66,
        f32_absolute = 0x8b, f32_negate = 0x8c, f32_truncate = 0x8f,
        f32_sqrt = 0x91,
        f32_add = 0x92, f32_sub = 0x93, f32_mul = 0x94, f32_div = 0x95,
        f64_absolute = 0x99, f64_negate = 0x9a, f64_truncate = 0x9d,
        f64_nearest = 0x9e, f64_sqrt = 0x9f,
        f64_add = 0xa0, f64_sub = 0xa1, f64_mul = 0xa2, f64_div = 0xa3,
        i32_truncate_f32_signed = 0xa8, i32_truncate_f64_signed = 0xaa,
        i64_truncate_f32_signed = 0xae, i64_truncate_f64_signed = 0xb0,
        f32_convert_i32_signed = 0xb2, f32_convert_i64_signed = 0xb4,
        f32_demote_f64 = 0xb6,
        f64_convert_i32_signed = 0xb7, f64_convert_i64_signed = 0xb9,
        f64_promote_f32 = 0xbb,
        i32_reinterpret_f32 = 0xbc, i64_reinterpret_f64 = 0xbd,
        f32_reinterpret_i32 = 0xbe, f64_reinterpret_i64 = 0xbf,
    }

    pub fn f32_load(&mut self, offset: u32) {
        self.memory_access(0x2a, 2, offset);
    }

    pub fn f64_load(&mut self, offset: u32) {
        self.memory_access(0x2b, 3, offset);
    }

    pub fn f32_store(&mut self, offset: u32) {
        self.memory_access(0x38, 2, offset);
    }

    pub fn f64_store(&mut self, offset: u32) {
        self.memory_access(0x39, 3, offset);
    }

    // ---- simd ------------------------------------------------------------
    //
    // Every SIMD instruction is the `0xfd` prefix followed by a LEB128
    // sub-opcode. XMM state is spelled as pairs of `i64` globals — `wasm-ld`
    // cannot link a `v128` global, because LLD's object reader does not
    // parse `v128.const` initializers — so these are the instructions that
    // assemble a `v128` from its halves and take it apart again.

    fn simd_opcode(&mut self, opcode: u32) {
        self.uses_simd = true;
        self.opcode(0xfd);
        self.unsigned_immediate(opcode);
    }

    pub fn v128_load(&mut self, alignment_log2: u32, offset: u32) {
        self.simd_opcode(0);
        self.unsigned_immediate(alignment_log2);
        self.unsigned_immediate(offset);
    }

    pub fn v128_store(&mut self, alignment_log2: u32, offset: u32) {
        self.simd_opcode(11);
        self.unsigned_immediate(alignment_log2);
        self.unsigned_immediate(offset);
    }

    pub fn i64x2_splat(&mut self) {
        self.simd_opcode(18);
    }

    pub fn i64x2_extract_lane(&mut self, lane: u8) {
        self.simd_opcode(29);
        self.code.push(lane);
    }

    pub fn i64x2_replace_lane(&mut self, lane: u8) {
        self.simd_opcode(30);
        self.code.push(lane);
    }

    // The wasm `min`, `max` and saturating-conversion instructions are
    // absent from this table for the same reason their scalar counterparts
    // are: SSE answers differently on ties, NaN and overflow, so those forms
    // are emulated rather than spelled.
    simd_operations! {
        f32x4_equal = 65, f32x4_less = 67, f32x4_greater = 68, f32x4_less_or_equal = 69,
        f64x2_equal = 71, f64x2_less = 73, f64x2_greater = 74, f64x2_less_or_equal = 75,
        v128_not = 77, v128_and = 78, v128_or = 80, v128_xor = 81,
        i8x16_equal = 35, i8x16_greater_signed = 39,
        i16x8_equal = 45, i16x8_greater_signed = 49,
        i32x4_equal = 55, i32x4_greater_signed = 59,
        i64x2_equal = 214, i64x2_greater_signed = 217,
        i8x16_shift_left = 107, i8x16_shift_right_signed = 108,
        i8x16_shift_right_unsigned = 109,
        i8x16_add = 110, i8x16_sub = 113,
        i16x8_shift_left = 139, i16x8_shift_right_signed = 140,
        i16x8_shift_right_unsigned = 141,
        i16x8_add = 142, i16x8_sub = 145, i16x8_mul = 149,
        i32x4_shift_left = 171, i32x4_shift_right_signed = 172,
        i32x4_shift_right_unsigned = 173,
        i32x4_add = 174, i32x4_sub = 177, i32x4_mul = 181,
        i64x2_shift_left = 203, i64x2_shift_right_signed = 204,
        i64x2_shift_right_unsigned = 205,
        i64x2_add = 206, i64x2_sub = 209, i64x2_mul = 213,
        f32x4_sqrt = 227,
        f32x4_add = 228, f32x4_sub = 229, f32x4_mul = 230, f32x4_div = 231,
        f64x2_sqrt = 239,
        f64x2_add = 240, f64x2_sub = 241, f64x2_mul = 242, f64x2_div = 243,
        f32x4_convert_i32x4_signed = 250,
        f64x2_convert_low_i32x4_signed = 254,
    }
}

fn group_consecutive_local_types(types: &[ValueType]) -> Vec<(u32, ValueType)> {
    let mut groups: Vec<(u32, ValueType)> = Vec::new();
    for value_type in types {
        match groups.last_mut() {
            Some((count, existing)) if existing == value_type => *count += 1,
            _ => groups.push((1, *value_type)),
        }
    }
    groups
}

/// Serializes a code section payload and rebases each body's relocations onto
/// offsets relative to that payload, as the `reloc.CODE` section requires.
pub fn write_code_section_payload(bodies: &[FunctionBody]) -> (Vec<u8>, Vec<Relocation>) {
    let mut payload = Vec::new();
    let mut relocations = Vec::new();
    write_unsigned_leb128(&mut payload, bodies.len() as u64);
    for body in bodies {
        write_unsigned_leb128(&mut payload, body.bytes.len() as u64);
        let body_start = payload.len() as u32;
        payload.extend_from_slice(&body.bytes);
        relocations.extend(body.relocations.iter().map(|relocation| Relocation {
            offset: relocation.offset + body_start,
            ..*relocation
        }));
    }
    debug_assert!(relocations.iter().all(|relocation| {
        relocation.offset as usize + RELOCATABLE_LEB128_LENGTH <= payload.len()
    }));
    (payload, relocations)
}
