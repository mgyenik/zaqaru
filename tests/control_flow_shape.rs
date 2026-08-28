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

/// A call that never comes back ends the function, and the graph says so by
/// handing the question on.
///
/// A compiler that knows a callee is `noreturn` emits nothing after the
/// call — no `ret`, no jump. The function's last byte is the call's last
/// byte. Reading that as a fall-through asks where control goes next and
/// finds no block there, which refused the function.
///
/// The answer is one of two things and the graph cannot tell which: either
/// another function begins where this one ends and control continues into
/// it, or nothing does and there is no path past the call. It sees one
/// function and the question is about the others, so it reports
/// `FallsOut` and the translator — which knows where every function begins
/// — decides. Here nothing follows, so the translation is a trap.
///
/// This is not a corner. gcc splits every cold path into a fragment ending
/// this way, and a static glibc `hello` had 332 functions refused for it —
/// nearly a third of the binary.
#[test]
fn a_function_ending_in_a_call_that_never_returns_stops_there() {
    use zaqaru::cfg::Terminator;

    let workspace = WorkingDirectory::new("cfg-noreturn");
    let object_path = compile_corpus_object(&workspace, "noreturn_call.c");
    let bytes = std::fs::read(&object_path).expect("read compiled object");
    let object = ObjectFile::parse(&bytes).expect("parse object");
    let functions = lifter::lift_object(&object).expect("lift object");

    let give_up = functions
        .iter()
        .find(|function| function.name == "give_up")
        .expect("the fixture no longer defines `give_up`");
    let graph = ControlFlowGraph::build(give_up).expect("build the graph");

    // The premise: the function really does end with its call.
    let last = give_up
        .instructions
        .last()
        .expect("`give_up` has instructions");
    assert_eq!(
        last.instruction.flow_control(),
        iced_x86::FlowControl::Call,
        "`give_up` no longer ends in a call, so this proves nothing"
    );

    let end = graph.blocks.last().expect("a graph has blocks");
    assert_eq!(
        end.terminator,
        Terminator::FallsOut {
            into: give_up.offset + give_up.size
        },
        "the block that runs off the end of the function does not say so"
    );
    // And the call itself is still translated: it is an ordinary instruction
    // that happens to be last, not a terminator that gets special handling.
    assert_eq!(
        end.body_instructions().end,
        end.instructions.end,
        "the call was treated as a terminator and dropped from the body"
    );
    assert!(graph.successors(graph.blocks.len() - 1).is_empty());

    // A function with one such path among others still translates whole.
    assert!(
        functions.iter().any(|function| function.name == "checked"),
        "the fixture no longer defines `checked`"
    );
    zaqaru::transpile::Transpiler::new(&object)
        .transpile()
        .expect("translating a function whose last instruction never returns");
}

/// `hlt` ends its block wherever it stands, not only at a function's end.
///
/// A compiler emits one where control provably does not reach — after a call
/// that never returns — and six of the seven in a static glibc are
/// *mid*-function, with more code after them. `abort`, `_Exit` and
/// `__libc_check_standard_fds` all carry one. Reading it as a fall-through
/// describes a path into the following block that cannot be taken, and at
/// the end of a function it describes a path off the end entirely, which is
/// what refused `_start`.
#[test]
fn an_instruction_that_never_continues_ends_its_block() {
    use zaqaru::cfg::Terminator;

    let workspace = WorkingDirectory::new("cfg-halting");
    let object_path = compile_corpus_object(&workspace, "halting.s");
    let bytes = std::fs::read(&object_path).expect("read compiled object");
    let object = ObjectFile::parse(&bytes).expect("parse object");
    let functions = lifter::lift_object(&object).expect("lift object");

    for name in ["halts_at_the_end", "halts_in_the_middle"] {
        let function = functions
            .iter()
            .find(|function| function.name == name)
            .unwrap_or_else(|| panic!("the fixture no longer defines `{name}`"));
        let graph = ControlFlowGraph::build(function).expect("build the graph");

        let halting = graph
            .blocks
            .iter()
            .enumerate()
            .find(|(_, block)| {
                function.instructions[block.instructions.end - 1]
                    .instruction
                    .mnemonic()
                    == iced_x86::Mnemonic::Hlt
            })
            .map(|(index, block)| (index, block.terminator.clone()))
            .unwrap_or_else(|| panic!("no block in `{name}` ends in `hlt`"));

        assert_eq!(
            halting.1,
            Terminator::Unreachable,
            "`{name}`'s halting block does not stop there"
        );
        assert!(
            graph.successors(halting.0).is_empty(),
            "`{name}` continues past a `hlt`"
        );
    }

    // The mid-function case has reachable code after the halt, which must
    // still be there — it is the branch's target, not the halt's successor.
    let middle = functions
        .iter()
        .find(|function| function.name == "halts_in_the_middle")
        .expect("the fixture");
    let graph = ControlFlowGraph::build(middle).expect("build the graph");
    assert!(
        graph.blocks.len() >= 3,
        "the block after the halt went missing: {} blocks",
        graph.blocks.len()
    );

    zaqaru::transpile::Transpiler::new(&object)
        .transpile()
        .expect("translating functions that halt");
}
