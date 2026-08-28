//! Basic-block control-flow graphs over lifted instructions.
//!
//! Block leaders are the function entry, every branch target inside the
//! function, and every instruction following a terminator. Calls are *not*
//! terminators: a call returns to the instruction after it, so splitting
//! there would only add blocks without adding information — except in the
//! [`ControlFlowGraph::build_resumable`] variant, where each post-call
//! instruction heading a block of its own is exactly the information wanted:
//! those blocks are the points a suspended frame can be re-entered at.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use iced_x86::FlowControl;

use crate::lifter::{LiftedFunction, LiftedInstruction};

/// How control leaves a basic block.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Terminator {
    /// Control continues at the next block; the block has no branch of its
    /// own.
    FallThrough { next: u64 },
    /// An unconditional jump within the function.
    Jump { target: u64 },
    /// A conditional jump: taken to `target`, otherwise to `not_taken`.
    Branch { target: u64, not_taken: u64 },
    /// A `switch` dispatch recovered from a jump table: control continues at
    /// whichever target the index selects.
    Switch { targets: Vec<u64> },
    /// A conditional branch out of the function — a tail call made only when
    /// the condition holds, which is how compilers reach a cold path split
    /// into a section of its own. Otherwise control continues at `not_taken`.
    ConditionalLeave { not_taken: u64 },
    /// A `ret`, or a tail jump to another function — including an indirect
    /// one — which the translator turns into a call followed by a return.
    Leaves,
    /// The block's last instruction runs, and control does not continue past
    /// it.
    ///
    /// A function whose final instruction is a `call` with nothing after it
    /// called something that does not return, and the compiler emitted
    /// nothing for a path that cannot be taken — `abort`, `exit`,
    /// `__stack_chk_fail`, `_Unwind_Resume`. Every `.cold` fragment gcc
    /// splits out of a function ends this way, and so does every wrapper
    /// around a `noreturn`. There is no fall-through successor because
    /// there is no fall-through.
    Unreachable,
}

#[derive(Clone, Debug)]
pub struct BasicBlock {
    /// Section offset of the block's first instruction.
    pub start: u64,
    /// Range of [`LiftedFunction::instructions`] the block covers.
    pub instructions: std::ops::Range<usize>,
    pub terminator: Terminator,
}

impl BasicBlock {
    /// The instructions the translator handles: everything except the
    /// terminating transfer, which belongs to the structurer.
    pub fn body_instructions(&self) -> std::ops::Range<usize> {
        match self.terminator {
            // The call is an ordinary instruction in both: what follows it
            // is the terminator, and for `Unreachable` what follows it is
            // nothing.
            Terminator::FallThrough { .. } | Terminator::Unreachable => self.instructions.clone(),
            _ => self.instructions.start..self.instructions.end - 1,
        }
    }

    /// The blocks a `switch` dispatch can reach, if this block ends in one.
    pub fn switch_targets(&self) -> &[u64] {
        match &self.terminator {
            Terminator::Switch { targets } => targets,
            _ => &[],
        }
    }

    /// Index of the instruction that ends the block, when the block ends with
    /// a transfer of its own rather than by running into the next one.
    pub fn terminating_instruction(&self) -> Option<usize> {
        match &self.terminator {
            Terminator::FallThrough { .. } | Terminator::Unreachable => None,
            _ => Some(self.instructions.end - 1),
        }
    }
}

pub struct ControlFlowGraph {
    pub blocks: Vec<BasicBlock>,
    index_of_start: HashMap<u64, usize>,
}

