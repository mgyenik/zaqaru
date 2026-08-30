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
//!
//! ## A linked executable has no relocations
//!
//! Everything above reads a table through the relocations on its entries:
//! they say where each entry points, how wide it is, and whether it holds a
//! difference or an address. A linked executable has none — the linker
//! consumed them and left the bytes — so all three facts have to come from
//! the bytes themselves.
//!
//! The width and the form are *inferred*, by trying each and keeping
//! whichever reads as a table. That sounds weak and is not, because the test
//! each candidate entry has to pass is severe: its target must land exactly
//! on an instruction boundary inside the dispatching function. Four
//! arbitrary bytes almost never do. Two forms are tried — eight-byte
//! absolute, which is what gcc emits for `-fno-pie`, and four-byte
//! differences against the table's own address, which is what it emits for
//! position-independent code — and the one yielding the longer run wins.

use anyhow::{Result, bail};
use iced_x86::FlowControl;

use crate::lifter::LiftedFunction;
use crate::reader::{Layout, ObjectFile, Relocation, SectionRole};

/// How a table's entries are stored, for an input where nothing says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct EntryForm {
    stride: u64,
    /// What an entry is a difference *from*, or `None` when it holds the
    /// target outright.
    ///
    /// Usually the table's own address, which is what a compiler emits for a
    /// `switch` in position-independent code. Not always: a **computed
    /// goto** stores differences from a code label instead, because the
    /// source wrote `&&label - &&base` and the compiler had no reason to
    /// pick the table. glibc's `__vfprintf_internal` is exactly this — its
    /// dispatch does
    ///
    /// ```text
    /// lea    table(%rip),%rsi
    /// lea    base(%rip),%rdi      ; a label inside the function
    /// movslq (%rsi,%rax,4),%rax
    /// add    %rdi,%rax
    /// jmp    *%rax
    /// ```
    ///
    /// and reading its entries against the table's address gives targets
    /// that are not instruction boundaries, so the table is not recognised
    /// at all and the dispatch becomes an indirect call into the middle of a
    /// function. The base is not guessed: it is an address the dispatch
    /// sequence itself computes.
    base: Option<u64>,
}

/// The forms worth trying, in the order a compiler is likely to have used
/// them. Both are real: `-fno-pie` gives whole addresses, and
/// position-independent code gives four-byte differences, because a
/// difference needs no relocation at load time.
/// The forms to try for one candidate table, in the order a compiler is
/// likely to have used them.
///
/// Eight-byte whole addresses is what `-fno-pie` gives. Four-byte
/// differences is what position-independent code gives, and the thing they
/// are differences from is either the table itself or some address the
/// dispatch computes — so every text address the dispatch mentions is
/// offered as a base, and the entry scan decides.
fn entry_forms(table_address: u64, bases: &[u64]) -> Vec<EntryForm> {
    let mut forms = vec![EntryForm {
        stride: 8,
        base: None,
    }];
    forms.push(EntryForm {
        stride: 4,
        base: Some(table_address),
    });
    forms.extend(bases.iter().map(|base| EntryForm {
        stride: 4,
        base: Some(*base),
    }));
    forms
}

/// Reads one entry of a linked table, and returns the section offset it
/// names — or `None` if it does not name code in the dispatching function's
/// section.
fn read_linked_entry(
    object: &ObjectFile,
    section: usize,
    entry_offset: u64,
    form: EntryForm,
) -> Option<u64> {
    let bytes = &object.sections[section].bytes;
    let at = entry_offset as usize;
    let end = at.checked_add(form.stride as usize)?;
    if end > bytes.len() {
        return None;
    }
    let address = match form.base {
        Some(base) => {
            let value = i32::from_le_bytes(bytes[at..end].try_into().ok()?) as i64;
            (base as i64).checked_add(value)? as u64
        }
        None => u64::from_le_bytes(bytes[at..end].try_into().ok()?),
    };
    let (target_section, offset) = object.section_at(address)?;
    (object.sections[target_section].role == SectionRole::Text).then_some(offset)
}

