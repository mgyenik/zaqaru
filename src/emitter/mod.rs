//! Emission of relocatable WebAssembly object files.
//!
//! This layer knows nothing about x86 or ELF: it takes an explicit
//! description of a wasm object — types, imports, functions, globals, data,
//! and the linking symbol table — and serializes it in the form stock
//! `wasm-ld` accepts.

pub mod binary;
pub mod code;
pub mod data;
pub mod linking;

use binary::{
    write_custom_section, write_name, write_section, write_signed_leb128, write_unsigned_leb128,
};
use code::FunctionBody;
use data::DataSegment;
use linking::Symbol;

/// The module and field names `wasm-ld` expects for the environment an
/// object links against.
pub const ENVIRONMENT_MODULE: &str = "env";
pub const LINEAR_MEMORY_IMPORT: &str = "__linear_memory";
pub const STACK_POINTER_IMPORT: &str = "__stack_pointer";
pub const INDIRECT_FUNCTION_TABLE_IMPORT: &str = "__indirect_function_table";

/// The linker leaves table slot zero unassigned so that a null function
/// pointer stays null, so our own slots start at one.
pub const FIRST_TABLE_INDEX: u32 = 1;

const WASM_PAGE_SIZE: usize = 65536;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueType {
    I32,
    I64,
    F32,
    F64,
    V128,
}

