//! Recovering `switch` dispatches from indirect jumps.
//!
//! A jump table is the one construct where the address-space split really
//! bites. Its entries are *code* addresses, which have no spelling in wasm at
//! all — not a linear-memory address, not a table index — so the dispatch
//! cannot be translated the way everything else is. It has to be recognised
//! and turned into a `br_table` over basic blocks.
//!
//! ## Recognition without idiom matching
//!
//! The obvious approach is to match the instruction sequence that computes
//! the target, pull the index register out of it, and branch on that. It does
//! not survive contact with real compilers: gcc and clang emit different
//! shapes, each changes shape between `-O0` and `-O1` and again between
//! position-independent and absolute code — the index is scaled inside the
//! jump on one path and in a separate `lea` on another, the loaded entry is
//! widened by `movsxd` here and by `cdqe` there.
//!
//! So the index is not recovered at all. Instead the transpiler uses the one
//! thing it controls: **the contents of the table**. However the address
//! arithmetic is spelled, every dispatch ends up computing
//!
//! ```text
//! target = table_address + entry     (entries stored as differences)
//! target = entry                     (entries stored as whole addresses)
//! ```
//!
//! so if entry `k` is rewritten to hold `k` in the first case and
//! `table_address + k` in the second, the value the guest arrives at is
//! `table_address + k` either way, and the dispatch becomes
//!
//! ```text
//! br_table over (computed target − table_address)
//! ```
//!
//! Every instruction computing that target is then translated normally, with
//! nothing suppressed and no register left holding something the machine
//! would not have held. What has to be recognised is only *which table* a
//! jump reads, and that comes from relocations rather than instruction
//! shapes.
//!
//! ## Telling a table from a function pointer
//!
//! Both are data holding relocations against a text section. What separates
//! them is where those relocations land: a function pointer names the *start*
//! of a function, while a jump table's entries name blocks inside the
//! function that dispatches through them — which always lie after the
//! dispatch, and so are never function starts.

use anyhow::{Result, bail};
use iced_x86::FlowControl;

use crate::lifter::LiftedFunction;
use crate::reader::{ObjectFile, Relocation, SectionRole};

/// A recovered `switch`.
#[derive(Clone, Debug)]
pub struct JumpTable {
    /// Section offsets of the blocks the dispatch can reach, in index order.
    pub targets: Vec<u64>,
    /// Where the table lives.
    pub table_section: usize,
    pub table_offset: u64,
    /// Bytes per entry, taken from the width of the entries' relocations.
    pub stride: u64,
    /// Whether entries are stored as differences from the table, which is
    /// what decides how they are rewritten.
    pub relative: bool,
}

impl JumpTable {
    pub fn byte_length(&self) -> u64 {
        self.targets.len() as u64 * self.stride
    }

    /// The `(section, offset)` of every entry. Their original relocations are
    /// replaced rather than translated: they name code, which has no address.
    pub fn entries(&self) -> impl Iterator<Item = (usize, u64)> + '_ {
        (0..self.targets.len() as u64)
            .map(move |index| (self.table_section, self.table_offset + index * self.stride))
    }
}

/// Where a dispatch reads its table, before the table's extent is known.
struct Candidate {
    /// Index into the owning function's instructions.
    position: usize,
    table_section: usize,
    table_offset: u64,
}

/// Finds every jump table in an object and attaches each to the function that
/// dispatches through it.
///
/// This works over all the functions together because a table's *end* is only
/// knowable that way: what stops one table is usually the beginning of the
/// next, which frequently belongs to a different function. Reading one
/// table's entries as another's does not merely add unreachable arms — in the
/// relative form entries are stored against their own table, so the targets
/// come out wrong rather than surplus.
pub fn recover_all(object: &ObjectFile, functions: &mut [LiftedFunction]) -> Result<()> {
    let mut candidates: Vec<(usize, Candidate)> = Vec::new();
    for (index, function) in functions.iter().enumerate() {
        for (position, lifted) in function.instructions.iter().enumerate() {
            if lifted.instruction.flow_control() != FlowControl::IndirectBranch {
                continue;
            }
            if let Some(candidate) = propose(object, function, position) {
                candidates.push((index, candidate));
            }
        }
    }

    let starts: Vec<(usize, u64)> = candidates
        .iter()
        .map(|(_, candidate)| (candidate.table_section, candidate.table_offset))
        .collect();

    for (function_index, candidate) in &candidates {
        let limit = table_limit(object, &starts, candidate);
        let Some(table) = read_table(object, &functions[*function_index], candidate, limit) else {
            continue;
        };
        functions[*function_index]
            .jump_tables
            .insert(candidate.position, table);
    }

    for function in functions.iter() {
        for (position, lifted) in function.instructions.iter().enumerate() {
            if lifted.instruction.flow_control() == FlowControl::IndirectBranch
                && !function.jump_tables.contains_key(&position)
            {
                reject_unrecognised_dispatch(object, function, position)?;
            }
        }
    }
    Ok(())
}

/// The nearest data location the dispatch could be reading its table from.
///
/// Nearest matters: a function with two dispatches names both tables, and
/// each loads the address of its own shortly beforehand.
fn propose(object: &ObjectFile, function: &LiftedFunction, position: usize) -> Option<Candidate> {
    for lifted in function.instructions[..=position].iter().rev() {
        for reference in [lifted.displacement, lifted.immediate]
            .into_iter()
            .flatten()
        {
            let Some((section, table_offset)) =
                data_location(object, reference.symbol, reference.addend)
            else {
                continue;
            };
            if holds_block_addresses(object, function, section, table_offset) {
                return Some(Candidate {
                    position,
                    table_section: section,
                    table_offset,
                });
            }
        }
    }
    None
}