/// A recovered `switch`.
#[derive(Clone, Debug)]
pub struct JumpTable {
    /// Section offsets of the blocks the dispatch can reach, in index order.
    ///
    /// This is the arm space of the dispatch's [`origin`](Self::origin), not
    /// of this one table: where several tables measure from the same place
    /// and a compiler merged their dispatches into one `jmp`, that jump can
    /// receive an entry from any of them, so the `br_table` has to cover all
    /// of them. See [`share_arm_spaces`].
    pub targets: Vec<u64>,
    /// Where the table lives.
    pub table_section: usize,
    pub table_offset: u64,
    /// How many entries this table has of its own.
    pub arms: u64,
    /// Where this table's arms begin inside [`targets`](Self::targets).
    pub arm_offset: u64,
    /// Bytes per entry, taken from the width of the entries' relocations.
    pub stride: u64,
    /// What an entry is a difference from, or `None` when it holds its
    /// target outright. See [`EntryForm::base`]; this is what decides how
    /// the entries are rewritten.
    pub base: Option<u64>,
    /// The address the dispatch's arithmetic is measured from, and therefore
    /// what it subtracts to get an arm number. The base for a table of
    /// differences; the table's own address for one of whole addresses.
    /// Meaningful for a linked input, where an address is a number; a
    /// relocatable one names its table with a relocation instead.
    pub origin: u64,
}

impl JumpTable {
    /// Whether entries hold differences rather than whole addresses.
    pub fn relative(&self) -> bool {
        self.base.is_some()
    }

    pub fn byte_length(&self) -> u64 {
        self.arms * self.stride
    }

    /// The `(section, offset)` of every entry. Their original relocations are
    /// replaced rather than translated: they name code, which has no address.
    pub fn entries(&self) -> impl Iterator<Item = (usize, u64)> + '_ {
        (0..self.arms).map(move |index| (self.table_section, self.table_offset + index * self.stride))
    }
}

/// Where a dispatch reads its table, before the table's extent is known.
struct Candidate {
    /// Index into the owning function's instructions.
    position: usize,
    table_section: usize,
    table_offset: u64,
    /// Text addresses the dispatch sequence computes, offered as bases for
    /// the relative form. Evidence rather than search: each is an address
    /// the code itself puts in a register on the way to the jump.
    bases: Vec<u64>,
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
    // Every instruction boundary in every function, by section. A table's
    // arms are blocks, and a block is not always in the function that
    // dispatches to it: gcc moves cold blocks into a `.cold` fragment of
    // their own, and the switch that reaches them still reaches them.
    // Bounding the scan to one function reads a short table, and a valid
    // index then dispatches past its end.
    let mut boundaries: std::collections::HashMap<usize, std::collections::BTreeSet<u64>> =
        std::collections::HashMap::new();
    for function in functions.iter() {
        let starts = boundaries.entry(function.section).or_default();
        for lifted in &function.instructions {
            starts.insert(lifted.offset);
        }
    }

    // Where each *body* begins — the extent a piece was cut from, not the
    // piece. This is what tells a dispatch table from an array of function
    // pointers: a function pointer names a function's start and a table
    // entry names a block inside one. Pieces are not starts in that sense,
    // because nothing outside the binary can name one.
    let mut bodies: std::collections::HashMap<usize, std::collections::BTreeSet<u64>> =
        std::collections::HashMap::new();
    for function in &object.functions {
        bodies
            .entry(function.section)
            .or_default()
            .insert(function.whole.start);
    }

    let mut candidates: Vec<(usize, Candidate)> = Vec::new();
    for (index, function) in functions.iter().enumerate() {
        for (position, lifted) in function.instructions.iter().enumerate() {
            if lifted.instruction.flow_control() != FlowControl::IndirectBranch {
                continue;
            }
            if let Some(candidate) = propose(object, function, position, &boundaries, &bodies) {
                candidates.push((index, candidate));
            }
        }
    }

