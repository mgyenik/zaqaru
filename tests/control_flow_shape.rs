//! What shape the structurer sees, and which translation it therefore picks.
//!
//! The differential tests prove both modes compute the right answers, but not
//! that the structured mode is ever *used*: a structurer that quietly fell
//! back to the dispatcher everywhere would pass them all. These tests pin the
//! classification down directly.

mod support;

use support::{WorkingDirectory, compile_corpus_object};
use zaqaru::cfg::{ControlFlowGraph, Dominators, LoopStructure};
use zaqaru::lifter;
use zaqaru::reader::ObjectFile;

/// Reports, per function, whether its control-flow graph is reducible and how
/// many basic blocks it has.
fn classify(source: &str) -> Vec<(String, bool, usize)> {
    let workspace = WorkingDirectory::new("cfg-shape");
    let object_path = compile_corpus_object(&workspace, source);
    let bytes = std::fs::read(&object_path).expect("read compiled object");
    let object = ObjectFile::parse(&bytes).expect("parse object");

    lifter::lift_object(&object)
        .expect("lift object")
        .iter()
        .map(|function| {
            let graph = ControlFlowGraph::build(function).expect("build control-flow graph");
            let dominators = Dominators::compute(&graph);
            let loops = LoopStructure::analyse(&graph, &dominators);
            (function.name.clone(), loops.reducible, graph.blocks.len())
        })
        .collect()
}

/// Everything a compiler emits from C should be reducible, and the loops in
/// the corpus should really be loops — otherwise the structured translation
/// is never exercised.
#[test]
fn compiled_corpus_is_reducible() {
    for source in [
        "add.c",
        "control_flow.c",
        "arithmetic.c",
        "calls.c",
        "data.c",
        "division.c",
        "function_pointers.c",
        "switch_dispatch.c",
        "interpreter.c",
        "two_functions.c",
    ] {
        for (name, reducible, blocks) in classify(source) {
            assert!(
                reducible,
                "`{name}` in {source} was classified as irreducible, so the \
                 structured translation silently falls back to the dispatcher"
            );
            let _ = blocks;
        }
    }

    let shapes = classify("control_flow.c");
    let gcd = shapes
        .iter()
        .find(|(name, _, _)| name == "gcd")
        .expect("`gcd` is in the control-flow corpus");
    assert!(
        gcd.2 >= 4,
        "`gcd` collapsed to {} basic block(s); it is supposed to be a loop \
         with a two-way body",
        gcd.2
    );
}

/// The hand-written irreducible case: a loop entered at either of two blocks,
/// which no nesting of `block` and `loop` can express.
#[test]
fn a_hand_written_irreducible_graph_is_detected() {
    let shapes = classify("irreducible.s");
    let (name, reducible, blocks) = shapes
        .iter()
        .find(|(name, _, _)| name == "irreducible")
        .expect("the corpus defines `irreducible`");
    assert!(
        !reducible,
        "`{name}` has {blocks} blocks and was classified as reducible, but its \
         loop has two entry points"
    );
}

/// Loop headers and merge points are what the structured translation nests
/// around; a graph where neither is found would emit nothing but straight
/// line code.
#[test]
fn a_loop_is_recognised_as_one() {
    let workspace = WorkingDirectory::new("cfg-loops");
    let object_path = compile_corpus_object(&workspace, "control_flow.c");
    let bytes = std::fs::read(&object_path).expect("read compiled object");
    let object = ObjectFile::parse(&bytes).expect("parse object");
    let functions = lifter::lift_object(&object).expect("lift object");

    let gcd = functions
        .iter()
        .find(|function| function.name == "gcd")
        .expect("`gcd` is in the corpus");
    let graph = ControlFlowGraph::build(gcd).expect("build control-flow graph");
    let dominators = Dominators::compute(&graph);
    let loops = LoopStructure::analyse(&graph, &dominators);

    assert!(
        loops.is_loop_header.iter().any(|header| *header),
        "no loop header found in `gcd`, which is a loop"
    );
    assert!(
        loops.is_merge_point.iter().any(|merge| *merge),
        "no merge point found in `gcd`, whose two-way body rejoins"
    );
    assert!(
        (0..graph.blocks.len()).all(|block| dominators.is_reachable(block)),
        "`gcd` has an unreachable basic block"
    );
}