impl ValueType {
    pub fn encoding(self) -> u8 {
        match self {
            ValueType::I32 => 0x7f,
            ValueType::I64 => 0x7e,
            ValueType::F32 => 0x7d,
            ValueType::F64 => 0x7c,
            ValueType::V128 => 0x7b,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FunctionType {
    pub parameters: Vec<ValueType>,
    pub results: Vec<ValueType>,
}

#[derive(Clone, Debug)]
pub struct ImportedFunction {
    pub module: String,
    pub field: String,
    pub type_index: u32,
}

#[derive(Clone, Debug)]
pub struct ImportedGlobal {
    pub module: String,
    pub field: String,
    pub value_type: ValueType,
    pub mutable: bool,
}

#[derive(Clone, Debug)]
pub struct DefinedGlobal {
    pub value_type: ValueType,
    pub mutable: bool,
    pub initial_value: i64,
}

#[derive(Clone, Debug)]
pub struct DefinedFunction {
    pub type_index: u32,
    pub body: FunctionBody,
}

/// A complete relocatable object, ready to serialize.
#[derive(Default)]
pub struct WasmObject {
    pub types: Vec<FunctionType>,
    pub imported_functions: Vec<ImportedFunction>,
    pub imported_globals: Vec<ImportedGlobal>,
    pub defined_functions: Vec<DefinedFunction>,
    pub defined_globals: Vec<DefinedGlobal>,
    pub data_segments: Vec<DataSegment>,
    pub symbols: Vec<Symbol>,
    /// Functions whose address is taken, in the order they occupy slots of
    /// the indirect function table.
    pub table_functions: Vec<u32>,
    /// Whether the object needs the indirect function table at all. Placing a
    /// function in it is one reason; calling through it is another, and an
    /// object that only receives function pointers from elsewhere does the
    /// second without the first.
    pub uses_function_table: bool,
}

impl WasmObject {
    pub fn new() -> Self {
        Self::default()
    }

    /// Interns a function type, returning its type index.
    pub fn intern_type(&mut self, function_type: FunctionType) -> u32 {
        if let Some(index) = self
            .types
            .iter()
            .position(|existing| *existing == function_type)
        {
            return index as u32;
        }
        self.types.push(function_type);
        (self.types.len() - 1) as u32
    }

    /// Index in the function index space of the next defined function.
    pub fn next_defined_function_index(&self) -> u32 {
        (self.imported_functions.len() + self.defined_functions.len()) as u32
    }

    /// Index in the global index space of the next defined global.
    pub fn next_defined_global_index(&self) -> u32 {
        (self.imported_globals.len() + self.defined_globals.len()) as u32
    }

    pub fn add_symbol(&mut self, symbol: Symbol) -> u32 {
        self.symbols.push(symbol);
        (self.symbols.len() - 1) as u32
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&binary::WASM_MAGIC);
        output.extend_from_slice(&binary::WASM_VERSION);

        // Section ordinals — the index a `reloc.*` section uses to name its
        // target — count every section emitted, custom sections included.
        let mut section_ordinal = 0u32;

        let mut payload = Vec::new();
        write_unsigned_leb128(&mut payload, self.types.len() as u64);
        for function_type in &self.types {
            payload.push(0x60);
            write_unsigned_leb128(&mut payload, function_type.parameters.len() as u64);
            for parameter in &function_type.parameters {
                payload.push(parameter.encoding());
            }
            write_unsigned_leb128(&mut payload, function_type.results.len() as u64);
            for result in &function_type.results {
                payload.push(result.encoding());
            }
        }
        write_section(&mut output, binary::SECTION_TYPE, &payload);
        section_ordinal += 1;

        payload.clear();
        let import_count = 1
            + usize::from(self.uses_function_table)
            + self.imported_functions.len()
            + self.imported_globals.len();
        write_unsigned_leb128(&mut payload, import_count as u64);
        write_name(&mut payload, ENVIRONMENT_MODULE);
        write_name(&mut payload, LINEAR_MEMORY_IMPORT);
        payload.push(0x02); // memory
        payload.push(0x00); // limits: minimum only
        write_unsigned_leb128(&mut payload, self.minimum_memory_pages() as u64);
        if self.uses_function_table {
            write_name(&mut payload, ENVIRONMENT_MODULE);
            write_name(&mut payload, INDIRECT_FUNCTION_TABLE_IMPORT);
            payload.push(0x01); // table
            payload.push(0x70); // funcref
            payload.push(0x00); // limits: minimum only
            write_unsigned_leb128(&mut payload, self.table_functions.len() as u64);
        }
        for function in &self.imported_functions {
            write_name(&mut payload, &function.module);
            write_name(&mut payload, &function.field);
            payload.push(0x00); // function
            write_unsigned_leb128(&mut payload, function.type_index as u64);
        }
        for global in &self.imported_globals {
            write_name(&mut payload, &global.module);
            write_name(&mut payload, &global.field);
            payload.push(0x03); // global
            payload.push(global.value_type.encoding());
            payload.push(u8::from(global.mutable));
        }
        write_section(&mut output, binary::SECTION_IMPORT, &payload);
        section_ordinal += 1;

        payload.clear();
        write_unsigned_leb128(&mut payload, self.defined_functions.len() as u64);
        for function in &self.defined_functions {
            write_unsigned_leb128(&mut payload, function.type_index as u64);
        }
        write_section(&mut output, binary::SECTION_FUNCTION, &payload);
        section_ordinal += 1;

        if !self.defined_globals.is_empty() {
            payload.clear();
            write_unsigned_leb128(&mut payload, self.defined_globals.len() as u64);
            for global in &self.defined_globals {
                payload.push(global.value_type.encoding());
                payload.push(u8::from(global.mutable));
                match global.value_type {
                    ValueType::I32 => {
                        payload.push(0x41);
                        write_signed_leb128(&mut payload, global.initial_value);
                    }
                    ValueType::I64 => {
                        payload.push(0x42);
                        write_signed_leb128(&mut payload, global.initial_value);
                    }
                    // A float global's initial value is its bit pattern, which
                    // is how the transpiler names every floating-point
                    // constant: the guest's own bits, never a decimal literal.
                    ValueType::F32 => {
                        payload.push(0x43);
                        payload.extend_from_slice(&(global.initial_value as u32).to_le_bytes());
                    }
                    ValueType::F64 => {
                        payload.push(0x44);
                        payload.extend_from_slice(&global.initial_value.to_le_bytes());
                    }
                    ValueType::V128 => {
                        // v128.const, the initial value in the low lane.
                        payload.extend_from_slice(&[0xfd, 0x0c]);
                        payload.extend_from_slice(&global.initial_value.to_le_bytes());
                        payload.extend_from_slice(&[0; 8]);
                    }
                }
                payload.push(0x0b); // end
            }
            write_section(&mut output, binary::SECTION_GLOBAL, &payload);
            section_ordinal += 1;
        }

        if !self.table_functions.is_empty() {
            // One active segment placing every address-taken function at its
            // provisional slot. The linker reassigns the slots; the segment
            // exists so that an unlinked object is self-consistent, exactly
            // as clang's is.
            payload.clear();
            write_unsigned_leb128(&mut payload, 1);
            payload.push(0x00); // active, table 0, function indices
            payload.push(0x41); // i32.const
            write_signed_leb128(&mut payload, FIRST_TABLE_INDEX as i64);
            payload.push(0x0b); // end
            write_unsigned_leb128(&mut payload, self.table_functions.len() as u64);
            for function_index in &self.table_functions {
                write_unsigned_leb128(&mut payload, *function_index as u64);
            }
            write_section(&mut output, binary::SECTION_ELEMENT, &payload);
            section_ordinal += 1;
        }

        if !self.data_segments.is_empty() {
            payload.clear();
            write_unsigned_leb128(&mut payload, self.data_segments.len() as u64);
            write_section(&mut output, binary::SECTION_DATA_COUNT, &payload);
            section_ordinal += 1;
        }

        let bodies: Vec<FunctionBody> = self
            .defined_functions
            .iter()
            .map(|function| function.body.clone())
            .collect();
        let (code_payload, code_relocations) = code::write_code_section_payload(&bodies);
        write_section(&mut output, binary::SECTION_CODE, &code_payload);
        let code_section_ordinal = section_ordinal;
        section_ordinal += 1;

        let mut data_relocations = Vec::new();
        let mut data_section_ordinal = 0u32;
        if !self.data_segments.is_empty() {
            let (data_payload, relocations) = data::write_data_section_payload(&self.data_segments);
            write_section(&mut output, binary::SECTION_DATA, &data_payload);
            data_relocations = relocations;
            data_section_ordinal = section_ordinal;
            section_ordinal += 1;
        }

        payload.clear();
        let segment_infos: Vec<_> = self
            .data_segments
            .iter()
            .map(DataSegment::segment_info)
            .collect();
        linking::write_linking_payload(&mut payload, &self.symbols, &segment_infos);
        write_custom_section(&mut output, "linking", &payload);
        section_ordinal += 1;

        if !code_relocations.is_empty() {
            payload.clear();
            linking::write_relocation_payload(
                &mut payload,
                code_section_ordinal,
                &code_relocations,
            );
            write_custom_section(&mut output, "reloc.CODE", &payload);
            section_ordinal += 1;
        }

        if !data_relocations.is_empty() {
            payload.clear();
            linking::write_relocation_payload(
                &mut payload,
                data_section_ordinal,
                &data_relocations,
            );
            write_custom_section(&mut output, "reloc.DATA", &payload);
            section_ordinal += 1;
        }

        payload.clear();
        let mut features = vec!["mutable-globals", "sign-ext"];
        if self.uses_v128() {
            features.push("simd128");
        }
        write_target_features(&mut payload, &features);
        write_custom_section(&mut output, "target_features", &payload);
        let _ = section_ordinal;

        output
    }

    /// Whether anything in the object involves SIMD — the `v128` type in a
    /// signature or global, or SIMD instructions and locals in a body — which
    /// decides whether it declares the `simd128` target feature.
    fn uses_v128(&self) -> bool {
        self.types.iter().any(|function_type| {
            function_type
                .parameters
                .iter()
                .chain(&function_type.results)
                .any(|value_type| *value_type == ValueType::V128)
        }) || self
            .defined_globals
            .iter()
            .map(|global| global.value_type)
            .chain(self.imported_globals.iter().map(|global| global.value_type))
            .any(|value_type| value_type == ValueType::V128)
            || self
                .defined_functions
                .iter()
                .any(|function| function.body.uses_simd)
    }

    fn minimum_memory_pages(&self) -> usize {
        let total: usize = self
            .data_segments
            .iter()
            .map(|segment| segment.bytes.len())
            .sum();
        total.div_ceil(WASM_PAGE_SIZE)
    }
}

fn write_target_features(payload: &mut Vec<u8>, features: &[&str]) {
    write_unsigned_leb128(payload, features.len() as u64);
    for feature in features {
        payload.push(b'+');
        write_name(payload, feature);
    }
}
