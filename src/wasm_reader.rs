//! Reading signatures out of a wasm object we are going to link against.
//!
//! Signature inference works on machine code, which is the right primary
//! source for the functions this project *translates* — they arrive as
//! stripped binaries and there is nothing else to read. It is the wrong
//! primary source for functions on the far side of a boundary, and the reason
//! is worth stating plainly, because it is a limit of information rather than
//! of the analysis.
//!
//! `int f(int x) { return g(x) + 1; }` compiles at `-O2` to `call g; add
//! $1,%eax; ret`. Nothing in the object mentions rdi. The value is passed
//! straight through, so neither the callee's own liveness nor the call site
//! can establish that `f` — or `g` — takes anything at all. No amount of
//! analysis recovers what was never written down.
//!
//! But it *was* written down, in the object that defines `g`. A wasm object
//! carries an explicit type for every function it defines, and interop means
//! having that object in hand — it is what is being linked against. So for
//! foreign functions the exact interface is available, and inference becomes
//! what it should be at a boundary: a cross-check that catches a mistake
//! rather than the thing being relied upon.
//!
//! Only enough of the format is read to answer one question: which names does
//! this object define, and with what type. Function bodies, relocations, data
//! and everything else are skipped.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::abi::{AbiType, Signature};

const MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6d];

const SECTION_CUSTOM: u8 = 0;
const SECTION_TYPE: u8 = 1;
const SECTION_IMPORT: u8 = 2;
const SECTION_FUNCTION: u8 = 3;

const IMPORT_FUNCTION: u8 = 0x00;
const SUBSECTION_SYMBOL_TABLE: u8 = 8;
const SYMBOL_KIND_FUNCTION: u8 = 0;
const SYMBOL_UNDEFINED: u32 = 0x10;
const SYMBOL_EXPLICIT_NAME: u32 = 0x40;

const TYPE_FUNCTION: u8 = 0x60;

/// Whether a file looks like a wasm object rather than an ELF one.
pub fn is_wasm_object(bytes: &[u8]) -> bool {
    bytes.starts_with(&MAGIC)
}

/// The signature of every function a wasm object defines, by symbol name.
///
/// Undefined symbols are skipped: an object that merely *calls* something
/// says what it expects, not what the thing is, and the expectation is
/// exactly what the linker will check anyway.
pub fn defined_signatures(bytes: &[u8]) -> Result<BTreeMap<String, Signature>> {
    let mut cursor = Cursor::new(bytes);
    cursor.expect_header()?;

    let mut types: Vec<Signature> = Vec::new();
    let mut imported_functions = 0usize;
    // Type index per *defined* function, in order.
    let mut function_types: Vec<u32> = Vec::new();
    let mut symbols: Vec<(String, u32, u32)> = Vec::new();

    while let Some((id, payload)) = cursor.next_section()? {
        let mut section = Cursor::new(payload);
        match id {
            SECTION_TYPE => {
                let count = section.leb128()?;
                for _ in 0..count {
                    types.push(section.function_type()?);
                }
            }
            SECTION_IMPORT => {
                let count = section.leb128()?;
                for _ in 0..count {
                    let _module = section.name()?;
                    let _field = section.name()?;
                    let kind = section.byte()?;
                    match kind {
                        IMPORT_FUNCTION => {
                            let _type_index = section.leb128()?;
                            imported_functions += 1;
                        }
                        // A table, memory, global or tag: skipped, but their
                        // descriptors have to be stepped over exactly.
                        0x01 => {
                            let _element = section.byte()?;
                            section.limits()?;
                        }
                        0x02 => section.limits()?,
                        0x03 => {
                            let _value_type = section.byte()?;
                            let _mutable = section.byte()?;
                        }
                        0x04 => {
                            let _attribute = section.byte()?;
                            let _type_index = section.leb128()?;
                        }
                        other => bail!("unknown import kind {other:#x} in a wasm object"),
                    }
                }
            }
            SECTION_FUNCTION => {
                let count = section.leb128()?;
                for _ in 0..count {
                    function_types.push(section.leb128()?);
                }
            }
            SECTION_CUSTOM => {
                let name = section.name()?;
                if name == "linking" {
                    read_linking(&mut section, &mut symbols)?;
                }
            }
            _ => {}
        }
    }

    let mut signatures = BTreeMap::new();
    for (name, flags, function_index) in symbols {
        if flags & SYMBOL_UNDEFINED != 0 {
            continue;
        }
        let Some(defined) = (function_index as usize).checked_sub(imported_functions) else {
            continue;
        };
        let Some(type_index) = function_types.get(defined) else {
            continue;
        };
        let Some(signature) = types.get(*type_index as usize) else {
            continue;
        };
        signatures.insert(name, signature.clone());
    }
    Ok(signatures)
}

