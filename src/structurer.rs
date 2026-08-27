//! Expressing an x86 control-flow graph in WebAssembly's structured control
//! flow.
//!
//! Two modes, both running over the same instruction translation so that one
//! can act as the other's oracle:
//!
//! 1. the **dispatcher** — a `loop` around a `br_table` over a block-index
//!    local — which is correct for any graph, reducible or not, and needs no
//!    graph theory at all;
//! 2. the **structured** translation, which follows the dominator tree to
//!    produce the `block`/`loop`/`if` nesting a reader would expect.
//!
//! The dispatcher is not a stepping stone that gets thrown away: it stays as
//! the fallback for irreducible graphs and as the oracle the structured mode
//! is checked against.
//!
//! The structured translation is the algorithm from Norman Ramsey's *"Beyond
//! Relooper: recursive translation of unstructured control flow to structured
//! control flow"* (ICFP 2022). Each node of the dominator tree is emitted
//! inside a `loop` if it is a loop header, wrapped in one `block` per merge
//! point it dominates; a branch to a merge point or backwards becomes a `br`
//! to the matching context entry, and any other branch inlines its target's
//! subtree.

use anyhow::Result;

use crate::cfg::{ControlFlowGraph, Dominators, LoopStructure, Terminator};
use crate::emitter::ValueType;
use crate::emitter::code::FunctionBodyBuilder;
use crate::lifter::LiftedFunction;
use crate::translate::FunctionTranslator;

/// Which control-flow translation to use.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// The dominator-based structured translation, falling back to the
    /// dispatcher for graphs it cannot express.
    #[default]
    Structured,
    /// The universal `br_table` dispatcher, always.
    Dispatcher,
}

/// Translates a whole function body.
pub fn translate_function(
    body: &mut FunctionBodyBuilder,
    translator: &mut FunctionTranslator<'_>,
    lifted: &LiftedFunction,
    mode: Mode,
) -> Result<()> {
    let graph = ControlFlowGraph::build(lifted)?;

    if mode == Mode::Structured {
        let dominators = Dominators::compute(&graph);
        let loops = LoopStructure::analyse(&graph, &dominators);
        // An irreducible graph has a loop with more than one entry, which no
        // nesting of `block` and `loop` can express; the dispatcher can.
        if loops.reducible
            && graph
                .blocks
                .iter()
                .enumerate()
                .all(|(index, _)| dominators.is_reachable(index))
        {
            let mut emitter = StructuredEmitter {
                graph: &graph,
                dominators: &dominators,
                loops: &loops,
                lifted,
                context: Vec::new(),
            };
            return emitter.emit_subtree(body, translator, 0);
        }
    }

    translate_with_dispatcher(body, translator, lifted, &graph)
}

/// Emits a block's instructions and, where the block simply runs into the
/// next one, nothing else. Terminating transfers are the caller's business.
fn emit_block_instructions(
    body: &mut FunctionBodyBuilder,
    translator: &mut FunctionTranslator<'_>,
    lifted: &LiftedFunction,
    block: usize,
    graph: &ControlFlowGraph,
) -> Result<()> {
    for position in graph.blocks[block].body_instructions() {
        translator.translate_instruction(body, &lifted.instructions[position])?;
    }
    Ok(())
}

/// Emits the `br_table` a recovered `switch` becomes.
///
/// Every arm has to be a branch — a `br_table` cannot inline a target the way
/// a two-way branch can — so the table is wrapped in one `block` per arm plus
/// one for the default, and each arm's landing point performs whatever
/// transfer the mode uses. The guest's own bounds check runs before the
/// dispatch, so the default arm is unreachable; it traps rather than
/// pretending otherwise.
fn emit_switch(
    body: &mut FunctionBodyBuilder,
    translator: &mut FunctionTranslator<'_>,
    lifted: &LiftedFunction,
    block: usize,
    graph: &ControlFlowGraph,
    mut transfer: impl FnMut(&mut FunctionBodyBuilder, u64, u32) -> Result<()>,
) -> Result<()> {
    let position = graph.blocks[block]
        .terminating_instruction()
        .expect("a switch ends with its dispatching jump");
    let table = lifted
        .jump_tables
        .get(&position)
        .expect("a switch terminator comes from a recovered jump table");
    let targets = &table.targets;
    let arms = targets.len();

    for _ in 0..=arms {
        body.block();
    }
    translator.emit_switch_index(body, &lifted.instructions[position], table)?;
    let depths: Vec<u32> = (0..arms as u32).collect();
    body.branch_table(&depths, arms as u32);
    body.end();

    for (arm, target) in targets.iter().enumerate() {
        // Inside arm `k` sit the arms after it plus the default block.
        transfer(body, *target, (arms - arm) as u32)?;
        body.end();
    }
    body.unreachable();
    Ok(())
}