    // Where the tables are, for bounding each one by the next.
    //
    // Not only the tables that were *recognised*. A dispatch whose table
    // this cannot read still reads it, and the address it reads from is
    // where some other table has to stop — otherwise the earlier table runs
    // straight through it, and the rewrite that makes the earlier dispatch
    // work overwrites entries the later one is still using. That is not a
    // surplus arm; it is a silent wrong branch in a dispatch nothing has
    // complained about.
    //
    // Measured on `/usr/bin/python3.12`: a 19-arm table in `.rodata` was
    // read as 25, swallowing the tokenizer's six-arm dispatch, whose entries
    // were then rewritten as arms of the first one's space — after which
    // `python -c 'print("hello")'` jumped into the middle of `.rodata`.
    //
    // The address is taken from the *jump's own* memory operand and not from
    // the backward scan, because a jump that reads its target out of memory
    // names the table exactly, and anything the scan turns up before it is a
    // guess that could truncate a table that is perfectly readable.
    let mut starts: Vec<(usize, u64)> = candidates
        .iter()
        .map(|(_, candidate)| (candidate.table_section, candidate.table_offset))
        .collect();
    for function in functions.iter() {
        for lifted in &function.instructions {
            if lifted.instruction.flow_control() != FlowControl::IndirectBranch {
                continue;
            }
            starts.extend(
                absolute_operands(
                    &lifted.instruction,
                    object.sections[function.section].address,
                )
                .filter_map(|address| data_at(object, address)),
            );
        }
    }
    starts.sort_unstable();
    starts.dedup();

    for (function_index, candidate) in &candidates {
        let limit = table_limit(object, &starts, candidate);
        let Some(table) = read_table(
            object,
            &functions[*function_index],
            candidate,
            limit,
            &boundaries,
        ) else {
            continue;
        };
        functions[*function_index]
            .jump_tables
            .insert(candidate.position, table);
    }

    // No two tables may describe the same bytes. Each one's entries are
    // *rewritten* so the dispatch computes an arm number, and two tables
    // overlapping means one rewrite lands on the other's entries — which
    // makes one of the two dispatches branch to the wrong arm and say
    // nothing. Bounding each table by the next is what should prevent it;
    // this is the assertion that the bounding worked, because the failure it
    // guards against has no other symptom.
    let mut extents: Vec<(usize, u64, u64)> = functions
        .iter()
        .flat_map(|function| function.jump_tables.values())
        .map(|table| {
            (
                table.table_section,
                table.table_offset,
                table.table_offset + table.byte_length(),
            )
        })
        .collect();
    extents.sort_unstable();
    extents.dedup();
    for pair in extents.windows(2) {
        let [(section, _, end), (next_section, next_start, _)] = pair else {
            continue;
        };
        if section == next_section && end > next_start {
            bail!(
                "two jump tables in {} overlap — one ends at {:#x} and the \
                 next begins at {:#x} — so rewriting the first would \
                 overwrite entries the second still dispatches through",
                object.sections[*section].name,
                object.sections[*section].address + end,
                object.sections[*section].address + next_start,
            );
        }
    }

    for function in functions.iter_mut() {
        share_arm_spaces(function);
    }

    for function in functions.iter() {
        for (position, lifted) in function.instructions.iter().enumerate() {
            if lifted.instruction.flow_control() == FlowControl::IndirectBranch
                && !function.jump_tables.contains_key(&position)
            {
                reject_unrecognised_dispatch(object, function, position, &boundaries, &bodies)?;
            }
        }
    }
    Ok(())
}