/// Whether a data location begins a run of references to blocks *inside* this
/// function — as opposed to a function pointer, which names a function's
/// start.
fn holds_block_addresses(
    object: &ObjectFile,
    function: &LiftedFunction,
    section: usize,
    offset: u64,
) -> bool {
    let Some(relocation) = relocation_at(object, section, offset) else {
        return false;
    };
    let Some(target) = resolve_entry(object, relocation, offset, offset) else {
        return false;
    };
    target != function.offset && begins_an_instruction(function, target)
}

/// Where a table ends: at the next table in the same section, or at the end
/// of the section.
fn table_limit(object: &ObjectFile, starts: &[(usize, u64)], candidate: &Candidate) -> u64 {
    starts
        .iter()
        .filter(|(section, offset)| {
            *section == candidate.table_section && *offset > candidate.table_offset
        })
        .map(|(_, offset)| *offset)
        .min()
        .unwrap_or_else(|| object.sections[candidate.table_section].size)
}

/// Reads a table's entries, stopping where they stop looking like entries.
fn read_table(
    object: &ObjectFile,
    function: &LiftedFunction,
    candidate: &Candidate,
    limit: u64,
) -> Option<JumpTable> {
    let first = relocation_at(object, candidate.table_section, candidate.table_offset)?;
    let stride = first.kind.width();
    let relative = first.kind.is_program_counter_relative();

    let mut targets = Vec::new();
    for index in 0.. {
        let entry_offset = candidate.table_offset + index * stride;
        if entry_offset + stride > limit {
            break;
        }
        let Some(relocation) = relocation_at(object, candidate.table_section, entry_offset) else {
            break;
        };
        if relocation.kind.width() != stride
            || relocation.kind.is_program_counter_relative() != relative
        {
            break;
        }
        let Some(target) = resolve_entry(object, relocation, entry_offset, candidate.table_offset)
        else {
            break;
        };
        if !begins_an_instruction(function, target) {
            break;
        }
        targets.push(target);
    }

    if targets.is_empty() {
        return None;
    }
    Some(JumpTable {
        targets,
        table_section: candidate.table_section,
        table_offset: candidate.table_offset,
        stride,
        relative,
    })
}

/// The text offset one entry names.
fn resolve_entry(
    object: &ObjectFile,
    relocation: &Relocation,
    entry_offset: u64,
    table_offset: u64,
) -> Option<u64> {
    let (section, offset) = object.resolve(relocation.symbol, relocation.addend)?;
    if object.sections[section].role != SectionRole::Text {
        return None;
    }
    // An entry of the relative form holds the difference between its target
    // and its own position, so the position has to be taken back out.
    let target = if relocation.kind.is_program_counter_relative() {
        offset - (entry_offset - table_offset) as i64
    } else {
        offset
    };
    u64::try_from(target).ok()
}

fn relocation_at(object: &ObjectFile, section: usize, offset: u64) -> Option<&Relocation> {
    object.sections[section]
        .relocations
        .iter()
        .find(|relocation| relocation.offset == offset)
}

/// An indirect jump that reads no table is an indirect tail call. One that
/// reads something table-shaped which the entry scan then rejected is a
/// dispatch we failed to recover, and must not be translated as a call.
fn reject_unrecognised_dispatch(
    object: &ObjectFile,
    function: &LiftedFunction,
    position: usize,
) -> Result<()> {
    for lifted in &function.instructions[..=position] {
        for reference in [lifted.displacement, lifted.immediate]
            .into_iter()
            .flatten()
        {
            let Some((section, offset)) = data_location(object, reference.symbol, reference.addend)
            else {
                continue;
            };
            let names_this_function = object.sections[section]
                .relocations
                .iter()
                .filter(|relocation| relocation.offset >= offset)
                .take(2)
                .any(|relocation| {
                    resolve_entry(object, relocation, relocation.offset, offset)
                        .is_some_and(|target| function.contains(target))
                });
            if names_this_function {
                bail!(
                    "`{}` at {:#x} dispatches through what looks like a jump \
                     table in {}, but its entries could not be read; \
                     translating it as an indirect call would branch somewhere \
                     arbitrary",
                    crate::translate::render(&function.instructions[position].instruction),
                    function.instructions[position].offset,
                    object.sections[section].name,
                );
            }
        }
    }
    Ok(())
}

/// Resolves a symbolic reference to a location in a data section, or `None`
/// if it names something else.
fn data_location(object: &ObjectFile, symbol: usize, addend: i64) -> Option<(usize, u64)> {
    let (section, offset) = object.resolve(symbol, addend)?;
    if !object.sections[section].role.is_data() || offset < 0 {
        return None;
    }
    Some((section, offset as u64))
}

/// Whether an offset is the start of an instruction of this function — the
/// last check that an entry really is one.
fn begins_an_instruction(function: &LiftedFunction, offset: u64) -> bool {
    function.contains(offset)
        && function
            .instructions
            .iter()
            .any(|lifted| lifted.offset == offset)
}