impl ControlFlowGraph {
    /// Index of the block starting at a section offset.
    pub fn block_at(&self, offset: u64) -> Result<usize> {
        self.index_of_start.get(&offset).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "{offset:#x} is not the start of a basic block; a branch into \
                 the middle of an instruction, or into another function, is \
                 out of scope"
            )
        })
    }

    /// Successors of a block, in no particular order.
    pub fn successors(&self, block: usize) -> Vec<usize> {
        match &self.blocks[block].terminator {
            Terminator::FallThrough { next } => {
                self.index_of_start.get(next).copied().into_iter().collect()
            }
            Terminator::Jump { target } => self
                .index_of_start
                .get(target)
                .copied()
                .into_iter()
                .collect(),
            Terminator::Switch { targets } => targets
                .iter()
                .filter_map(|offset| self.index_of_start.get(offset).copied())
                .collect(),
            Terminator::ConditionalLeave { not_taken } => self
                .index_of_start
                .get(not_taken)
                .copied()
                .into_iter()
                .collect(),
            // Nothing follows it, which is the point.
            Terminator::Unreachable => Vec::new(),
            Terminator::Branch { target, not_taken } => [target, not_taken]
                .iter()
                .filter_map(|offset| self.index_of_start.get(offset).copied())
                .collect(),
            Terminator::Leaves => Vec::new(),
        }
    }

    pub fn build(function: &LiftedFunction) -> Result<Self> {
        Self::build_with_leaders(function, false)
    }

    /// The same graph with every call ending its block, so that each
    /// post-call instruction heads a block — the entry points of the
    /// function's resume body. A call still falls through to the next block;
    /// nothing about the terminators changes.
    pub fn build_resumable(function: &LiftedFunction) -> Result<Self> {
        Self::build_with_leaders(function, true)
    }

    fn build_with_leaders(function: &LiftedFunction, split_after_calls: bool) -> Result<Self> {
        let leaders = collect_leaders(function, split_after_calls)?;
        let mut blocks = Vec::new();

        let starts: Vec<u64> = leaders.iter().copied().collect();
        for (position, start) in starts.iter().enumerate() {
            let end = starts
                .get(position + 1)
                .copied()
                .unwrap_or(function.offset + function.size);
            let first = instruction_index(function, *start)?;
            let last = function
                .instructions
                .iter()
                .rposition(|lifted| lifted.offset < end)
                .expect("a block contains at least one instruction");

            let terminator = classify_terminator(&function.instructions[last], function, last, end);
            blocks.push(BasicBlock {
                start: *start,
                instructions: first..last + 1,
                terminator,
            });
        }

        let index_of_start = blocks
            .iter()
            .enumerate()
            .map(|(index, block)| (block.start, index))
            .collect();

        let graph = Self {
            blocks,
            index_of_start,
        };

        // Every internal transfer must land on a block boundary; anything
        // else means the leader scan missed a target.
        for index in 0..graph.blocks.len() {
            match &graph.blocks[index].terminator {
                Terminator::FallThrough { next } | Terminator::Jump { target: next } => {
                    graph.block_at(*next)?;
                }
                Terminator::Branch { target, not_taken } => {
                    graph.block_at(*target)?;
                    graph.block_at(*not_taken)?;
                }
                // No internal transfer to check: control does not continue.
                Terminator::Unreachable => {}
                Terminator::Switch { targets } => {
                    for target in targets {
                        graph.block_at(*target)?;
                    }
                }
                Terminator::ConditionalLeave { not_taken } => {
                    graph.block_at(*not_taken)?;
                }
                Terminator::Leaves => {}
            }
        }

        Ok(graph)
    }
}

/// The dominator tree, plus the orderings the structured translation is
/// built on.
///
/// Dominators are computed by the Cooper-Harvey-Kennedy iteration over
/// reverse postorder: simple, fast enough for function-sized graphs, and easy
/// to check by hand.
pub struct Dominators {
    /// Block indices in reverse postorder. Unreachable blocks do not appear.
    pub reverse_postorder: Vec<usize>,
    /// Position of each block in [`Self::reverse_postorder`], or `None` if
    /// the block is unreachable.
    pub order_of_block: Vec<Option<usize>>,
    /// Immediate dominator of each reachable block; the entry dominates
    /// itself.
    pub immediate_dominator: Vec<usize>,
    pub predecessors: Vec<Vec<usize>>,
}

impl Dominators {
    pub fn compute(graph: &ControlFlowGraph) -> Self {
        let count = graph.blocks.len();
        let reverse_postorder = compute_reverse_postorder(graph);
        let mut order_of_block = vec![None; count];
        for (position, block) in reverse_postorder.iter().enumerate() {
            order_of_block[*block] = Some(position);
        }

        let mut predecessors = vec![Vec::new(); count];
        for block in 0..count {
            for successor in graph.successors(block) {
                predecessors[successor].push(block);
            }
        }

        const UNSET: usize = usize::MAX;
        let mut immediate_dominator = vec![UNSET; count];
        if let Some(&entry) = reverse_postorder.first() {
            immediate_dominator[entry] = entry;
        }

        let order = |block: usize| order_of_block[block].unwrap_or(usize::MAX);
        let mut changed = true;
        while changed {
            changed = false;
            for &block in reverse_postorder.iter().skip(1) {
                let mut candidate = UNSET;
                for &predecessor in &predecessors[block] {
                    if immediate_dominator[predecessor] == UNSET {
                        continue;
                    }
                    candidate = if candidate == UNSET {
                        predecessor
                    } else {
                        intersect(&immediate_dominator, &order, predecessor, candidate)
                    };
                }
                if candidate != UNSET && immediate_dominator[block] != candidate {
                    immediate_dominator[block] = candidate;
                    changed = true;
                }
            }
        }

        Self {
            reverse_postorder,
            order_of_block,
            immediate_dominator,
            predecessors,
        }
    }

