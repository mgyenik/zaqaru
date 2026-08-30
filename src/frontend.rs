//! The front end, run to a fixpoint: discover → lift → recover → discover.
//!
//! Discovery decides where functions begin; lifting decodes them; jump-table
//! recovery reads the dispatches out of what was decoded. Written as a
//! one-way pipeline that is an ordering bug waiting to happen, because the
//! last stage produces evidence the first stage needed:
//!
//! A `switch` compiles to a table of *code addresses*, and the only way to
//! translate one is a `br_table` over an arm number — which requires every
//! arm to be somewhere the translation can name. An arm inside the piece
//! that dispatches is a block, and a `br_table` reaches it. An arm outside
//! it has to be a function, because a wasm branch cannot enter another
//! function's interior; [`crate::structurer`] emits such an arm as a tail
//! call, and [`crate::translate::FunctionTranslator::emit_fall_out`] emits
//! `unreachable` when nothing begins there.
//!
//! Nothing begins there whenever the body was cut at a direct branch into
//! its middle, because splitting cuts on direct branches and an arm is an
//! *indirect* target: nothing knows it is an arm until recovery, and
//! recovery needs the cutting to have happened. CPython's `_Py_HashBytes`
//! is siphash, its tail is `switch (len & 7)`, and two of its eight arms
//! land in a sibling piece — so the interpreter traps on the first string
//! it hashes.
//!
//! The resolution is to state what was always true: **discovery is not
//! finished until the evidence the later passes produce stops arriving.**
//! This module owns that loop. It is the same shape as the two fixpoints
//! already inside the front end — the poisoned-offset decode loop in
//! [`crate::lifter`], the transfer-target rounds in [`crate::discover`] —
//! drawn around the whole instead of around each part.
//!
//! Three rules make it safe, and each is load-bearing:
//!
//! - **The evidence grade.** A recovered table's arm is not a heuristic:
//!   the dispatch provably transfers there. That is the grade a direct
//!   branch target has — the grade that created these pieces in the first
//!   place — so an arm may cut a *stated* extent, and the invariant of
//!   `docs/code-discovery.md` is untouched. This is not a weak witness
//!   gaining a permission; it is a proven transfer acquiring the standing
//!   proven transfers already have.
//! - **The feedback set.** Only arms *outside the dispatching piece* and
//!   *not already a function start* feed back — exactly the set the
//!   structurer would otherwise mis-emit, and nothing more. This is what
//!   keeps the concrete trap closed: `_PyEval_EvalFrameDefault`'s 256
//!   computed-goto labels are all inside one piece, because nothing ever
//!   split the eval loop, so they are `br_table` arms, never fed back, and
//!   the function is never shredded.
//! - **Cuts go through the splitter.** [`crate::discover::cut_at_switch_arms`]
//!   is the door; there is no private cut pass. Evidence that bypasses the
//!   splitter leaves the splitter wrong about the world.
//!
//! What is *not* re-run is witness collection, and the reason is a fact
//! rather than a saving: an arm can never establish a function. Every arm
//! is an instruction boundary of some already-discovered function, because
//! that is what [`crate::jump_table`] checks before it will read a table at
//! all — so an arm always lands inside something, and cutting is the only
//! act available. Re-running the strong and weak witnesses would produce
//! the same functions and, after a merge, would need per-module bases the
//! merged object no longer separates.

use anyhow::{Result, bail};

use crate::lifter::{self, LiftedFunction};
use crate::reader::{Layout, ObjectFile};

/// How many times the loop may go round.
///
/// Each round can only cut, and a cut only subdivides, so the sequence is
/// strictly decreasing in the number of arms that lie outside their piece.
/// Two rounds is what the case that forced this needs. The bound is here to
/// make a wrong belief about that loud rather than to run out.
const ROUNDS: usize = 8;

/// Runs the front end to a fixpoint, leaving `object.functions` final.
///
/// Called from [`ObjectFile::parse_at`], which is what makes it impossible
/// to hold an object whose function list a later pass would still revise.
pub fn settle(object: &mut ObjectFile) -> Result<()> {
    let mut recovered: Option<std::collections::BTreeSet<(usize, u64)>> = None;

    for round in 0..=ROUNDS {
        let lifted = match lifter::lift_object(object) {
            Ok(lifted) => lifted,
            // On the first round, an object that cannot be lifted is not
            // this pass's business: there is no feedback to collect, and the
            // failure belongs to whoever actually asked for the lifting,
            // with the context they can give it. Reporting it here would
            // move every decode and dispatch error from translation into
            // `parse`, which is a different tool's error message.
            Err(_) if round == 0 => return Ok(()),
            // Later, it is exactly this pass's business: the object lifted
            // before the cut and does not lift after it.
            Err(error) => {
                return Err(error.context(
                    "lifting after cutting functions at the arms of a recovered \
                     `switch` — the cut broke a decode that worked before it",
                ));
            }
        };

        // The monotonicity guard. A cut lands between a dispatch's `lea` and
        // its `jmp`, and the backward scan that attributes a table to that
        // jump no longer reaches it — so a table recovered in one round is
        // unrecoverable in the next, and the dispatch degrades to an
        // exec-map lookup that misses at run time saying nothing about why.
        // The set may grow and may not shrink; if it shrinks, that is a
        // build-time error naming the dispatch.
        let now = dispatches(&lifted);
        if let Some(before) = &recovered {
            still_recovered(&object.sections, before, &now)?;
        }
        recovered = Some(now);

        let arms = stranded_arms(object, &lifted);
        if arms.is_empty() {
            return Ok(());
        }
        if round == ROUNDS {
            bail!(
                "the front end did not settle in {ROUNDS} rounds: {} `switch` \
                 arms still lie outside the piece that dispatches to them, \
                 the first at {:#x}",
                arms.len(),
                arms.first().copied().unwrap_or_default()
            );
        }
        if !crate::discover::cut_at_switch_arms(
            &object.sections,
            object.layout,
            &arms,
            &mut object.functions,
        )? {
            // Cutting is supposed to be always available: an arm is an
            // instruction boundary of some discovered function, so something
            // contains it. If nothing does, the belief is wrong and saying
            // so beats looping to the round limit with a vaguer message.
            bail!(
                "the `switch` arm at {:#x} lies outside the piece that \
                 dispatches to it and inside no function that can be cut at \
                 it, so no function can be made to begin there",
                arms.first().copied().unwrap_or_default()
            );
        }
    }
    Ok(())
}

