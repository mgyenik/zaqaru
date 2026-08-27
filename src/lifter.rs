//! Decoding machine code into instructions whose operands are *symbolic*.
//!
//! The core lifting invariant: no concrete address is ever assigned. A
//! function is decoded with its section offset as the instruction pointer, and
//! wherever an ELF relocation lands inside an instruction, that operand
//! becomes `symbol + addend` rather than a number. Program-counter relativity
//! is resolved away here, so nothing downstream needs to know it existed.

use anyhow::{Context, Result, bail};
use iced_x86::{Decoder, DecoderOptions, Instruction};

use crate::reader::{Function, ObjectFile};

/// An operand that names a symbol: the address it computes is
/// `address_of(symbol) + addend`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SymbolReference {
    /// Index into [`ObjectFile::symbols`].
    pub symbol: usize,
    /// Byte offset from the symbol. Program-counter relativity is already
    /// folded in: this is an offset from the symbol, not from anything else.
    pub addend: i64,
    /// True when the operand reaches the symbol through a global offset
    /// table slot rather than naming it directly. The translated module has
    /// no such table, so the operand denotes the symbol's address itself —
    /// the relaxation a linker would have performed.
    pub via_global_offset_table: bool,
}

/// One decoded instruction, with any symbolic operands it carries.
#[derive(Clone, Debug)]
pub struct LiftedInstruction {
    pub instruction: Instruction,
    /// Section offset of the instruction's first byte.
    pub offset: u64,
    /// Symbol named by the memory displacement, if a relocation covers it.
    pub displacement: Option<SymbolReference>,
    /// Symbol named by the immediate, if a relocation covers it.
    pub immediate: Option<SymbolReference>,
}

impl LiftedInstruction {
    pub fn length(&self) -> u64 {
        self.instruction.len() as u64
    }

    /// Section offset one past the instruction's last byte.
    pub fn end_offset(&self) -> u64 {
        self.offset + self.length()
    }
}

/// A decoded function: instructions in address order, still flat (the
/// control-flow graph is built from this by [`crate::cfg`]).
#[derive(Clone, Debug)]
pub struct LiftedFunction {
    pub name: String,
    /// Index into [`ObjectFile::functions`].
    pub function: usize,
    /// Index into [`ObjectFile::sections`] of the text section the function
    /// lives in. Branch and call targets without a relocation are offsets
    /// into this section.
    pub section: usize,
    /// Section offset of the function's first byte.
    pub offset: u64,
    pub size: u64,
    pub instructions: Vec<LiftedInstruction>,
    /// Indirect jumps that turned out to be `switch` dispatches, keyed by
    /// their index in [`Self::instructions`].
    pub jump_tables: std::collections::BTreeMap<usize, crate::jump_table::JumpTable>,
}

impl LiftedFunction {
    /// Whether a section offset falls inside this function.
    pub fn contains(&self, offset: u64) -> bool {
        offset >= self.offset && offset < self.offset + self.size
    }
}

/// Decodes one function, resolving every relocated operand to a symbol.
///
/// Jump tables are *not* recovered here: a table's extent depends on where
/// the next one begins, which is only knowable across the whole object. Use
/// [`lift_object`], which does both.
fn decode_function(object: &ObjectFile, function_index: usize) -> Result<LiftedFunction> {
    let function: &Function = &object.functions[function_index];
    let section = &object.sections[function.section];
    let start = function.offset as usize;
    let end = start + function.size as usize;
    let body = &section.bytes[start..end];

    let mut decoder = Decoder::with_ip(64, body, function.offset, DecoderOptions::NONE);
    let mut instructions = Vec::new();

    while decoder.can_decode() {
        let offset = decoder.ip();
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            bail!(
                "undecodable bytes in `{}` at offset {:#x}",
                function.name,
                offset
            );
        }
        let constant_offsets = decoder.get_constant_offsets(&instruction);
        let lifted =
            resolve_symbolic_operands(object, function, offset, instruction, constant_offsets)
                .with_context(|| {
                    format!(
                        "resolving relocated operands in `{}` at offset {offset:#x}",
                        function.name
                    )
                })?;
        instructions.push(lifted);
    }

    Ok(LiftedFunction {
        name: function.name.clone(),
        function: function_index,
        section: function.section,
        offset: function.offset,
        size: function.size,
        instructions,
        jump_tables: std::collections::BTreeMap::new(),
    })
}

/// Decodes every function in the object and recovers its jump tables.
pub fn lift_object(object: &ObjectFile) -> Result<Vec<LiftedFunction>> {
    let mut functions: Vec<LiftedFunction> = (0..object.functions.len())
        .map(|index| decode_function(object, index))
        .collect::<Result<_>>()?;
    crate::jump_table::recover_all(object, &mut functions).context("recovering jump tables")?;
    Ok(functions)
}

fn resolve_symbolic_operands(
    object: &ObjectFile,
    function: &Function,
    offset: u64,
    instruction: Instruction,
    constant_offsets: iced_x86::ConstantOffsets,
) -> Result<LiftedInstruction> {
    let length = instruction.len() as u64;
    let mut lifted = LiftedInstruction {
        instruction,
        offset,
        displacement: None,
        immediate: None,
    };

    for relocation in object.relocations_in(function.section, offset..offset + length) {
        let field_offset = relocation.offset - offset;
        let field_width = relocation.kind.width();

        // `symbol + addend + (instruction end - field start)` is the value a
        // program-counter-relative field ultimately computes; for an absolute
        // field the addend already is the offset from the symbol.
        let addend = if relocation.kind.is_program_counter_relative() {
            relocation.addend + (length - field_offset) as i64
        } else {
            relocation.addend
        };
        let reference = SymbolReference {
            symbol: relocation.symbol,
            addend,
            via_global_offset_table: relocation.kind.is_global_offset_table(),
        };

        // A relocation must line up exactly with an operand field: same
        // offset, same width. Anything else means we misread the encoding,
        // which is a bug, not something to paper over.
        let is_field = |start: usize, width: usize| -> bool {
            width != 0 && field_offset == start as u64 && field_width == width as u64
        };

        if is_field(
            constant_offsets.displacement_offset(),
            constant_offsets.displacement_size(),
        ) {
            lifted.displacement = Some(reference);
        } else if is_field(
            constant_offsets.immediate_offset(),
            constant_offsets.immediate_size(),
        ) || is_field(
            constant_offsets.immediate_offset2(),
            constant_offsets.immediate_size2(),
        ) {
            lifted.immediate = Some(reference);
        } else {
            bail!(
                "relocation at {:#x} does not line up with any operand field \
                 of the instruction (displacement at +{} width {}, immediate \
                 at +{} width {})",
                relocation.offset,
                constant_offsets.displacement_offset(),
                constant_offsets.displacement_size(),
                constant_offsets.immediate_offset(),
                constant_offsets.immediate_size(),
            );
        }
    }

    Ok(lifted)
}