/// Gives every dispatch measuring from the same place the same arm space.
///
/// The problem this solves is a compiler optimisation, not a corner case.
/// `gcc -O2` merges identical tails, and the tail of a computed goto is
/// `jmp *%rax` — so a function with six dispatch tables can end up with one
/// jump reached from six paths, each having loaded a *different* table into
/// the register. glibc's `__vfprintf_internal` is exactly that: six
/// thirty-two-entry tables, all measured from one label, and twenty-nine
/// jumps between them.
///
/// A backward scan cannot attribute a table to such a jump, because there is
/// no single right answer — and the failure is not loud. Guessing the
/// nearest table makes the dispatch subtract the wrong address, so the arm
/// number comes out wrong: sometimes outside the table, where it falls back
/// to the exec map and reports a miss on an address that was never a
/// function; sometimes *inside* it, where it branches to the wrong arm and
/// says nothing at all.
///
/// So the arm space is a property of the origin rather than of a table.
/// Every table measuring from the same place contributes its arms to one
/// list, each dispatch branches over the whole list, and each table's
/// entries are rewritten to index into it. Which table a given jump was
/// handed then stops mattering — any of them names an arm of the same
/// space, which is precisely what the hardware was doing.
fn share_arm_spaces(function: &mut LiftedFunction) {
    use std::collections::BTreeMap;

    // Distinct tables per origin, in address order so the arm numbering is
    // deterministic across bakes.
    let mut tables: BTreeMap<u64, BTreeMap<(usize, u64), Vec<u64>>> = BTreeMap::new();
    for table in function.jump_tables.values() {
        tables
            .entry(table.origin)
            .or_default()
            .insert((table.table_section, table.table_offset), table.targets.clone());
    }

    let mut spaces: BTreeMap<u64, (Vec<u64>, BTreeMap<(usize, u64), u64>)> = BTreeMap::new();
    for (origin, by_table) in tables {
        let mut targets = Vec::new();
        let mut offsets = BTreeMap::new();
        for (where_it_is, arms) in by_table {
            offsets.insert(where_it_is, targets.len() as u64);
            targets.extend(arms);
        }
        spaces.insert(origin, (targets, offsets));
    }

    for table in function.jump_tables.values_mut() {
        let Some((targets, offsets)) = spaces.get(&table.origin) else {
            continue;
        };
        table.arm_offset = offsets[&(table.table_section, table.table_offset)];
        table.targets = targets.clone();
    }
}

/// The nearest data location the dispatch could be reading its table from.
///
/// Nearest matters: a function with two dispatches names both tables, and
/// each loads the address of its own shortly beforehand.
fn propose(
    object: &ObjectFile,
    function: &LiftedFunction,
    position: usize,
    boundaries: &std::collections::HashMap<usize, std::collections::BTreeSet<u64>>,
    bodies: &std::collections::HashMap<usize, std::collections::BTreeSet<u64>>,
) -> Option<Candidate> {
    let mut bases: Vec<u64> = Vec::new();
    for lifted in function.instructions[..=position].iter().rev() {
        // Every text address this instruction computes, kept for the entry
        // scan: a computed goto's entries are differences from one of them.
        if object.layout == Layout::Linked {
            bases.extend(
                absolute_operands(
                    &lifted.instruction,
                    object.sections[function.section].address,
                )
                .filter(|address| {
                    object
                        .section_at(*address)
                        .is_some_and(|(section, _)| {
                            object.sections[section].role == SectionRole::Text
                        })
                }),
            );
        }
        // A relocatable input names the table with a relocation; a linked
        // one has the address in the instruction, because that is what the
        // linker put there.
        let locations: Vec<(usize, u64)> = if object.layout == Layout::Linked {
            absolute_operands(
                &lifted.instruction,
                object.sections[function.section].address,
            )
            .filter_map(|address| data_at(object, address))
            .collect()
        } else {
            [lifted.displacement, lifted.immediate]
                .into_iter()
                .flatten()
                .filter_map(|reference| data_location(object, reference.symbol, reference.addend))
                .collect()
        };
        for (section, table_offset) in locations {
            if holds_block_addresses(
                object,
                function,
                section,
                table_offset,
                &bases,
                boundaries,
                bodies,
            ) {
                return Some(Candidate {
                    position,
                    table_section: section,
                    table_offset,
                    bases: bases.clone(),
                });
            }
        }
    }
    None
}