/// The monotonicity guard: a round may recover more dispatches and may not
/// recover fewer.
///
/// The pathological case a fixpoint over cutting has, and the reason the
/// bound is a set rather than a count. A cut lands between a dispatch's
/// table load and its jump; the backward scan that attributes the table to
/// that jump no longer reaches it; and a table recovered in one round is
/// unrecoverable in the next. Nothing about that is loud on its own — the
/// dispatch simply degrades into an indirect transfer through the exec map,
/// which misses at run time on an address that was never a function and says
/// nothing about why. So it is made loud here, at build time, naming the
/// dispatch.
fn still_recovered(
    sections: &[crate::reader::Section],
    before: &std::collections::BTreeSet<(usize, u64)>,
    now: &std::collections::BTreeSet<(usize, u64)>,
) -> Result<()> {
    let Some((section, offset)) = before.difference(now).next() else {
        return Ok(());
    };
    let address = sections
        .get(*section)
        .map(|section| section.address + offset)
        .unwrap_or(*offset);
    bail!(
        "the dispatch at {address:#x} was recovered before functions were cut \
         at the arms of a recovered `switch` and is not recovered after — the \
         cutting has made its table unreadable, which would leave it \
         dispatching through the exec map and missing at run time"
    );
}

/// Every recovered dispatch, by where its jump is. Section offsets, which a
/// cut does not move — unlike a function index or a position within one.
fn dispatches(lifted: &[LiftedFunction]) -> std::collections::BTreeSet<(usize, u64)> {
    lifted
        .iter()
        .flat_map(|function| {
            function.jump_tables.keys().map(|position| {
                (
                    function.section,
                    function.instructions[*position].offset,
                )
            })
        })
        .collect()
}

/// The arms that need a function to begin at them and have none: virtual
/// addresses, in order.
///
/// The set the whole design turns on, and public so that a test can say so
/// directly. An arm inside the dispatching piece is a `br_table` arm and is
/// not here — which is why an unsplit function's computed goto, however many
/// labels it has, contributes nothing, and why
/// `_PyEval_EvalFrameDefault`'s opcode table cannot shred the eval loop
/// through this door. Empty means the fixpoint changed nothing, which is a
/// stronger statement than byte-identical output because it says why.
pub fn stranded_arms(
    object: &ObjectFile,
    lifted: &[LiftedFunction],
) -> std::collections::BTreeSet<u64> {
    if object.layout != Layout::Linked {
        // A relocatable object's arms are named by relocations against the
        // text section, and an arm that leaves the function is a symbol the
        // assembler emitted — gcc's cold-block split. There is nothing for a
        // cut to add, and the addresses this works in are not addresses.
        return std::collections::BTreeSet::new();
    }
    let starts: std::collections::HashSet<(usize, u64)> = object
        .functions
        .iter()
        .map(|function| (function.section, function.offset))
        .collect();
    let mut stranded = std::collections::BTreeSet::new();
    for function in lifted {
        for table in function.jump_tables.values() {
            for target in &table.targets {
                if function.contains(*target) || starts.contains(&(function.section, *target)) {
                    continue;
                }
                stranded.insert(object.sections[function.section].address + target);
            }
        }
    }
    stranded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{Section, SectionRole};

    fn section(address: u64) -> Section {
        Section {
            name: ".text".into(),
            role: SectionRole::Text,
            address,
            bytes: Vec::new(),
            size: 0x1000,
            alignment: 16,
            relocations: Vec::new(),
        }
    }

    #[test]
    fn a_round_may_recover_more_dispatches() {
        let sections = [section(0x400000)];
        let before = std::collections::BTreeSet::from([(0, 0x10)]);
        let now = std::collections::BTreeSet::from([(0, 0x10), (0, 0x40)]);
        assert!(still_recovered(&sections, &before, &now).is_ok());
        assert!(still_recovered(&sections, &before, &before).is_ok());
    }

    /// The guard, and the reason it names an address rather than a count: a
    /// dispatch that stops being recovered is a runtime miss with nothing to
    /// point at, so the build-time refusal has to say which one.
    #[test]
    fn a_round_that_loses_a_dispatch_is_refused_by_address() {
        let sections = [section(0x400000)];
        let before = std::collections::BTreeSet::from([(0, 0x10), (0, 0x40)]);
        let now = std::collections::BTreeSet::from([(0, 0x10)]);
        let error = still_recovered(&sections, &before, &now)
            .expect_err("losing a dispatch was accepted");
        let report = format!("{error:#}");
        assert!(
            report.contains("0x400040"),
            "the refusal did not name the dispatch it lost: {report}"
        );
    }
}
