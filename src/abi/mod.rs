//! Signatures at the boundary between the emulated convention and ordinary
//! wasm.
//!
//! A translated function has wasm type `() -> ()` and keeps everything in the
//! emulated register file, which is what makes it correct without knowing any
//! calling convention. That property is also what makes it unreachable from
//! any wasm module that was not produced here. Both directions of interop
//! therefore need one thing the translator deliberately does without: a
//! function's actual signature.
//!
//! This module holds what a signature *is*, where the SysV argument
//! convention puts each of its parameters, and how a signature is written
//! down. It knows nothing about how signatures are discovered — declared,
//! inferred, or read out of debug information — so that every source can be
//! cross-checked against every other in one vocabulary.

pub mod effects;
pub mod infer;
pub mod marshal;

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::emitter::ValueType;

/// Registers holding the first six integer arguments, in SysV order.
pub const ARGUMENT_REGISTERS: [usize; 6] = [7, 6, 2, 1, 8, 9];
/// Register holding an integer return value.
pub const RETURN_VALUE_REGISTER: usize = 0;
/// XMM registers holding the first eight floating-point arguments, which
/// SysV numbers in order.
pub const FLOAT_ARGUMENT_REGISTERS: [usize; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
/// XMM register holding a floating-point return value.
pub const FLOAT_RETURN_VALUE_REGISTER: usize = 0;

/// A type as it crosses the boundary.
///
/// These are wasm value types rather than C types on purpose: what a thunk
/// has to know is how many bits to move and which register file to move them
/// through, and every C type that this project can carry across a boundary
/// answers those two questions as one of these four.
///
/// Pointers are [`AbiType::I32`]. That is not an approximation — the target
/// is wasm32, so an address really is 32 bits wide, and a guest pointer is a
/// 64-bit register holding one of those addresses with the top half zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AbiType {
    I32,
    I64,
    F32,
    F64,
}

impl AbiType {
    pub fn value_type(self) -> ValueType {
        match self {
            AbiType::I32 => ValueType::I32,
            AbiType::I64 => ValueType::I64,
            AbiType::F32 => ValueType::F32,
            AbiType::F64 => ValueType::F64,
        }
    }

    /// Which register file the SysV convention passes this type in.
    pub fn register_file(self) -> RegisterFile {
        match self {
            AbiType::I32 | AbiType::I64 => RegisterFile::Integer,
            AbiType::F32 | AbiType::F64 => RegisterFile::Float,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            AbiType::I32 => "i32",
            AbiType::I64 => "i64",
            AbiType::F32 => "f32",
            AbiType::F64 => "f64",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "i32" => Some(AbiType::I32),
            "i64" => Some(AbiType::I64),
            "f32" => Some(AbiType::F32),
            "f64" => Some(AbiType::F64),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegisterFile {
    Integer,
    Float,
}

/// Where SysV puts one argument: an index into the argument registers of
/// whichever file its type travels in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgumentLocation {
    /// An index into [`ARGUMENT_REGISTERS`].
    Integer(usize),
    /// An index into [`FLOAT_ARGUMENT_REGISTERS`], which is also the XMM
    /// register number.
    Float(usize),
}

/// A function's type at the boundary.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Signature {
    pub parameters: Vec<AbiType>,
    /// `None` is a `void` return, not an unknown one. Nothing in this module
    /// represents an unknown signature: a signature that is not known is
    /// absent from the table rather than present and vague.
    pub result: Option<AbiType>,
}

impl Signature {
    /// Where each parameter travels, in parameter order.
    ///
    /// The two register files are counted separately, which is the whole of
    /// SysV's scalar argument rule: an integer argument consumes the next
    /// integer register and a floating-point one the next SSE register,
    /// regardless of what came before it.
    pub fn argument_locations(&self) -> Result<Vec<ArgumentLocation>> {
        let mut locations = Vec::with_capacity(self.parameters.len());
        let (mut integers, mut floats) = (0usize, 0usize);
        for parameter in &self.parameters {
            match parameter.register_file() {
                RegisterFile::Integer => {
                    if integers >= ARGUMENT_REGISTERS.len() {
                        bail!(
                            "more than {} integer arguments, which travel on the \
                             stack — out of scope",
                            ARGUMENT_REGISTERS.len()
                        );
                    }
                    locations.push(ArgumentLocation::Integer(integers));
                    integers += 1;
                }
                RegisterFile::Float => {
                    if floats >= FLOAT_ARGUMENT_REGISTERS.len() {
                        bail!(
                            "more than {} floating-point arguments, which travel \
                             on the stack — out of scope",
                            FLOAT_ARGUMENT_REGISTERS.len()
                        );
                    }
                    locations.push(ArgumentLocation::Float(floats));
                    floats += 1;
                }
            }
        }
        Ok(locations)
    }

    /// Whether every parameter fits in a register, so a thunk can carry it.
    pub fn is_representable(&self) -> bool {
        self.argument_locations().is_ok()
    }

    /// How this signature is written in a declaration file.
    pub fn render(&self, name: &str) -> String {
        let parameters: Vec<&str> = self
            .parameters
            .iter()
            .map(|parameter| parameter.name())
            .collect();
        let rendered = format!("{name}({})", parameters.join(", "));
        match self.result {
            Some(result) => format!("{rendered} -> {}", result.name()),
            None => rendered,
        }
    }
}

/// Signatures by symbol name.
///
/// Deliberately a plain map with no notion of where an entry came from: the
/// point of the vocabulary is that a declared signature and an inferred one
/// are the same kind of thing and can be compared directly.
#[derive(Default, Clone, Debug)]
pub struct SignatureTable {
    entries: BTreeMap<String, Signature>,
}

impl SignatureTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&Signature> {
        self.entries.get(name)
    }