/// The absolute addresses an instruction mentions.
///
/// A table's address reaches the dispatch as a displacement — `lea rcx,
/// [rip + table]`, which is what a position-independent compilation emits —
/// or as an immediate, which is what an absolute one does. Both are offered
/// and the entry scan decides which, if either, is a table.
///
/// `section_base` is where the instruction's own section was placed, and it
/// applies to the program-counter-relative form only. The decoder runs with
/// a section-relative program counter, so such a displacement resolves to a
/// section offset rather than an address; an absolute displacement is
/// already the address and must not be moved.
fn absolute_operands(
    instruction: &iced_x86::Instruction,
    section_base: u64,
) -> impl Iterator<Item = u64> {
    let mut addresses = Vec::new();
    match instruction.memory_base() {
        iced_x86::Register::RIP => {
            addresses.push(section_base.wrapping_add(instruction.memory_displacement64()));
        }
        iced_x86::Register::None => addresses.push(instruction.memory_displacement64()),
        _ => {}
    }
    for index in 0..instruction.op_count() {
        match instruction.op_kind(index) {
            iced_x86::OpKind::Immediate32to64
            | iced_x86::OpKind::Immediate64
            | iced_x86::OpKind::Immediate32 => addresses.push(instruction.immediate(index)),
            _ => {}
        }
    }
    addresses.into_iter().filter(|address| *address != 0)
}

/// The data location a virtual address names, if it is in a data section a
/// jump table could live in.
///
/// `.got` and `.got.plt` are excluded, and by the ABI rather than by
/// heuristic: they are the linker's own indirection tables, written by the
/// loader, and a `jmp` through one is a linkage stub's dispatch. Nothing a
/// compiler emits as a `switch` lands there.
///
/// Excluding them is not merely tidiness. A `.got.plt` holds one entry per
/// stub, each pointing at `stub + 6`, so read as a table its entries are a
/// run of addresses inside the linkage section — the shape of a dispatch
/// table exactly. What makes the exclusion safe is what makes it correct:
/// a `jmp *GOT[n]` that recovers no table is translated as the indirect
/// transfer it is, which resolves through the exec map. That is the
/// cross-DSO call path the container plan calls the shadow GOT's generic
/// fallback.
fn data_at(object: &ObjectFile, address: u64) -> Option<(usize, u64)> {
    let (section, offset) = object.section_at(address)?;
    let section_data = &object.sections[section];
    if matches!(section_data.name.as_str(), ".got" | ".got.plt") {
        return None;
    }
    section_data.role.is_data().then_some((section, offset))
}

