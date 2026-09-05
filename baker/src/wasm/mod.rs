//! A relocatable WebAssembly object that carries data and nothing else.
//!
//! The image becomes two data segments and two symbols, and `wasm-ld` places
//! them beside the kernel's own data like any other object. This is the
//! smallest object the linker will accept: an empty type section, the one
//! memory import every object declares, empty function and code sections,
//! the data, and the LLVM tool-conventions `linking` metadata that names the
//! segments and the symbols.
//!
//! Reference: <https://github.com/WebAssembly/tool-conventions/blob/main/Linking.md>

pub mod binary;

use binary::{write_custom_section, write_name, write_section, write_signed_leb128, write_unsigned_leb128};

/// The module and field names `wasm-ld` expects for the environment an
/// object links against.
pub const ENVIRONMENT_MODULE: &str = "env";
pub const LINEAR_MEMORY_IMPORT: &str = "__linear_memory";

pub const LINKING_METADATA_VERSION: u32 = 2;

const SUBSECTION_SEGMENT_INFO: u8 = 5;
const SUBSECTION_SYMBOL_TABLE: u8 = 8;
const SYMBOL_KIND_DATA: u8 = 1;

const WASM_PAGE_SIZE: usize = 65536;

/// Symbol flags, as defined by the linking format.
pub mod symbol_flags {
    pub const WEAK: u32 = 0x01;
    pub const LOCAL: u32 = 0x02;
    pub const HIDDEN: u32 = 0x04;
    pub const UNDEFINED: u32 = 0x10;
    pub const EXPORTED: u32 = 0x20;
    pub const EXPLICIT_NAME: u32 = 0x40;
    pub const NO_STRIP: u32 = 0x80;
}

/// One data segment.
#[derive(Clone, Debug)]
pub struct DataSegment {
    /// Segment name, carried in the linking section's segment info. The
    /// `.bss` prefix is meaningful: `wasm-ld` recognises it and places the
    /// segment in the zero-initialised region instead of emitting its bytes.
    pub name: String,
    pub alignment_log2: u32,
    pub bytes: Vec<u8>,
}

/// A defined data symbol: a span of one segment.
#[derive(Clone, Debug)]
pub struct DataSymbol {
    pub name: String,
    pub segment_index: u32,
    pub offset: u32,
    pub size: u32,
    pub flags: u32,
}

/// A complete data-only relocatable object, ready to serialize.
#[derive(Default)]
pub struct DataObject {
    pub segments: Vec<DataSegment>,
    pub symbols: Vec<DataSymbol>,
}

impl DataObject {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&binary::WASM_MAGIC);
        output.extend_from_slice(&binary::WASM_VERSION);

        let mut payload = Vec::new();
        // No types.
        write_unsigned_leb128(&mut payload, 0);
        write_section(&mut output, binary::SECTION_TYPE, &payload);

        // The one import: linear memory, sized to hold the data.
        payload.clear();
        write_unsigned_leb128(&mut payload, 1);
        write_name(&mut payload, ENVIRONMENT_MODULE);
        write_name(&mut payload, LINEAR_MEMORY_IMPORT);
        payload.push(0x02); // memory
        payload.push(0x00); // limits: minimum only
        write_unsigned_leb128(&mut payload, self.minimum_memory_pages() as u64);
        write_section(&mut output, binary::SECTION_IMPORT, &payload);

        // No functions.
        payload.clear();
        write_unsigned_leb128(&mut payload, 0);
        write_section(&mut output, binary::SECTION_FUNCTION, &payload);

        if !self.segments.is_empty() {
            payload.clear();
            write_unsigned_leb128(&mut payload, self.segments.len() as u64);
            write_section(&mut output, binary::SECTION_DATA_COUNT, &payload);
        }

        // No bodies.
        payload.clear();
        write_unsigned_leb128(&mut payload, 0);
        write_section(&mut output, binary::SECTION_CODE, &payload);

        if !self.segments.is_empty() {
            payload.clear();
            write_unsigned_leb128(&mut payload, self.segments.len() as u64);
            // The offset expressions carry each segment's running offset
            // within this object, mirroring what LLVM emits; the linker
            // assigns the real addresses.
            let mut running_offset: i64 = 0;
            for segment in &self.segments {
                payload.push(0x00); // active segment, memory index 0
                payload.push(0x41); // i32.const
                write_signed_leb128(&mut payload, running_offset);
                payload.push(0x0b); // end
                write_unsigned_leb128(&mut payload, segment.bytes.len() as u64);
                payload.extend_from_slice(&segment.bytes);
                running_offset += segment.bytes.len() as i64;
            }
            write_section(&mut output, binary::SECTION_DATA, &payload);
        }

        payload.clear();
        self.write_linking_payload(&mut payload);
        write_custom_section(&mut output, "linking", &payload);

        payload.clear();
        let features = ["mutable-globals", "sign-ext"];
        write_unsigned_leb128(&mut payload, features.len() as u64);
        for feature in features {
            payload.push(b'+');
            write_name(&mut payload, feature);
        }
        write_custom_section(&mut output, "target_features", &payload);

        output
    }

    /// The `linking` custom section payload: segment info, then the symbol
    /// table.
    fn write_linking_payload(&self, payload: &mut Vec<u8>) {
        write_unsigned_leb128(payload, LINKING_METADATA_VERSION as u64);

        if !self.segments.is_empty() {
            let mut subsection = Vec::new();
            write_unsigned_leb128(&mut subsection, self.segments.len() as u64);
            for segment in &self.segments {
                write_name(&mut subsection, &segment.name);
                write_unsigned_leb128(&mut subsection, segment.alignment_log2 as u64);
                write_unsigned_leb128(&mut subsection, 0); // flags
            }
            write_subsection(payload, SUBSECTION_SEGMENT_INFO, &subsection);
        }

        let mut subsection = Vec::new();
        write_unsigned_leb128(&mut subsection, self.symbols.len() as u64);
        for symbol in &self.symbols {
            subsection.push(SYMBOL_KIND_DATA);
            write_unsigned_leb128(&mut subsection, symbol.flags as u64);
            // Data symbols always carry their name.
            write_name(&mut subsection, &symbol.name);
            write_unsigned_leb128(&mut subsection, symbol.segment_index as u64);
            write_unsigned_leb128(&mut subsection, symbol.offset as u64);
            write_unsigned_leb128(&mut subsection, symbol.size as u64);
        }
        write_subsection(payload, SUBSECTION_SYMBOL_TABLE, &subsection);
    }

    fn minimum_memory_pages(&self) -> usize {
        let total: usize = self.segments.iter().map(|segment| segment.bytes.len()).sum();
        total.div_ceil(WASM_PAGE_SIZE)
    }
}

fn write_subsection(output: &mut Vec<u8>, subsection_id: u8, payload: &[u8]) {
    output.push(subsection_id);
    write_unsigned_leb128(output, payload.len() as u64);
    output.extend_from_slice(payload);
}
