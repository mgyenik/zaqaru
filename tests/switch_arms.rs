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

/// The path the container's program is placed at, as everywhere else.
const PROGRAM: &str = "init";

fn path(segments: &[&[u8]]) -> Vec<Vec<u8>> {
    segments.iter().map(|segment| segment.to_vec()).collect()
}

/// The fixture, read the way a bake reads it.
fn split_switch(workspace: &WorkingDirectory) -> (Vec<u8>, ObjectFile) {
    let elf = support::link_corpus_executable(workspace, "split_switch.s", "_start", "-O1");
    let bytes = std::fs::read(&elf).expect("read the program");
    let object = ObjectFile::parse(&bytes).expect("parse the program");
    (bytes, object)
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
    let (_, object) = split_switch(&workspace);

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
    let (bytes, object) = split_switch(&workspace);
    let elf = support::link_corpus_executable(&workspace, "split_switch.s", "_start", "-O1");

    let native = std::process::Command::new(&elf)
        .env_clear()
        .output()
        .expect("run the program natively");
    let native_status = native.status.code().expect("a native exit status");
    assert_eq!(
        native_status, 63,
        "natively the program no longer visits every arm; it wrote {:?}",
        native.stdout
    );

    let top = object
        .segments
        .iter()
        .map(|segment| segment.address + segment.memory_size)
        .max()
        .expect("a linked program has segments");
    let translation = zaqaru::transpile::Transpiler::new(&object)
        .translate()
        .expect("translate the program");
    let guest = workspace.write("split_switch.wasm.o", &translation.module);

    let root = workspace.path().join("image");
    std::fs::create_dir_all(&root).expect("create the image tree");
    let mut placed = bytes.clone();
    baker::program::apply(&mut placed, &translation.patches).expect("apply the patches");
    std::fs::write(root.join(PROGRAM), &placed).expect("place the program");
    let image = baker::object::emit(&baker::bake_directory(&root).expect("bake"))
        .expect("emit the image object");

    let module = support::link_container_for_program(
        &workspace,
        std::slice::from_ref(&guest),
        &image,
        "switch-arms",
        Some(top),
    );
    let mut container = runner::Container::instantiate(
        &std::fs::read(&module).expect("read the container"),
        support::mounts_seeded(&[0x33; 32]),
    )
    .expect("instantiate the container");

    let status = container.boot().unwrap_or_else(|error| {
        let log = container
            .mounts()
            .read(&path(&[b"iso", b"log", b"error"]))
            .ok()
            .flatten()
            .unwrap_or_default();
        panic!(
            "the container did not finish: {error:?}\nkernel log: {}",
            String::from_utf8_lossy(&log)
        )
    });

    let written = container
        .mounts()
        .read(&path(&[b"iso", b"console", b"stdout"]))
        .expect("the console mount failed")
        .unwrap_or_default();
    assert_eq!(
        written, native.stdout,
        "the transpiled program reached different arms than the native one"
    );
    assert_eq!(
        i64::from(status),
        i64::from(native_status),
        "the transpiled program's arms summed differently"
    );
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