/// Whether a data location begins a run of references to blocks *inside* this
/// function — as opposed to a function pointer, which names a function's
/// start.
fn holds_block_addresses(
    object: &ObjectFile,
    function: &LiftedFunction,
    section: usize,
    offset: u64,
    bases: &[u64],
    boundaries: &std::collections::HashMap<usize, std::collections::BTreeSet<u64>>,
    bodies: &std::collections::HashMap<usize, std::collections::BTreeSet<u64>>,
) -> bool {
    if object.layout == Layout::Linked {
        // **A table entry names a block; a function pointer names a
        // function's start.** That is the whole discriminator, and the only
        // difficulty is spelling "a block".
        //
        // Written first as "the first two entries are inside the body this
        // function was cut from", it failed twice on one binary, both times
        // because gcc's hot/cold splitting puts the arms of one dispatch in
        // two bodies:
        //
        // - CPython's `opcode_targets[]` has 181 of its 256 entries inside
        //   `_PyEval_EvalFrameDefault` and 75 inside its cold twin, and
        //   entry *zero* is one of the 75.
        // - The tokenizer's six-arm dispatch at `0x51a410` has **one** entry
        //   inside its own body and five in a cold region at `0x422xxx`.
        //
        // So which entries land in the dispatching body is a property of the
        // numbering, and counting them is the wrong question. The right one
        // is asked of every entry: is it a *block* — an instruction boundary
        // that is not where some body begins? An array of function pointers
        // holds nothing but body starts, whatever else it holds, and this
        // rejects it on the first entry.
        //
        // So the test is: **two entries that are blocks, and at least one of
        // them inside the body that dispatches.** The second half ties the
        // table to *this* `switch` rather than to some other function's; a
        // dispatch every one of whose arms was moved out would be refused,
        // as a loud miss with the binary in hand, which is when to revisit
        // it.
        //
        // An entry that *is* a body start is not evidence and is not a veto
        // either. Written first as a veto, it rejected three of the four
        // dispatches in CPython's `unicode_compare_eq` — each has a cold arm
        // that gcc moved out far enough that discovery gave it a function of
        // its own, beginning exactly at the arm. An array of function
        // pointers is told apart by having *nothing but* starts, not by
        // having one.
        //
        // The scan stops where the entries stop being instruction
        // boundaries, the same bound `read_linked_table` uses.
        let whole = &object.functions[function.function].whole;
        let table_address = object.sections[section].address + offset;
        let starts = boundaries.get(&function.section);
        let begins_a_body = bodies.get(&function.section);
        return entry_forms(table_address, bases).iter().any(|form| {
            let mut blocks = 0;
            let mut ours = false;
            for index in 0.. {
                let Some(target) =
                    read_linked_entry(object, section, offset + index * form.stride, *form)
                else {
                    break;
                };
                if !starts.is_some_and(|starts| starts.contains(&target)) {
                    break;
                }
                if begins_a_body.is_some_and(|starts| starts.contains(&target)) {
                    // A body's start, which is what a function pointer names.
                    // Not evidence of a table; not evidence against one.
                    continue;
                }
                blocks += 1;
                ours |= whole.contains(&target);
                if blocks >= 2 && ours {
                    return true;
                }
            }
            false
        });
    }
    // The same rule as above, asked of relocations instead of bytes: two
    // entries that are blocks, at least one of them inside the body that
    // dispatches. A relocatable object states its stride in the relocation
    // rather than in the entry's shape, so there is one form to try rather
    // than several.
    let whole = &object.functions[function.function].whole;
    let starts = boundaries.get(&function.section);
    let begins_a_body = bodies.get(&function.section);
    let Some(first) = relocation_at(object, section, offset) else {
        return false;
    };
    let stride = first.kind.width();
    let mut blocks = 0;
    let mut ours = false;
    for index in 0.. {
        let entry = offset + index * stride;
        let Some(relocation) = relocation_at(object, section, entry) else {
            break;
        };
        let Some(target) = resolve_entry(object, relocation, entry, offset, function.section)
        else {
            break;
        };
        if !starts.is_some_and(|starts| starts.contains(&target)) {
            break;
        }
        if begins_a_body.is_some_and(|starts| starts.contains(&target)) {
            continue;
        }
        blocks += 1;
        ours |= whole.contains(&target);
        if blocks >= 2 && ours {
            return true;
        }
    }
    false
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
    boundaries: &std::collections::HashMap<usize, std::collections::BTreeSet<u64>>,
) -> Option<JumpTable> {
    if object.layout == Layout::Linked {
        // Nothing says which form the entries are in, so each is tried and
        // the longer run wins. A tie cannot happen in practice — an
        // eight-byte absolute entry read as two four-byte differences gives
        // targets that are not instruction boundaries — and if one did, the
        // first form listed is the one a non-PIE compiler used.
        let table_address =
            object.sections[candidate.table_section].address + candidate.table_offset;
        return entry_forms(table_address, &candidate.bases)
            .into_iter()
            .filter_map(|form| {
                read_linked_table(object, function, candidate, limit, form, boundaries)
            })
            .max_by_key(|table| table.targets.len());
    }
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
        let Some(target) = resolve_entry(
            object,
            relocation,
            entry_offset,
            candidate.table_offset,
            function.section,
        ) else {
            break;
        };
        // An instruction boundary anywhere in this section, not only in the
        // dispatching function — the same rule the linked reader follows,
        // and for the same reason: gcc moves a `switch`'s cold arms into a
        // fragment of their own, and bounding the scan to the dispatching
        // function reads a short table whose valid indices then dispatch
        // past its end.
        if !boundaries
            .get(&function.section)
            .is_some_and(|starts| starts.contains(&target))
        {
            break;
        }
        targets.push(target);
    }

    if targets.is_empty() {
        return None;
    }
    let table_address =
        object.sections[candidate.table_section].address + candidate.table_offset;
    Some(JumpTable {
        arms: targets.len() as u64,
        arm_offset: 0,
        targets,
        table_section: candidate.table_section,
        table_offset: candidate.table_offset,
        stride,
        // A relocatable input's relative entries are always differences
        // from the table's own address; the relocations say so.
        base: relative.then_some(table_address),
        origin: table_address,
    })
}

