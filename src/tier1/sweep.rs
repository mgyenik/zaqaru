//! Finding blocks in an ELF without running it.
//!
//! A block here is exactly what the block cache decodes at run time
//! (`targum::block`): a run of instructions from one address through
//! conditional branches to the first unconditional transfer, call, return
//! or `syscall`, capped at `MAX_INSTRUCTIONS`. The sweep produces blocks of
//! that shape from the file's bytes, at the file's virtual addresses, so
//! that a block the engine decodes at run time — from the same bytes, at
//! whatever address the loader put them — is byte for byte one the sweep
//! produced, and the compiled function attaches.
//!
//! Two passes. The **descent** starts at every function symbol and the
//! entry point and follows what it can see: the fall-through of a block
//! that ends in a call (the return lands there), and every direct branch
//! and call target. The **walk** then goes over each executable segment
//! from its start, decoding a block at every address the descent left
//! uncovered — which is where a jump-table case, a computed-goto target
//! or a function without a symbol begins, since each of those follows an
//! unconditional transfer that ended the block before it.
//!
//! The sweep is allowed to be wrong about what is code. Bytes that were
//! never instructions decode into a block that no run-time decode will
//! ever match, and that block is a function nobody enters; the cost is
//! its size, and `docs/tier1-plan.md` §10 is the budget that bounds it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::Result;
use iced_x86::{Decoder, DecoderError, DecoderOptions, FlowControl, Instruction, OpKind};
use targum::block::{MAX_INSTRUCTIONS, terminator};

use crate::reader::{ObjectFile, SymbolRole};

/// One block the sweep found: its bytes, exactly as the cache would take
/// them, and the address they sit at in the file, which the compiler
/// needs only to turn the addresses inside them into deltas.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub address: u64,
    pub bytes: Vec<u8>,
    pub instructions: usize,
}

/// An executable segment, padded to its memory size so that a block
/// reaching past the file's bytes sees the zeros the loader will put there.
struct Text {
    start: u64,
    bytes: Vec<u8>,
}

impl Text {
    fn end(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }

    fn contains(&self, address: u64) -> bool {
        address >= self.start && address < self.end()
    }
}

