//! D7: a `switch` arm that lies outside the piece that dispatches to it.
//!
//! The front end is a fixpoint — discover, lift, recover, discover again —
//! because the last pass produces evidence the first one needed. A recovered
//! table's arm is a place the dispatch provably transfers to; when the body
//! was cut at a direct branch into its middle, some arms land in a sibling
//! piece with nothing beginning at them, and a `br_table` cannot branch into
//! another function. See `crate::frontend` and `docs/code-discovery.md`.
//!
//! What is checked here is the whole claim in three parts: that the cut
//! happens and says what made it, that the program it unblocks computes what
//! the same program computes natively, and — the half that keeps the design
//! safe — that a `switch` whose arms all sit in one piece feeds nothing back
//! at all.

mod support;

use support::WorkingDirectory;
use zaqaru::discover::{Extent, Witness};
use zaqaru::reader::ObjectFile;

/// A fixture, read the way a bake reads it.
fn read(workspace: &WorkingDirectory, source: &str) -> (std::path::PathBuf, Vec<u8>, ObjectFile) {
    let elf = support::link_corpus_executable(workspace, source, "_start", "-O1");
    let bytes = std::fs::read(&elf).expect("read the program");
    let object = ObjectFile::parse(&bytes).expect("parse the program");
    (elf, bytes, object)
}

/// A function begins at every arm that needed one, and says why it does.
///
/// The negative control is the witness itself. `Witness::SwitchArm` is
/// assigned nowhere but the forced cut, so a piece carrying it is a piece
/// that would not exist without the feedback — and without it the structurer
/// has no function to tail-call and emits `unreachable`, which is what
/// stopped CPython.
#[test]
fn an_arm_in_a_sibling_piece_becomes_a_function() {
    let workspace = WorkingDirectory::new("switch-arms-cut");
    let (_, _, object) = read(&workspace, "split_switch.s");

    // The fixture has to be the shape it claims to be: one recovered table,
    // whose arms are not all inside the piece that dispatches.
    let lifted = zaqaru::lifter::lift_object(&object).expect("lift");
    let tables: Vec<_> = lifted
        .iter()
        .flat_map(|function| function.jump_tables.values())
        .collect();
    assert_eq!(
        tables.len(),
        1,
        "the fixture no longer has exactly one recovered dispatch"
    );
    assert_eq!(tables[0].targets.len(), 4, "the table lost arms");

    let cut: Vec<&zaqaru::discover::Function> = object
        .functions
        .iter()
        .filter(|function| function.witness == Witness::SwitchArm)
        .collect();
    assert_eq!(
        cut.len(),
        2,
        "expected the two arms past the cut to become functions; found {:?}",
        cut.iter().map(|function| &function.name).collect::<Vec<_>>()
    );

    // Each one begins exactly at an arm, which is the property the tail call
    // the structurer emits depends on.
    let text = &object.sections[cut[0].section];
    for function in &cut {
        assert!(
            tables[0].targets.contains(&function.offset),
            "a `SwitchArm` piece does not begin at an arm"
        );
        assert_eq!(function.section, cut[0].section);
    }

    // And it cut a *stated* extent — `dispatch` states its own size. That is
    // the evidence-grade claim made checkable: a recovered arm carries the
    // standing of a proven transfer, not of a weak witness, which may only
    // revise a guess.
    let dispatch = object
        .functions
        .iter()
        .find(|function| function.name == "dispatch")
        .expect("the fixture defines `dispatch`");
    assert_eq!(
        dispatch.extent,
        Extent::Stated,
        "the fixture stopped stating `dispatch`'s size, so this proves nothing"
    );
    assert!(
        cut.iter()
            .all(|function| function.whole == dispatch.whole),
        "the pieces were cut out of some other body than `dispatch`"
    );
    assert!(
        text.address + cut[0].offset > text.address,
        "the arms are at the section's very start, which cannot be right"
    );

    // Nothing is left stranded: the fixpoint settled rather than stopping.
    assert!(
        zaqaru::frontend::stranded_arms(&object, &lifted).is_empty(),
        "an arm still has no function to begin at it"
    );
}

/// And the program runs, against the only oracle worth having: the same
/// bytes executed by Linux.
#[test]
fn a_program_whose_switch_arms_were_split_away_runs() {
    let workspace = WorkingDirectory::new("switch-arms-run");
    let status = support::program_agrees_with_native(&workspace, "split_switch.s", "switch-arms");
    assert_eq!(status, 63, "the program no longer reaches every arm");
}