fn read_linking(section: &mut Cursor<'_>, symbols: &mut Vec<(String, u32, u32)>) -> Result<()> {
    let _version = section.leb128()?;
    while !section.finished() {
        let id = section.byte()?;
        let size = section.leb128()? as usize;
        let payload = section.take(size)?;
        if id != SUBSECTION_SYMBOL_TABLE {
            continue;
        }
        let mut table = Cursor::new(payload);
        let count = table.leb128()?;
        for _ in 0..count {
            let kind = table.byte()?;
            let flags = table.leb128()?;
            if kind != SYMBOL_KIND_FUNCTION {
                // Only function symbols matter here, but every symbol still
                // has to be stepped over exactly to stay in sync.
                skip_non_function_symbol(&mut table, kind, flags)?;
                continue;
            }
            let index = table.leb128()?;
            // An undefined function takes its name from the import that
            // stands in for it, and carries no name of its own.
            let named = flags & SYMBOL_UNDEFINED == 0 || flags & SYMBOL_EXPLICIT_NAME != 0;
            let name = if named {
                table.name()?.to_string()
            } else {
                String::new()
            };
            if !name.is_empty() {
                symbols.push((name, flags, index));
            }
        }
    }
    Ok(())
}

fn skip_non_function_symbol(table: &mut Cursor<'_>, kind: u8, flags: u32) -> Result<()> {
    const SYMBOL_KIND_DATA: u8 = 1;
    if kind == SYMBOL_KIND_DATA {
        let _name = table.name()?;
        if flags & SYMBOL_UNDEFINED == 0 {
            let _segment = table.leb128()?;
            let _offset = table.leb128()?;
            let _size = table.leb128()?;
        }
        return Ok(());
    }
    // Globals, tables, tags and sections all encode an index, then a name
    // unless they are undefined.
    let _index = table.leb128()?;
    if flags & SYMBOL_UNDEFINED == 0 || flags & SYMBOL_EXPLICIT_NAME != 0 {
        let _name = table.name()?;
    }
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn finished(&self) -> bool {
        self.position >= self.bytes.len()
    }

    fn expect_header(&mut self) -> Result<()> {
        let header = self.take(8)?;
        if header[..4] != MAGIC {
            bail!("not a wasm object");
        }
        Ok(())
    }

    fn next_section(&mut self) -> Result<Option<(u8, &'a [u8])>> {
        if self.finished() {
            return Ok(None);
        }
        let id = self.byte()?;
        let size = self.leb128()? as usize;
        Ok(Some((id, self.take(size)?)))
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| anyhow::anyhow!("truncated wasm object"))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn leb128(&mut self) -> Result<u32> {
        let mut value = 0u32;
        let mut shift = 0;
        loop {
            let byte = self.byte()?;
            value |= u32::from(byte & 0x7f)
                .checked_shl(shift)
                .ok_or_else(|| anyhow::anyhow!("oversized LEB128 in a wasm object"))?;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    fn name(&mut self) -> Result<&'a str> {
        let length = self.leb128()? as usize;
        std::str::from_utf8(self.take(length)?)
            .map_err(|_| anyhow::anyhow!("a name in a wasm object is not valid UTF-8"))
    }

    fn limits(&mut self) -> Result<()> {
        let flags = self.byte()?;
        let _minimum = self.leb128()?;
        if flags & 0x01 != 0 {
            let _maximum = self.leb128()?;
        }
        Ok(())
    }

    fn function_type(&mut self) -> Result<Signature> {
        let form = self.byte()?;
        if form != TYPE_FUNCTION {
            bail!("type {form:#x} in a wasm object is not a function type");
        }
        let count = self.leb128()?;
        let mut parameters = Vec::new();
        for _ in 0..count {
            parameters.push(value_type(self.byte()?)?);
        }
        let results = self.leb128()?;
        if results > 1 {
            bail!("a function with {results} results cannot be described as a C signature");
        }
        let result = if results == 1 {
            Some(value_type(self.byte()?)?)
        } else {
            None
        };
        Ok(Signature { parameters, result })
    }
}

fn value_type(encoding: u8) -> Result<AbiType> {
    Ok(match encoding {
        0x7f => AbiType::I32,
        0x7e => AbiType::I64,
        0x7d => AbiType::F32,
        0x7c => AbiType::F64,
        other => bail!("value type {other:#x} does not cross a C boundary"),
    })
}
