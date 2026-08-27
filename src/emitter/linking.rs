//! The LLVM tool-conventions linking metadata: the `linking` custom section
//! and the `reloc.*` custom sections that make an emitted module a
//! *relocatable object* stock `wasm-ld` can consume.
//!
//! Reference: <https://github.com/WebAssembly/tool-conventions/blob/main/Linking.md>

use super::binary::{write_name, write_signed_leb128, write_unsigned_leb128};

pub const LINKING_METADATA_VERSION: u32 = 2;

const SUBSECTION_SEGMENT_INFO: u8 = 5;
const SUBSECTION_SYMBOL_TABLE: u8 = 8;

/// Relocation types we emit. The numeric values are fixed by the linking
/// format; the variants are only those the transpiler can produce.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelocationKind {
    /// Function index as a relocatable unsigned LEB (the `call` immediate).
    FunctionIndexLeb = 0,
    /// A function's slot in the indirect function table, as a relocatable
    /// signed LEB — the `i32.const` that produces a function pointer.
    TableIndexSleb = 1,
    /// The same slot as a 4-byte little-endian word, for a function pointer
    /// stored in data.
    TableIndexI32 = 2,
    /// Linear-memory address as a relocatable signed LEB (an `i32.const`).
    MemoryAddressSleb = 4,
    /// Linear-memory address as a 4-byte little-endian word, inside data.
    MemoryAddressI32 = 5,
    /// Type index as a relocatable unsigned LEB (the `call_indirect`
    /// immediate). The linker merges type sections, so even this is
    /// renumbered.
    TypeIndexLeb = 6,
    /// Global index as a relocatable unsigned LEB (a `global.get`/`global.set`).
    GlobalIndexLeb = 7,
}

impl RelocationKind {
    /// Whether the wire format carries an addend after the symbol index.
    /// Only the memory-address and offset kinds do.
    pub fn has_addend(self) -> bool {
        matches!(
            self,
            RelocationKind::MemoryAddressSleb | RelocationKind::MemoryAddressI32
        )
    }
}

/// One relocation, with `offset` relative to the containing *chunk* — a
/// function body or a data segment. Section-relative offsets are computed
/// during serialization, where the chunk's placement is known.
#[derive(Clone, Copy, Debug)]
pub struct Relocation {
    pub kind: RelocationKind,
    pub offset: u32,
    pub symbol_index: u32,
    pub addend: i32,
}

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymbolKind {
    Function = 0,
    Data = 1,
    Global = 2,
}

/// Where a data symbol's bytes live: a span of one data segment.
#[derive(Clone, Copy, Debug)]
pub struct DataSymbolLocation {
    pub segment_index: u32,
    pub offset: u32,
    pub size: u32,
}

/// What a symbol refers to. Undefined symbols still carry the index of the
/// import that stands in for them.
#[derive(Clone, Copy, Debug)]
pub enum SymbolTarget {
    /// Index into the function index space (imports first, then definitions).
    Function(u32),
    /// Index into the global index space (imports first, then definitions).
    Global(u32),
    /// A span of a data segment; `None` for an undefined data symbol.
    Data(Option<DataSymbolLocation>),
}

impl SymbolTarget {
    pub fn kind(&self) -> SymbolKind {
        match self {
            SymbolTarget::Function(_) => SymbolKind::Function,
            SymbolTarget::Global(_) => SymbolKind::Global,
            SymbolTarget::Data(_) => SymbolKind::Data,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub target: SymbolTarget,
    pub flags: u32,
}

impl Symbol {
    pub fn is_undefined(&self) -> bool {
        self.flags & symbol_flags::UNDEFINED != 0
    }
}

/// Per-segment metadata carried in the linking section.
pub struct SegmentInfo {
    pub name: String,
    pub alignment_log2: u32,
    pub flags: u32,
}

/// Serializes the `linking` custom section payload (without the section
/// header or the section name).
pub fn write_linking_payload(payload: &mut Vec<u8>, symbols: &[Symbol], segments: &[SegmentInfo]) {
    write_unsigned_leb128(payload, LINKING_METADATA_VERSION as u64);

    if !segments.is_empty() {
        let mut subsection = Vec::new();
        write_unsigned_leb128(&mut subsection, segments.len() as u64);
        for segment in segments {
            write_name(&mut subsection, &segment.name);
            write_unsigned_leb128(&mut subsection, segment.alignment_log2 as u64);
            write_unsigned_leb128(&mut subsection, segment.flags as u64);
        }
        write_subsection(payload, SUBSECTION_SEGMENT_INFO, &subsection);
    }

    let mut subsection = Vec::new();
    write_unsigned_leb128(&mut subsection, symbols.len() as u64);
    for symbol in symbols {
        write_symbol(&mut subsection, symbol);
    }
    write_subsection(payload, SUBSECTION_SYMBOL_TABLE, &subsection);
}

fn write_symbol(output: &mut Vec<u8>, symbol: &Symbol) {
    output.push(symbol.target.kind() as u8);
    write_unsigned_leb128(output, symbol.flags as u64);

    match symbol.target {
        SymbolTarget::Function(index) | SymbolTarget::Global(index) => {
            write_unsigned_leb128(output, index as u64);
            // An undefined function or global takes its name from the import
            // that stands in for it, unless EXPLICIT_NAME says otherwise.
            let name_is_implicit =
                symbol.is_undefined() && symbol.flags & symbol_flags::EXPLICIT_NAME == 0;
            if !name_is_implicit {
                write_name(output, &symbol.name);
            }
        }
        SymbolTarget::Data(location) => {
            // Data symbols always carry their name, defined or not.
            write_name(output, &symbol.name);
            if let Some(location) = location {
                write_unsigned_leb128(output, location.segment_index as u64);
                write_unsigned_leb128(output, location.offset as u64);
                write_unsigned_leb128(output, location.size as u64);
            }
        }
    }
}

fn write_subsection(output: &mut Vec<u8>, subsection_id: u8, payload: &[u8]) {
    output.push(subsection_id);
    write_unsigned_leb128(output, payload.len() as u64);
    output.extend_from_slice(payload);
}

/// Serializes a `reloc.*` custom section payload (without the section header
/// or the section name). `section_index` is the index of the target section
/// within the module's section sequence; offsets are relative to that
/// section's payload.
pub fn write_relocation_payload(
    payload: &mut Vec<u8>,
    section_index: u32,
    relocations: &[Relocation],
) {
    write_unsigned_leb128(payload, section_index as u64);
    write_unsigned_leb128(payload, relocations.len() as u64);
    for relocation in relocations {
        payload.push(relocation.kind as u8);
        write_unsigned_leb128(payload, relocation.offset as u64);
        write_unsigned_leb128(payload, relocation.symbol_index as u64);
        if relocation.kind.has_addend() {
            write_signed_leb128(payload, relocation.addend as i64);
        }
    }
}