/// Emits the end of a block that leaves the function: a `ret`, or a tail jump
/// to another function.
fn emit_leaving(
    body: &mut FunctionBodyBuilder,
    translator: &mut FunctionTranslator<'_>,
    lifted: &LiftedFunction,
    block: usize,
    graph: &ControlFlowGraph,
) -> Result<()> {
    let index = graph.blocks[block]
        .terminating_instruction()
        .expect("a block that leaves the function ends with an instruction that does so");
    let instruction = &lifted.instructions[index];
    if instruction.instruction.flow_control() == iced_x86::FlowControl::Return {
        translator.emit_return(body);
    } else {
        translator.emit_tail_call(body, instruction)?;
    }
    body.return_();
    Ok(())
}

/// Emits a conditional tail call: the transfer happens only when the branch
/// is taken, and control otherwise continues after it.
fn emit_conditional_leave(
    body: &mut FunctionBodyBuilder,
    translator: &mut FunctionTranslator<'_>,
    lifted: &LiftedFunction,
    block: usize,
    graph: &ControlFlowGraph,
) -> Result<()> {
    let index = graph.blocks[block]
        .terminating_instruction()
        .expect("a conditional leave ends with its branch");
    let instruction = &lifted.instructions[index];
    translator.branch_condition(body, instruction)?;
    body.if_();
    translator.emit_tail_call(body, instruction)?;
    body.return_();
    body.end();
    Ok(())
}

// ---- the dispatcher ------------------------------------------------------

/// The dispatcher's shape, for `n` blocks:
///
/// ```text
/// loop                        ;; re-entered on every transfer
///   block                     ;; default: an unreachable state
///     block                   ;; block n-1
///       ...
///         block               ;; block 0, innermost
///           local.get state
///           br_table 0 1 ... n-1, default n
///         end                 ;; landing point for block 0
///         <block 0>
///       end                   ;; landing point for block 1
///       <block 1>
///     ...
///   end
///   unreachable               ;; only reachable from an invalid state
/// end
/// ```
///
/// Every block body ends by setting `state` and branching back to the loop,
/// or by returning, so control never falls from one body into the next.
fn translate_with_dispatcher(
    body: &mut FunctionBodyBuilder,
    translator: &mut FunctionTranslator<'_>,
    lifted: &LiftedFunction,
    graph: &ControlFlowGraph,
) -> Result<()> {
    let block_count = graph.blocks.len();
    let state = body.declare_local(ValueType::I32);

    // The entry block is the one at the lowest offset, which is index 0, and
    // a fresh local already holds 0.
    body.loop_();
    for _ in 0..=block_count {
        body.block();
    }
    body.local_get(state);
    let table: Vec<u32> = (0..block_count as u32).collect();
    body.branch_table(&table, block_count as u32);
    body.end();

    for index in 0..block_count {
        // A block body sits inside the blocks that follow it, plus the
        // default block, plus the loop.
        let loop_depth = (block_count - index) as u32;
        let transfer =
            |body: &mut FunctionBodyBuilder, target: u64, extra_depth: u32| -> Result<()> {
                let destination = graph.block_at(target)?;
                body.i32_const(destination as i32);
                body.local_set(state);
                body.branch(loop_depth + extra_depth);
                Ok(())
            };

        emit_block_instructions(body, translator, lifted, index, graph)?;
        match &graph.blocks[index].terminator {
            Terminator::FallThrough { next } => transfer(body, *next, 0)?,
            Terminator::Jump { target } => transfer(body, *target, 0)?,
            Terminator::Switch { .. } => {
                emit_switch(
                    body,
                    translator,
                    lifted,
                    index,
                    graph,
                    |body, target, extra| transfer(body, target, extra),
                )?;
            }
            Terminator::Branch { target, not_taken } => {
                let condition = graph.blocks[index]
                    .terminating_instruction()
                    .expect("a conditional block ends with its branch");
                translator.branch_condition(body, &lifted.instructions[condition])?;
                body.if_();
                transfer(body, *target, 1)?;
                body.end();
                transfer(body, *not_taken, 0)?;
            }
            Terminator::ConditionalLeave { not_taken } => {
                let not_taken = *not_taken;
                emit_conditional_leave(body, translator, lifted, index, graph)?;
                transfer(body, not_taken, 0)?;
            }
            Terminator::Leaves => emit_leaving(body, translator, lifted, index, graph)?,
        }
        body.end();
    }

    body.unreachable();
    body.end();
    Ok(())
}

// ---- the structured translation ------------------------------------------

/// One level of the `block`/`loop`/`if` nesting currently open, from the
/// point of view of a `br`: the depth of an entry is its distance from the
/// top of the stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ContextEntry {
    /// A `loop` whose header is this block; branching here re-enters it.
    LoopHeader(usize),
    /// A `block` whose end is immediately followed by this block's code;
    /// branching here jumps forward to it.
    BlockFollowedBy(usize),
    /// An `if` or `else` arm, which is never a branch target of its own but
    /// still counts towards depth.
    ConditionalArm,
}

struct StructuredEmitter<'a> {
    graph: &'a ControlFlowGraph,
    dominators: &'a Dominators,
    loops: &'a LoopStructure,
    lifted: &'a LiftedFunction,
    context: Vec<ContextEntry>,
}