    /// Whether `ancestor` dominates `block` — every path from the entry to
    /// `block` passes through it.
    pub fn dominates(&self, ancestor: usize, block: usize) -> bool {
        let mut current = block;
        loop {
            if current == ancestor {
                return true;
            }
            let parent = self.immediate_dominator[current];
            if parent == current || parent == usize::MAX {
                return false;
            }
            current = parent;
        }
    }

    pub fn is_reachable(&self, block: usize) -> bool {
        self.order_of_block[block].is_some()
    }

    /// Whether an edge runs backwards in reverse postorder — a loop's back
    /// edge, or, in an irreducible graph, an entry into a loop from outside
    /// its header.
    pub fn is_retreating(&self, from: usize, to: usize) -> bool {
        match (self.order_of_block[from], self.order_of_block[to]) {
            (Some(from), Some(to)) => to <= from,
            _ => false,
        }
    }
}

fn intersect(
    immediate_dominator: &[usize],
    order: &impl Fn(usize) -> usize,
    mut left: usize,
    mut right: usize,
) -> usize {
    while left != right {
        while order(left) > order(right) {
            left = immediate_dominator[left];
        }
        while order(right) > order(left) {
            right = immediate_dominator[right];
        }
    }
    left
}

fn compute_reverse_postorder(graph: &ControlFlowGraph) -> Vec<usize> {
    let count = graph.blocks.len();
    if count == 0 {
        return Vec::new();
    }
    let mut visited = vec![false; count];
    let mut postorder = Vec::new();

    // An explicit stack, because a long chain of blocks would otherwise be a
    // recursion depth proportional to the function's size.
    let mut stack = vec![(0usize, 0usize)];
    visited[0] = true;
    while let Some((block, next_successor)) = stack.pop() {
        let successors = graph.successors(block);
        if next_successor < successors.len() {
            stack.push((block, next_successor + 1));
            let successor = successors[next_successor];
            if !visited[successor] {
                visited[successor] = true;
                stack.push((successor, 0));
            }
        } else {
            postorder.push(block);
        }
    }

    postorder.reverse();
    postorder
}

/// What the structured translation needs to know about a graph's shape.
pub struct LoopStructure {
    /// Blocks that are the target of a back edge.
    pub is_loop_header: Vec<bool>,
    /// Blocks reached by more than one forward edge, which therefore need a
    /// `block` to branch to rather than being inlined.
    pub is_merge_point: Vec<bool>,
    /// Children in the dominator tree, in reverse postorder.
    pub dominator_children: Vec<Vec<usize>>,
    /// False when some retreating edge enters a loop somewhere other than
    /// its header, which no `block`/`loop` nesting can express.
    pub reducible: bool,
}

impl LoopStructure {
    pub fn analyse(graph: &ControlFlowGraph, dominators: &Dominators) -> Self {
        let count = graph.blocks.len();
        let mut is_loop_header = vec![false; count];
        let mut forward_predecessor_count = vec![0usize; count];
        let mut reducible = true;

        for &block in &dominators.reverse_postorder {
            for successor in graph.successors(block) {
                if dominators.is_retreating(block, successor) {
                    if dominators.dominates(successor, block) {
                        is_loop_header[successor] = true;
                    } else {
                        reducible = false;
                    }
                } else {
                    forward_predecessor_count[successor] += 1;
                }
            }
        }

        let mut is_merge_point: Vec<bool> = forward_predecessor_count
            .iter()
            .map(|count| *count > 1)
            .collect();

        // A `br_table` arm has to branch; unlike a two-way branch it cannot
        // inline its target. Treating every dispatch target as a merge point
        // gives each one a `block` of its own to land in.
        for block in 0..count {
            for target in graph.blocks[block].switch_targets() {
                if let Ok(destination) = graph.block_at(*target) {
                    is_merge_point[destination] = true;
                }
            }
        }

        let mut dominator_children = vec![Vec::new(); count];
        for &block in dominators.reverse_postorder.iter().skip(1) {
            let parent = dominators.immediate_dominator[block];
            if parent != block && parent != usize::MAX {
                dominator_children[parent].push(block);
            }
        }
        for children in &mut dominator_children {
            children.sort_by_key(|block| dominators.order_of_block[*block]);
        }

        Self {
            is_loop_header,
            is_merge_point,
            dominator_children,
            reducible,
        }
    }
}

