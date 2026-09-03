//! Regions: blocks connected by constant-target branches, compiled into one
//! function.
//!
//! A block alone is a proof of the seam and not a speed-up. Every call
//! into it loads the registers it uses, runs its handful of instructions,
//! and stores them back — and on the Django demo, whose blocks average five
//! instructions and a quarter of which end in a call, that frame costs
//! more than interpreting the five. A region keeps guest registers in
//! locals across many blocks and turns a branch to another block of the
//! region into a branch inside the function, which is where the speed the
//! design promised lives.
//!
//! Formation is by the edges a compiled block can take without leaving:
//! the taken target of a conditional branch, the target of a direct jump,
//! and the fall-through of a block the cache's cap or a page cut short.
//! Calls and returns leave a region in this version — a return address is
//! not a constant, and a call's target is another region's business. From
//! each block not yet in a region, in address order, blocks are gathered
//! breadth-first along those edges up to the cap, and every block belongs
//! to exactly one region.

use std::collections::{BTreeMap, VecDeque};

use iced_x86::{FlowControl, Instruction, OpKind};

use super::sweep::{Candidate, decode_instructions};

/// How many blocks a region may hold. Not "larger is better": Cranelift
/// does not split a function it cannot allocate registers for, and the
/// interpreter's own history is a list of bodies that got slower by
/// growing.
///
/// Sixty-four blocks of five instructions is a few hundred wasm
/// instructions per body, which Cranelift handles. The multi-member defect
/// that held this at one earlier was the defer helper naming an
/// instruction by its position in the *entry* block — wrong once a region
/// runs instructions from several blocks — and it is fixed: the helper
/// takes the instruction's guest address and the block cache decodes it.
/// The container suite passes with every block compiled into regions this
/// size, and the engine's verify mode is clean. What regions do *not* buy,
/// measured, is speed on the container: see `docs/tier1-plan.md` T3.
pub const MAX_MEMBERS: usize = 64;

/// A region: its members in address order, and the address the deltas
/// inside the compiled function are taken from.
#[derive(Clone, Debug)]
pub struct Region {
    pub members: Vec<Candidate>,
}

impl Region {
    /// The address every delta in the function is relative to: the first
    /// member's.
    pub fn base(&self) -> u64 {
        self.members[0].address
    }
}

/// The addresses a block can branch to without leaving compiled code.
fn edges(instructions: &[Instruction]) -> Vec<u64> {
    let mut targets = Vec::new();
    for instruction in instructions {
        match instruction.flow_control() {
            FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch
                if instruction.op0_kind() == OpKind::NearBranch64 =>
            {
                targets.push(instruction.near_branch64());
            }
            _ => {}
        }
    }
    // A block that did not end in a transfer falls through.
    if let Some(last) = instructions.last()
        && last.flow_control() == FlowControl::Next
    {
        targets.push(last.next_ip());
    }
    targets
}

/// Gathers the candidates into regions.
pub fn form(candidates: &[Candidate]) -> Vec<Region> {
    let by_address: BTreeMap<u64, usize> = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.address, index))
        .collect();
    let successors: Vec<Vec<usize>> = candidates
        .iter()
        .map(|candidate| {
            edges(&decode_instructions(candidate))
                .into_iter()
                .filter_map(|target| by_address.get(&target).copied())
                .collect()
        })
        .collect();
    let mut assigned = vec![false; candidates.len()];
    let mut regions = Vec::new();
    for start in 0..candidates.len() {
        if assigned[start] {
            continue;
        }
        let mut members = Vec::new();
        let mut queue = VecDeque::from([start]);
        assigned[start] = true;
        while let Some(index) = queue.pop_front() {
            members.push(index);
            if members.len() + queue.len() >= MAX_MEMBERS {
                continue;
            }
            for &next in &successors[index] {
                if !assigned[next] && members.len() + queue.len() < MAX_MEMBERS {
                    assigned[next] = true;
                    queue.push_back(next);
                }
            }
        }
        members.sort_unstable();
        regions.push(Region {
            members: members.into_iter().map(|index| candidates[index].clone()).collect(),
        });
    }
    regions
}
