//! An independent reader for the linking metadata in a relocatable wasm
//! object.
//!
//! This deliberately does not reuse the emitter's code. Reading our own
//! output back with our own writer would only prove the writer is
//! self-consistent; the point is to check it against the format as an
//! outside party understands it, and against what clang produces for the
//! same construct.

#![allow(dead_code)]

/// One section of a wasm module, located in the file.
#[derive(Debug)]
pub struct Section {
    pub id: u8,
    pub name: Option<String>,
    /// Position of the section's payload within the file. For a custom
    /// section this includes the name, matching what `reloc.*` offsets are
    /// measured against for non-custom sections.
    pub payload: std::ops::Range<usize>,
    /// Index of this section among all sections, which is what a `reloc.*`
    /// section names.
    pub index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolTarget {
    Function { index: u32 },
    Global { index: u32 },
    Data(Option<DataLocation>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataLocation {
    pub segment: u32,
    pub offset: u32,
    pub size: u32,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: Option<String>,
    pub flags: u32,
    pub target: SymbolTarget,
}

impl Symbol {
    pub fn is_undefined(&self) -> bool {
        self.flags & 0x10 != 0
    }
    pub fn is_weak(&self) -> bool {
        self.flags & 0x01 != 0
    }
    pub fn is_local(&self) -> bool {
        self.flags & 0x02 != 0
    }
    pub fn is_exported(&self) -> bool {
        self.flags & 0x20 != 0
    }
}

#[derive(Clone, Debug)]
pub struct SegmentInfo {
    pub name: String,
    pub alignment_log2: u32,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Relocation {
    pub kind: u8,
    pub offset: u32,
    pub symbol_index: u32,
    pub addend: Option<i64>,
}

#[derive(Debug)]
pub struct RelocationSection {
    pub name: String,
    pub target_section: u32,
    pub entries: Vec<Relocation>,
}

/// Everything the linking metadata of an object says.
#[derive(Debug)]
pub struct LinkingMetadata {
    pub version: u32,
    pub symbols: Vec<Symbol>,
    pub segments: Vec<SegmentInfo>,
    pub relocations: Vec<RelocationSection>,
    pub sections: Vec<Section>,
}

impl LinkingMetadata {
    pub fn symbol_named(&self, name: &str) -> Option<&Symbol> {
        self.symbols
            .iter()
            .find(|symbol| symbol.name.as_deref() == Some(name))
    }

    pub fn relocations_for(&self, section_name: &str) -> Option<&RelocationSection> {
        self.relocations
            .iter()
            .find(|section| section.name == format!("reloc.{section_name}"))
    }

    pub fn section(&self, index: u32) -> Option<&Section> {
        self.sections.iter().find(|section| section.index == index)
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn byte(&mut self) -> u8 {
        let value = self.bytes[self.position];
        self.position += 1;
        value
    }

    fn unsigned(&mut self) -> u64 {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.byte();
            result |= u64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return result;
            }
        }
    }

    fn signed(&mut self) -> i64 {
        let mut result = 0i64;
        let mut shift = 0;
        loop {
            let byte = self.byte();
            result |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                return result;
            }
        }
    }

    fn name(&mut self) -> String {
        let length = self.unsigned() as usize;
        let text = String::from_utf8_lossy(&self.bytes[self.position..self.position + length])
            .into_owned();
        self.position += length;
        text
    }

    fn done(&self) -> bool {
        self.position >= self.bytes.len()
    }
}

/// Splits a wasm module into its sections.
pub fn read_sections(module: &[u8]) -> Vec<Section> {
    assert_eq!(&module[0..4], b"\0asm", "not a wasm module");
    let mut reader = Reader::new(module);
    reader.position = 8;

    let mut sections = Vec::new();
    let mut index = 0;
    while !reader.done() {
        let id = reader.byte();
        let length = reader.unsigned() as usize;
        let payload = reader.position..reader.position + length;
        let name = if id == 0 {
            let mut inner = Reader::new(&module[payload.clone()]);
            Some(inner.name())
        } else {
            None
        };
        sections.push(Section {
            id,
            name,
            payload: payload.clone(),
            index,
        });
        reader.position = payload.end;
        index += 1;
    }
    sections
}

/// Reads the `linking` and `reloc.*` sections of a relocatable object.
pub fn read_linking_metadata(module: &[u8]) -> LinkingMetadata {
    let sections = read_sections(module);
    let mut version = 0;
    let mut symbols = Vec::new();
    let mut segments = Vec::new();
    let mut relocations = Vec::new();

    for section in &sections {
        let Some(name) = section.name.as_deref() else {
            continue;
        };
        let payload = &module[section.payload.clone()];
        let mut reader = Reader::new(payload);
        let _ = reader.name();

        if name == "linking" {
            version = reader.unsigned() as u32;
            while !reader.done() {
                let subsection_id = reader.byte();
                let length = reader.unsigned() as usize;
                let end = reader.position + length;
                match subsection_id {
                    5 => {
                        let count = reader.unsigned();
                        for _ in 0..count {
                            segments.push(SegmentInfo {
                                name: reader.name(),
                                alignment_log2: reader.unsigned() as u32,
                                flags: reader.unsigned() as u32,
                            });
                        }
                    }
                    8 => {
                        let count = reader.unsigned();
                        for _ in 0..count {
                            symbols.push(read_symbol(&mut reader));
                        }
                    }
                    _ => {}
                }
                reader.position = end;
            }
        } else if let Some(suffix) = name.strip_prefix("reloc.") {
            let target_section = reader.unsigned() as u32;
            let count = reader.unsigned();
            let mut entries = Vec::new();
            for _ in 0..count {
                let kind = reader.byte();
                let offset = reader.unsigned() as u32;
                let symbol_index = reader.unsigned() as u32;
                let addend = if relocation_kind_has_addend(kind) {
                    Some(reader.signed())
                } else {
                    None
                };
                entries.push(Relocation {
                    kind,
                    offset,
                    symbol_index,
                    addend,
                });
            }
            relocations.push(RelocationSection {
                name: format!("reloc.{suffix}"),
                target_section,
                entries,
            });
        }
    }

    LinkingMetadata {
        version,
        symbols,
        segments,
        relocations,
        sections,
    }
}

fn read_symbol(reader: &mut Reader<'_>) -> Symbol {
    const UNDEFINED: u32 = 0x10;
    const EXPLICIT_NAME: u32 = 0x40;

    let kind = reader.byte();
    let flags = reader.unsigned() as u32;
    match kind {
        0 | 2 => {
            let index = reader.unsigned() as u32;
            let implicit_name = flags & UNDEFINED != 0 && flags & EXPLICIT_NAME == 0;
            let name = if implicit_name {
                None
            } else {
                Some(reader.name())
            };
            let target = if kind == 0 {
                SymbolTarget::Function { index }
            } else {
                SymbolTarget::Global { index }
            };
            Symbol {
                name,
                flags,
                target,
            }
        }
        1 => {
            let name = reader.name();
            let location = if flags & UNDEFINED == 0 {
                Some(DataLocation {
                    segment: reader.unsigned() as u32,
                    offset: reader.unsigned() as u32,
                    size: reader.unsigned() as u32,
                })
            } else {
                None
            };
            Symbol {
                name: Some(name),
                flags,
                target: SymbolTarget::Data(location),
            }
        }
        other => panic!("unknown symbol kind {other} in the linking section"),
    }
}

/// Which relocation types carry an addend, per the linking format.
pub fn relocation_kind_has_addend(kind: u8) -> bool {
    matches!(kind, 5 | 4 | 3 | 8 | 9 | 14 | 15 | 16 | 18)
}

/// Whether the relocation type patches a LEB128 immediate, and so needs a
/// fixed-width five-byte site.
pub fn relocation_kind_is_leb(kind: u8) -> bool {
    matches!(kind, 0 | 1 | 3 | 4 | 6 | 7 | 10 | 11 | 12)
}

/// Checks that a five-byte span really is a non-canonical, linker-patchable
/// LEB128: four continuation bytes followed by a terminator.
pub fn is_relocatable_leb_site(bytes: &[u8]) -> bool {
    bytes.len() >= 5 && bytes[..4].iter().all(|byte| byte & 0x80 != 0) && bytes[4] & 0x80 == 0
}