/// A table whose *first* entry is in the cold body is still a table.
///
/// The discriminator between a dispatch table and an array of function
/// pointers is where the entries land — a function pointer names a
/// function's start, a table entry names a block inside the body that
/// dispatches. That was written as "the first two entries", and gcc says the
/// arms of one dispatch land in two bodies, because it splits cold blocks
/// out into a body of their own. CPython's `opcode_targets[]` has 181 of its
/// 256 entries inside `_PyEval_EvalFrameDefault` and 75 inside its cold
/// twin, and entry zero is one of the 75 — so the hottest dispatch in the
/// milestone target was not recognised at all, and translated as an indirect
/// transfer that missed the first time a bytecode ran.
#[test]
fn a_table_whose_first_arm_is_in_the_cold_body_is_still_recovered() {
    let workspace = WorkingDirectory::new("switch-arms-cold");
    let (_, _, object) = read(&workspace, "cold_switch.s");

    let lifted = zaqaru::lifter::lift_object(&object).expect("lift");
    let tables: Vec<_> = lifted
        .iter()
        .flat_map(|function| function.jump_tables.values())
        .collect();
    assert_eq!(
        tables.len(),
        1,
        "the dispatch was not recovered, so its first entry is still deciding"
    );

    // The fixture is the shape it claims to be: the first entry is outside
    // the body that dispatches, and the ones after it are inside.
    let dispatch = object
        .functions
        .iter()
        .find(|function| function.name == "dispatch")
        .expect("the fixture defines `dispatch`");
    let arms = &tables[0].targets;
    assert!(
        !dispatch.whole.contains(&arms[0]),
        "the first arm is inside the dispatching body, so this proves nothing"
    );
    assert!(
        arms[1..]
            .iter()
            .all(|arm| dispatch.whole.contains(arm)),
        "the later arms are not inside the dispatching body"
    );

    // And the arm in the cold body became a function, since nothing else
    // begins there — the fixpoint's work, on a body it does not dispatch
    // from.
    let cut = object
        .functions
        .iter()
        .find(|function| function.witness == Witness::SwitchArm)
        .expect("the cold arm did not become a function");
    assert_eq!(cut.offset, arms[0], "the piece does not begin at the arm");
}

#[test]
fn a_program_dispatching_into_its_cold_body_runs() {
    let workspace = WorkingDirectory::new("switch-arms-cold-run");
    let status = support::program_agrees_with_native(&workspace, "cold_switch.s", "cold-switch");
    assert_eq!(status, 31, "the program no longer reaches every arm");
}

/// The trap this design exists to leave closed.
///
/// CPython's `opcode_targets[]` is ~256 addresses that all land inside
/// `_PyEval_EvalFrameDefault`, and a discovery pass that let them define
/// boundaries would shred the hottest function in the milestone target. Under
/// the fixpoint they are a non-event, and the reason is precisely stateable:
/// the feedback set is *arms outside the piece that dispatches*, and an arm
/// inside it is a `br_table` arm. So a compiled `switch` nothing has split
/// contributes nothing to feed back, and the assertion is on that set rather
/// than on the bytes — it says why, where byte-identity would only say that.
#[test]
fn a_switch_whose_arms_stay_in_one_piece_feeds_nothing_back() {
    for optimisation in ["-O1", "-O2"] {
        let workspace = WorkingDirectory::new(&format!("switch-arms-whole{optimisation}"));
        let elf = support::link_corpus_executable(
            &workspace,
            "switch_dispatch.c",
            "classify",
            optimisation,
        );
        let bytes = std::fs::read(&elf).expect("read");
        let object = ObjectFile::parse(&bytes).expect("parse");
        let lifted = zaqaru::lifter::lift_object(&object).expect("lift");

        let tables: usize = lifted
            .iter()
            .map(|function| function.jump_tables.len())
            .sum();
        assert!(
            tables > 0,
            "[{optimisation}] no table was recovered, so this proves nothing"
        );
        assert!(
            zaqaru::frontend::stranded_arms(&object, &lifted).is_empty(),
            "[{optimisation}] a switch nothing split fed arms back"
        );
        assert!(
            !object
                .functions
                .iter()
                .any(|function| function.witness == Witness::SwitchArm),
            "[{optimisation}] the fixpoint cut a function it had no evidence to cut"
        );
    }
}