/// Every block in an ELF's executable segments, descent first and then the
/// walk, each address at most once.
pub fn sweep(elf: &[u8]) -> Result<Vec<Candidate>> {
    let object = ObjectFile::parse(elf)?;
    let texts: Vec<Text> = object
        .segments
        .iter()
        .filter(|segment| segment.executable)
        .map(|segment| {
            let mut bytes = segment.bytes.clone();
            bytes.resize(segment.memory_size.max(segment.file_size) as usize, 0);
            Text {
                start: segment.address,
                bytes,
            }
        })
        .collect();
    if texts.is_empty() {
        return Ok(Vec::new());
    }

    let mut starts: VecDeque<u64> = VecDeque::new();
    if object.entry != 0 {
        starts.push_back(object.entry);
    }
    for symbol in &object.symbols {
        if symbol.role != SymbolRole::Function || !symbol.defined {
            continue;
        }
        let Some(section) = symbol.section else {
            continue;
        };
        let Some(section) = object.sections.get(section) else {
            continue;
        };
        starts.push_back(section.address + symbol.offset);
    }

    let mut blocks: BTreeMap<u64, Candidate> = BTreeMap::new();
    // Every instruction start inside some block, with its length, so that
    // the walk can step over what the descent covered.
    let mut covered: BTreeMap<u64, u64> = BTreeMap::new();
    let mut seen: BTreeSet<u64> = BTreeSet::new();

    while let Some(address) = starts.pop_front() {
        if !seen.insert(address) {
            continue;
        }
        let Some(text) = texts.iter().find(|text| text.contains(address)) else {
            continue;
        };
        let Some((candidate, successors)) = decode_block(text, address) else {
            continue;
        };
        let mut at = candidate.address;
        for instruction in decode_instructions(&candidate) {
            covered.insert(at, instruction.len() as u64);
            at += instruction.len() as u64;
        }
        for successor in successors {
            if texts.iter().any(|text| text.contains(successor)) {
                starts.push_back(successor);
            }
        }
        blocks.insert(candidate.address, candidate);
    }

    // The walk, and then the descent again from whatever the walk's blocks
    // branch to, until nothing new turns up: a jump-table case found by
    // the walk jumps to a shared tail that only it knew about.
    loop {
        let mut found = Vec::new();
        for text in &texts {
            let mut at = text.start;
            while at < text.end() {
                if let Some(length) = covered.get(&at) {
                    at += length.max(&1);
                    continue;
                }
                if seen.contains(&at) {
                    at += 1;
                    continue;
                }
                seen.insert(at);
                match decode_block(text, at) {
                    Some((candidate, successors)) => {
                        let mut here = candidate.address;
                        for instruction in decode_instructions(&candidate) {
                            covered.entry(here).or_insert(instruction.len() as u64);
                            here += instruction.len() as u64;
                        }
                        let next = here;
                        blocks.insert(candidate.address, candidate);
                        found.extend(successors);
                        at = next.max(at + 1);
                    }
                    None => at += 1,
                }
            }
        }
        let mut any = false;
        let mut starts: VecDeque<u64> = found.into_iter().collect();
        while let Some(address) = starts.pop_front() {
            if !seen.insert(address) {
                continue;
            }
            let Some(text) = texts.iter().find(|text| text.contains(address)) else {
                continue;
            };
            let Some((candidate, successors)) = decode_block(text, address) else {
                continue;
            };
            any = true;
            let mut at = candidate.address;
            for instruction in decode_instructions(&candidate) {
                covered.entry(at).or_insert(instruction.len() as u64);
                at += instruction.len() as u64;
            }
            for successor in successors {
                if texts.iter().any(|text| text.contains(successor)) {
                    starts.push_back(successor);
                }
            }
            blocks.insert(candidate.address, candidate);
        }
        if !any {
            break;
        }
    }

    Ok(blocks.into_values().collect())
}

/// Decodes one block at `address` the way the cache does, and answers it
/// with the addresses the descent should try next.
fn decode_block(text: &Text, address: u64) -> Option<(Candidate, Vec<u64>)> {
    let offset = (address - text.start) as usize;
    let cap = 15 * MAX_INSTRUCTIONS;
    let window = &text.bytes[offset..(offset + cap).min(text.bytes.len())];
    let mut decoder = Decoder::with_ip(64, window, address, DecoderOptions::NONE);
    let mut count = 0usize;
    let mut end = address;
    let mut successors = Vec::new();
    let mut last: Option<Instruction> = None;
    while count < MAX_INSTRUCTIONS {
        if !decoder.can_decode() {
            break;
        }
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            match decoder.last_error() {
                DecoderError::NoMoreBytes if count > 0 => break,
                _ => return None,
            }
        }
        end = instruction.next_ip();
        count += 1;
        if instruction.op0_kind() == OpKind::NearBranch64 {
            successors.push(instruction.near_branch64());
        }
        let terminates = terminator(&instruction).is_some();
        last = Some(instruction);
        if terminates {
            break;
        }
    }
    if count == 0 {
        return None;
    }
    if let Some(last) = last
        && last.flow_control() == FlowControl::Call
    {
        // The return lands after the call.
        successors.push(last.next_ip());
    }
    let bytes = text.bytes[offset..(end - text.start) as usize].to_vec();
    Some((
        Candidate {
            address,
            bytes,
            instructions: count,
        },
        successors,
    ))
}

/// The instructions of a candidate, decoded again — the cache's decode,
/// which is also the compiler's.
pub fn decode_instructions(candidate: &Candidate) -> Vec<Instruction> {
    let mut decoder = Decoder::with_ip(
        64,
        &candidate.bytes,
        candidate.address,
        DecoderOptions::NONE,
    );
    let mut instructions = Vec::with_capacity(candidate.instructions);
    while decoder.can_decode() && instructions.len() < MAX_INSTRUCTIONS {
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            break;
        }
        instructions.push(instruction);
    }
    instructions
}
