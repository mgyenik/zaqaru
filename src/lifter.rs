//! Decoding machine code into instructions whose operands are *symbolic*.
//!
//! The core lifting invariant: no concrete address is ever assigned. A
//! function is decoded with its section offset as the instruction pointer, and
//! wherever an ELF relocation lands inside an instruction, that operand
//! becomes `symbol + addend` rather than a number. Program-counter relativity
//! is resolved away here, so nothing downstream needs to know it existed.

use std::collections::BTreeSet;
use std::ops::Bound;

use anyhow::{Context, Result, bail};
use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, OpKind};

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
/// Whether control continues past an instruction into the next bytes.
///
/// A `ret` or an unconditional jump does not, which is what makes the bytes
/// after one nobody's instructions.
fn continues_past(instruction: &Instruction) -> bool {
    !matches!(
        instruction.flow_control(),
        FlowControl::Return | FlowControl::UnconditionalBranch | FlowControl::IndirectBranch
    )
}

/// How many times [`decode_function`] will re-sweep before deciding its own
/// reasoning is wrong. Every real function settles in one or two.
const MAX_DECODE_ROUNDS: usize = 8;

/// One sweep, with a set of offsets that are known to begin instructions.
///
/// An instruction that would span one of them is discarded and decoding
/// resumes there. What is thrown away is unreachable by construction — it is
/// only ever the bytes after a call that does not return — so the fall-through
/// this leaves behind, from the instruction before the discarded one straight
/// to the boundary, describes a path no execution takes.
fn decode_pass(
    object: &ObjectFile,
    function: &Function,
    body: &[u8],
    boundaries: &BTreeSet<u64>,
    poisoned: &mut BTreeSet<u64>,
) -> Result<Vec<LiftedInstruction>> {
    let mut decoder = Decoder::with_ip(64, body, function.offset, DecoderOptions::NONE);
    let mut instructions = Vec::new();

    while decoder.can_decode() {
        let offset = decoder.ip();
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            // Not an error yet. Bytes nothing reaches are not instructions
            // and are not required to decode as any — the padding after a
            // call that never returns is exactly that, and here it happens
            // to spell a `lock` prefix on a register operand, which no
            // decoder will accept. Whether this mattered is decided after
            // the boundaries have settled: if something falls through into
            // it, it is an error and says so.
            poisoned.insert(offset);
            decoder.set_position((offset - function.offset) as usize + 1)?;
            decoder.set_ip(offset + 1);
            continue;
        }
        let length = instruction.len() as u64;
        if let Some(boundary) = boundaries
            .range((Bound::Excluded(offset), Bound::Excluded(offset + length)))
            .next()
            .copied()
        {
            decoder.set_position((boundary - function.offset) as usize)?;
            decoder.set_ip(boundary);
            continue;
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
    Ok(instructions)
}

fn decode_function(object: &ObjectFile, function_index: usize) -> Result<LiftedFunction> {
    let function: &Function = &object.functions[function_index];
    let section = &object.sections[function.section];
    let start = function.offset as usize;
    let end = start + function.size as usize;
    let body = &section.bytes[start..end];

    // Decoding is a fixpoint rather than a sweep, because a linear sweep can
    // be wrong about where instructions begin.
    //
    // The bytes after a call to a function that never returns are not
    // instructions — nothing reaches them — but a decoder does not know
    // that and decodes them anyway, and what it produces can *straddle* the
    // place a real instruction starts. glibc's `____longjmp_chk` is the
    // case: two branches target one offset, and the padding after its
    // `call __fortify_fail` decodes into an instruction that spans through
    // it. Sweeping linearly, that offset is not an instruction boundary and
    // the function is refused.
    //
    // So: sweep, collect the branch targets, and if any of them landed
    // inside an instruction, sweep again with that offset known to be a
    // boundary — an instruction that would straddle one is discarded and
    // decoding resumes there. Each round can only discover more boundaries
    // and there are finitely many offsets, so it terminates; the bound is
    // there to make a bug in that reasoning loud rather than silent.
    let mut boundaries: BTreeSet<u64> = BTreeSet::new();
    let mut instructions;
    let mut poisoned = BTreeSet::new();
    let mut rounds = 0;
    loop {
        poisoned.clear();
        instructions = decode_pass(object, function, body, &boundaries, &mut poisoned)?;
        let starts: BTreeSet<u64> = instructions.iter().map(|lifted| lifted.offset).collect();
        let discovered: BTreeSet<u64> = instructions
            .iter()
            .filter(|lifted| {
                matches!(
                    lifted.instruction.flow_control(),
                    FlowControl::ConditionalBranch
                        | FlowControl::UnconditionalBranch
                        | FlowControl::Call
                ) && lifted.instruction.op0_kind() == OpKind::NearBranch64
            })
            .map(|lifted| lifted.instruction.near_branch64())
            .filter(|target| {
                *target >= function.offset
                    && *target < function.offset + function.size
                    && !starts.contains(target)
                    && !boundaries.contains(target)
            })
            .collect();
        if discovered.is_empty() {
            // Now that the boundaries have settled, undecodable bytes are an
            // error exactly when something runs into them: an instruction
            // that continues and whose next byte is the first poisoned one.
            // That is the case where the decode has lost sync with the
            // program, which has to be loud — the alternative is a
            // translation quietly missing instructions.
            for lifted in &instructions {
                let next = lifted.offset + lifted.length();
                if poisoned.contains(&next) && continues_past(&lifted.instruction) {
                    bail!(
                        "undecodable bytes in `{}` at offset {next:#x}, which \
                         `{:#x}` runs into",
                        function.name,
                        lifted.offset
                    );
                }
            }
            break;
        }
        boundaries.extend(discovered);
        rounds += 1;
        if rounds > MAX_DECODE_ROUNDS {
            bail!(
                "the instruction boundaries in `{}` did not settle after \
                 {MAX_DECODE_ROUNDS} rounds",
                function.name
            );
        }
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