impl StructuredEmitter<'_> {
    /// Emits a dominator-tree node: a `loop` if it heads one, then the node's
    /// own code wrapped in a `block` for each merge point it dominates.
    fn emit_subtree(
        &mut self,
        body: &mut FunctionBodyBuilder,
        translator: &mut FunctionTranslator<'_>,
        block: usize,
    ) -> Result<()> {
        let merge_children: Vec<usize> = self.loops.dominator_children[block]
            .iter()
            .copied()
            .filter(|child| self.loops.is_merge_point[*child])
            .collect();

        if self.loops.is_loop_header[block] {
            body.loop_();
            self.context.push(ContextEntry::LoopHeader(block));
            self.emit_within(body, translator, block, &merge_children)?;
            self.context.pop();
            body.end();
        } else {
            self.emit_within(body, translator, block, &merge_children)?;
        }
        Ok(())
    }

    /// Wraps `block`'s code in one `block` per remaining merge child, so that
    /// the children are laid out after it in reverse postorder and every
    /// forward branch has somewhere to land.
    fn emit_within(
        &mut self,
        body: &mut FunctionBodyBuilder,
        translator: &mut FunctionTranslator<'_>,
        block: usize,
        merge_children: &[usize],
    ) -> Result<()> {
        let Some((&last, rest)) = merge_children.split_last() else {
            return self.emit_node(body, translator, block);
        };

        body.block();
        self.context.push(ContextEntry::BlockFollowedBy(last));
        self.emit_within(body, translator, block, rest)?;
        self.context.pop();
        body.end();

        self.emit_subtree(body, translator, last)
    }

    /// Emits a block's own instructions and its transfer.
    fn emit_node(
        &mut self,
        body: &mut FunctionBodyBuilder,
        translator: &mut FunctionTranslator<'_>,
        block: usize,
    ) -> Result<()> {
        emit_block_instructions(body, translator, self.lifted, block, self.graph)?;

        match &self.graph.blocks[block].terminator {
            Terminator::FallThrough { next } | Terminator::Jump { target: next } => {
                let destination = self.graph.block_at(*next)?;
                self.emit_branch(body, translator, block, destination)
            }
            Terminator::Switch { targets } => {
                // Every target is forced to be a merge point, so each has a
                // `block` of its own to branch to and none needs inlining.
                let depths: Vec<u32> = targets
                    .iter()
                    .map(|target| {
                        let destination = self.graph.block_at(*target)?;
                        self.branch_depth(destination)
                    })
                    .collect::<Result<_>>()?;
                let mut arm = 0;
                emit_switch(
                    body,
                    translator,
                    self.lifted,
                    block,
                    self.graph,
                    |body, _target, extra| {
                        body.branch(depths[arm] + extra);
                        arm += 1;
                        Ok(())
                    },
                )
            }
            Terminator::Branch { target, not_taken } => {
                let taken = self.graph.block_at(*target)?;
                let untaken = self.graph.block_at(*not_taken)?;
                let condition = self.graph.blocks[block]
                    .terminating_instruction()
                    .expect("a conditional block ends with its branch");
                translator.branch_condition(body, &self.lifted.instructions[condition])?;

                body.if_();
                self.context.push(ContextEntry::ConditionalArm);
                self.emit_branch(body, translator, block, taken)?;
                self.context.pop();

                body.else_();
                self.context.push(ContextEntry::ConditionalArm);
                self.emit_branch(body, translator, block, untaken)?;
                self.context.pop();
                body.end();
                Ok(())
            }
            Terminator::ConditionalLeave { not_taken } => {
                let destination = self.graph.block_at(*not_taken)?;
                emit_conditional_leave(body, translator, self.lifted, block, self.graph)?;
                self.emit_branch(body, translator, block, destination)
            }
            Terminator::Leaves => emit_leaving(body, translator, self.lifted, block, self.graph),
        }
    }

    /// A branch either jumps to an enclosing construct — backwards to a loop
    /// header, or forwards to a merge point's `block` — or, when the target
    /// has no other way in, is inlined where it is reached from.
    fn emit_branch(
        &mut self,
        body: &mut FunctionBodyBuilder,
        translator: &mut FunctionTranslator<'_>,
        source: usize,
        target: usize,
    ) -> Result<()> {
        if self.dominators.is_retreating(source, target) || self.loops.is_merge_point[target] {
            let depth = self.branch_depth(target)?;
            body.branch(depth);
            Ok(())
        } else {
            self.emit_subtree(body, translator, target)
        }
    }

    fn branch_depth(&self, target: usize) -> Result<u32> {
        self.context
            .iter()
            .rev()
            .position(|entry| {
                matches!(
                    entry,
                    ContextEntry::LoopHeader(block) | ContextEntry::BlockFollowedBy(block)
                        if *block == target
                )
            })
            .map(|depth| depth as u32)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "block {target} is branched to but no enclosing construct \
                     lands there; the control-flow graph is not shaped the way \
                     the structured translation assumes"
                )
            })
    }
}