/// Reads a linked table in one particular form, stopping where the entries
/// stop looking like entries.
fn read_linked_table(
    object: &ObjectFile,
    function: &LiftedFunction,
    candidate: &Candidate,
    limit: u64,
    form: EntryForm,
    boundaries: &std::collections::HashMap<usize, std::collections::BTreeSet<u64>>,
) -> Option<JumpTable> {
    let elsewhere = boundaries.get(&function.section);
    let mut targets = Vec::new();
    for index in 0.. {
        let entry_offset = candidate.table_offset + index * form.stride;
        if entry_offset + form.stride > limit {
            break;
        }
        let Some(target) = read_linked_entry(object, candidate.table_section, entry_offset, form)
        else {
            break;
        };
        // An instruction boundary anywhere in this section, not only in the
        // dispatching function: an arm may land in a `.cold` fragment that
        // the symbol table calls a function of its own. What decides that
        // this *is* a table — that its first entries are blocks inside the
        // dispatching function and not its start — is asked separately, and
        // stays strict.
        let boundary = elsewhere.is_some_and(|starts| starts.contains(&target));
        if !boundary {
            break;
        }
        targets.push(target);
    }
    // One entry is a coincidence waiting to happen when the evidence is
    // bytes rather than a relocation: a single word that happens to land on
    // an instruction boundary is not a dispatch table. Two in a row is.
    let table_address =
        object.sections[candidate.table_section].address + candidate.table_offset;
    (targets.len() >= 2).then(|| JumpTable {
        arms: targets.len() as u64,
        arm_offset: 0,
        targets,
        table_section: candidate.table_section,
        table_offset: candidate.table_offset,
        stride: form.stride,
        base: form.base,
        origin: form.base.unwrap_or(table_address),
    })
}

/// The text offset one entry names, within the section that dispatches.
///
/// `in_section` is not a filter for tidiness. A recovered table's targets are
/// section offsets carried without a section — `emit_switch` reads them
/// against the dispatching function's — so an arm resolving into a *different*
/// text section (gcc's `.text.unlikely`, which is where cold blocks go in a
/// relocatable object) would have its offset read against the wrong base and
/// branch somewhere arbitrary with nothing said. Such an arm is not
/// representable, and stopping the run is the honest answer until targets
/// carry their section.
fn resolve_entry(
    object: &ObjectFile,
    relocation: &Relocation,
    entry_offset: u64,
    table_offset: u64,
    in_section: usize,
) -> Option<u64> {
    let (section, offset) = object.resolve(relocation.symbol, relocation.addend)?;
    if section != in_section || object.sections[section].role != SectionRole::Text {
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
    boundaries: &std::collections::HashMap<usize, std::collections::BTreeSet<u64>>,
    bodies: &std::collections::HashMap<usize, std::collections::BTreeSet<u64>>,
) -> Result<()> {
    if object.layout == Layout::Linked {
        // The same question, asked of bytes: does anything this dispatch
        // reads look like a table of blocks in this function? If it does and
        // the scan still rejected it, the dispatch is one we failed to
        // recover, and translating it as an indirect call would branch
        // somewhere arbitrary.
        // The same bases `propose` offers, so that this refuses exactly what
        // that failed to read rather than a different set.
        let bases: Vec<u64> = function.instructions[..=position]
            .iter()
            .flat_map(|lifted| {
                absolute_operands(
                    &lifted.instruction,
                    object.sections[function.section].address,
                )
            })
            .filter(|address| {
                object
                    .section_at(*address)
                    .is_some_and(|(section, _)| object.sections[section].role == SectionRole::Text)
            })
            .collect();
        for lifted in &function.instructions[..=position] {
            for address in absolute_operands(
                &lifted.instruction,
                object.sections[function.section].address,
            ) {
                let Some((section, offset)) = data_at(object, address) else {
                    continue;
                };
                if holds_block_addresses(
                    object, function, section, offset, &bases, boundaries, bodies,
                ) {
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
        return Ok(());
    }
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
                    resolve_entry(
                        object,
                        relocation,
                        relocation.offset,
                        offset,
                        function.section,
                    )
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

