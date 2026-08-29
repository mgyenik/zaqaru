//! What is left to implement in a linked program, and how much of it can
//! actually be reached.
//!
//! The refusal list a transpile prints is a worklist, but a raw count of it
//! overstates the work by a wide margin: a statically linked C library
//! carries every string routine for every microarchitecture, and curated
//! CPUID means the reachable program selects one of them. What matters is
//! the subset reachable from the entry point.
//!
//! Reachability here follows direct calls and jumps, and also any operand
//! that names a function's first byte — the `lea` that hands `main` to
//! `__libc_start_main` is the reason: following calls alone stops at
//! `_start` and reports the whole program as unreachable, which is worse
//! than useless because it reads as good news.
//!
//! It is still a lower bound. A function reached only through a pointer
//! computed at run time, or through a table this does not recognise, is
//! invisible to it, and an address-taken function counts as reached whether
//! or not it is ever called. That is the honest shape of the number and the
//! reason the worklog quotes both columns rather than picking one.
//!
//! Usage: `cargo run --example refusals -- <linked-elf>`

use std::collections::{HashMap, HashSet};

use iced_x86::{Decoder, DecoderOptions, FlowControl, OpKind};

fn main() {
    let path = std::env::args().nth(1).expect("usage: refusals <elf>");
    let bytes = std::fs::read(&path).expect("read the program");
    let object = zaqaru::reader::ObjectFile::parse(&bytes).expect("parse");

    let translation = zaqaru::transpile::Transpiler::new(&object)
        .with_untranslatable(zaqaru::transpile::Untranslatable::Trap)
        .translate()
        .expect("translate");

    // Where each function starts, in the addresses the program is loaded
    // at, so a branch operand can be looked up directly.
    let mut at: HashMap<u64, usize> = HashMap::new();
    for (index, function) in object.functions.iter().enumerate() {
        let address = object.sections[function.section].address + function.offset;
        at.insert(address, index);
    }

    let mut reached: HashSet<usize> = HashSet::new();
    let mut worklist: Vec<usize> = at.get(&object.entry).copied().into_iter().collect();
    while let Some(index) = worklist.pop() {
        if !reached.insert(index) {
            continue;
        }
        let function = &object.functions[index];
        let section = &object.sections[function.section];
        let start = function.offset as usize;
        let body = &section.bytes[start..start + function.size as usize];
        // The decoder runs section-relative, which is what makes a branch
        // operand a section offset; adding the section's address turns it
        // back into the address the loader will use.
        let mut decoder = Decoder::with_ip(64, body, function.offset, DecoderOptions::NONE);
        for instruction in &mut decoder {
            let branches = matches!(
                instruction.flow_control(),
                FlowControl::Call
                    | FlowControl::UnconditionalBranch
                    | FlowControl::ConditionalBranch
            );
            if branches && instruction.op0_kind() == OpKind::NearBranch64 {
                let target = section.address.wrapping_add(instruction.near_branch64());
                if let Some(&callee) = at.get(&target) {
                    worklist.push(callee);
                }
                continue;
            }

            // Every other way an address can appear: a program-counter
            // relative `lea`, and an absolute immediate. Both are section
            // offsets as the decoder reports them, for the same reason the
            // branch operand is.
            for operand in 0..instruction.op_count() {
                let candidate = match instruction.op_kind(operand) {
                    OpKind::Memory if instruction.is_ip_rel_memory_operand() => section
                        .address
                        .wrapping_add(instruction.ip_rel_memory_address()),
                    OpKind::Immediate32to64 | OpKind::Immediate64 => instruction.immediate(operand),
                    OpKind::Immediate32 => u64::from(instruction.immediate32()),
                    _ => continue,
                };
                if let Some(&taken) = at.get(&candidate) {
                    worklist.push(taken);
                }
            }
        }
    }

    let refused: HashMap<&str, &str> = translation
        .refused
        .iter()
        .map(|refusal| (refusal.name.as_str(), refusal.reason.as_str()))
        .collect();

    let mut tail: Vec<(&str, &str)> = reached
        .iter()
        .filter_map(|&index| {
            let name = object.functions[index].name.as_str();
            refused.get(name).map(|reason| (name, *reason))
        })
        .collect();
    tail.sort_unstable();

    // The histogram is the actual worklist: one reason repeated forty times
    // is one instruction to implement, not forty functions to read.
    let mut histogram: HashMap<&str, usize> = HashMap::new();
    for (_, reason) in &tail {
        *histogram.entry(reason).or_default() += 1;
    }
    let mut counts: Vec<(&str, usize)> = histogram.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));

    println!(
        "{} functions, {} reachable from the entry point",
        object.functions.len(),
        reached.len()
    );
    println!(
        "{} refused, {} of them reachable",
        translation.refused.len(),
        tail.len()
    );
    for (reason, count) in &counts {
        println!("  {count:4}  {reason}");
    }
    // The names go to stderr so the summary can be read on its own.
    for (name, reason) in &tail {
        eprintln!("{name}: {reason}");
    }
}