    pub fn insert(&mut self, name: impl Into<String>, signature: Signature) {
        self.entries.insert(name.into(), signature);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Signature)> {
        self.entries.iter()
    }

    /// Reads a declaration file.
    ///
    /// The format is one signature per line, written the way a wasm type is
    /// usually spoken, with `#` comments and blank lines ignored:
    ///
    /// ```text
    /// # the argument order is the C one; the types are what crosses
    /// add(i32, i32) -> i32
    /// scale(f64, i32) -> f64
    /// note_event(i64)
    /// ```
    ///
    /// A missing `->` is a `void` return. Pointers are written `i32`, which
    /// is what they are on a wasm32 target.
    pub fn parse(text: &str) -> Result<Self> {
        let mut table = Self::new();
        for (number, line) in text.lines().enumerate() {
            let line = match line.find('#') {
                Some(comment) => &line[..comment],
                None => line,
            }
            .trim();
            if line.is_empty() {
                continue;
            }
            let (name, signature) = parse_declaration(line)
                .map_err(|error| error.context(format!("line {}", number + 1)))?;
            if table.entries.contains_key(&name) {
                bail!("line {}: `{name}` is declared twice", number + 1);
            }
            table.insert(name, signature);
        }
        Ok(table)
    }

    /// Reads a declaration file from disk.
    pub fn read(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("reading {}: {error}", path.display()))?;
        Self::parse(&text).map_err(|error| error.context(format!("in {}", path.display())))
    }
}

fn parse_declaration(line: &str) -> Result<(String, Signature)> {
    let Some(open) = line.find('(') else {
        bail!("expected `name(types...)`, found `{line}`");
    };
    let name = line[..open].trim();
    if name.is_empty() {
        bail!("a declaration needs a symbol name: `{line}`");
    }
    let Some(close) = line[open..].find(')').map(|offset| offset + open) else {
        bail!("unclosed parameter list: `{line}`");
    };

    let inside = line[open + 1..close].trim();
    let mut parameters = Vec::new();
    if !inside.is_empty() {
        for piece in inside.split(',') {
            let piece = piece.trim();
            let Some(parameter) = AbiType::parse(piece) else {
                bail!("unknown type `{piece}` in `{line}` (expected i32, i64, f32 or f64)");
            };
            parameters.push(parameter);
        }
    }

    let tail = line[close + 1..].trim();
    let result = if tail.is_empty() {
        None
    } else if let Some(rest) = tail.strip_prefix("->") {
        let rest = rest.trim();
        match AbiType::parse(rest) {
            Some(result) => Some(result),
            None => bail!("unknown result type `{rest}` in `{line}`"),
        }
    } else {
        bail!("expected `-> type` or nothing after the parameter list: `{line}`");
    };

    Ok((name.to_string(), Signature { parameters, result }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declaration_file_round_trips() {
        let text = "\
            # a comment\n\
            \n\
            add(i32, i32) -> i32\n\
            scale(f64, i32) -> f64   # trailing comment\n\
            note(i64)\n\
            nothing()\n";
        let table = SignatureTable::parse(text).expect("parse");
        assert_eq!(table.len(), 4);
        assert_eq!(
            table.get("add"),
            Some(&Signature {
                parameters: vec![AbiType::I32, AbiType::I32],
                result: Some(AbiType::I32),
            })
        );
        assert_eq!(
            table.get("note"),
            Some(&Signature {
                parameters: vec![AbiType::I64],
                result: None,
            })
        );
        assert_eq!(
            table.get("nothing"),
            Some(&Signature {
                parameters: vec![],
                result: None,
            })
        );
        assert_eq!(
            table.get("scale").unwrap().render("scale"),
            "scale(f64, i32) -> f64"
        );
    }

    #[test]
    fn the_two_register_files_are_counted_separately() {
        let signature = Signature {
            parameters: vec![
                AbiType::F64,
                AbiType::I32,
                AbiType::F32,
                AbiType::I64,
                AbiType::F64,
            ],
            result: None,
        };
        assert_eq!(
            signature.argument_locations().unwrap(),
            vec![
                ArgumentLocation::Float(0),
                ArgumentLocation::Integer(0),
                ArgumentLocation::Float(1),
                ArgumentLocation::Integer(1),
                ArgumentLocation::Float(2),
            ]
        );
    }

    #[test]
    fn arguments_past_the_registers_are_refused() {
        let too_many = Signature {
            parameters: vec![AbiType::I64; ARGUMENT_REGISTERS.len() + 1],
            result: None,
        };
        assert!(!too_many.is_representable());

        let too_many_floats = Signature {
            parameters: vec![AbiType::F64; FLOAT_ARGUMENT_REGISTERS.len() + 1],
            result: None,
        };
        assert!(!too_many_floats.is_representable());
    }

    #[test]
    fn malformed_declarations_are_refused() {
        for bad in [
            "add i32, i32",
            "add(i32",
            "add(int) -> i32",
            "add(i32) => i32",
            "add(i32) -> void",
            "(i32) -> i32",
        ] {
            assert!(
                SignatureTable::parse(bad).is_err(),
                "`{bad}` should not have parsed"
            );
        }
    }

    #[test]
    fn a_repeated_declaration_is_refused() {
        assert!(SignatureTable::parse("add(i32) -> i32\nadd(i64) -> i64\n").is_err());
    }
}