fn instruction_index(function: &LiftedFunction, offset: u64) -> Result<usize> {
    function
        .instructions
        .iter()
        .position(|lifted| lifted.offset == offset)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{offset:#x} is not an instruction boundary in `{}`",
                function.name
            )
        })
}

fn collect_leaders(function: &LiftedFunction, split_after_calls: bool) -> Result<BTreeSet<u64>> {
    let mut leaders = BTreeSet::new();
    leaders.insert(function.offset);

    for (position, lifted) in function.instructions.iter().enumerate() {
        let instruction = &lifted.instruction;
        let branches = is_branch(lifted);

        // A recovered `switch` reaches every one of its targets.
        if let Some(table) = function.jump_tables.get(&position) {
            leaders.extend(table.targets.iter().copied());
        }

        if branches && is_internal_branch(lifted, function) {
            leaders.insert(instruction.near_branch64());
        }

        let terminates = branches
            || matches!(
                instruction.flow_control(),
                FlowControl::Return | FlowControl::IndirectBranch
            )
            || (split_after_calls
                && matches!(
                    instruction.flow_control(),
                    FlowControl::Call | FlowControl::IndirectCall
                ));
        if let (true, Some(next)) = (terminates, function.instructions.get(position + 1)) {
            leaders.insert(next.offset);
        }
    }

    Ok(leaders)
}

fn is_branch(lifted: &LiftedInstruction) -> bool {
    matches!(
        lifted.instruction.flow_control(),
        FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch
    )
}

/// A branch is *internal* when it lands inside this function. A jump naming a
/// symbol, or landing in another function of the same section, is a tail
/// call: the translator turns it into a call followed by a return.
fn is_internal_branch(lifted: &LiftedInstruction, function: &LiftedFunction) -> bool {
    is_branch(lifted)
        && lifted.immediate.is_none()
        && function.contains(lifted.instruction.near_branch64())
}

fn classify_terminator(
    lifted: &LiftedInstruction,
    function: &LiftedFunction,
    last: usize,
    block_end: u64,
) -> Terminator {
    let instruction = &lifted.instruction;
    if instruction.flow_control() == FlowControl::Return {
        return Terminator::Leaves;
    }
    if instruction.flow_control() == FlowControl::IndirectBranch {
        // A dispatch the lifter recovered a table for is a `switch`;
        // any other indirect jump is an indirect tail call.
        return match function.jump_tables.get(&last) {
            Some(table) => Terminator::Switch {
                targets: table.targets.clone(),
            },
            None => Terminator::Leaves,
        };
    }
    if is_internal_branch(lifted, function) {
        return match instruction.flow_control() {
            FlowControl::ConditionalBranch => Terminator::Branch {
                target: instruction.near_branch64(),
                not_taken: block_end,
            },
            _ => Terminator::Jump {
                target: instruction.near_branch64(),
            },
        };
    }
    if instruction.flow_control() == FlowControl::UnconditionalBranch {
        return Terminator::Leaves;
    }
    // A conditional branch that does not stay inside the function is a tail
    // call made only when the condition holds.
    if instruction.flow_control() == FlowControl::ConditionalBranch {
        return Terminator::ConditionalLeave {
            not_taken: block_end,
        };
    }
    // Nothing follows, and the block runs off the end of the function. A
    // compiler emits that only after a call that does not return, so the
    // honest translation of the path past it is that there is not one.
    if instruction.flow_control() == FlowControl::Call && !function.contains(block_end) {
        return Terminator::Unreachable;
    }
    Terminator::FallThrough { next: block_end }
}
