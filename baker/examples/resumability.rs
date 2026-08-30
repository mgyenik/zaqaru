//! How much of a container's resume-body weight the suspension analysis
//! could remove — measured, not estimated.
//!
//! Like `examples/survey` in the main crate, this is a survey instrument
//! and must not become a test: it exists for the moments when the question
//! is about a population of functions rather than about a change.
//!
//! Under `--resume` every function gets a second body, and the second body
//! exists so a suspended chain can be re-entered mid-frame. A function can
//! only be suspended-in if a suspension point can occur while its frame is
//! live — a structural property of the code, valid for every input, unlike
//! reachability, which is a property of the inputs someone imagined:
//!
//!   L(f)        = "invoking f can lead to a suspension while the chain is
//!                  live": f contains a syscall, an indirect call, an
//!                  indirect tail transfer (callee unknown), is or reaches
//!                  the setjmp/longjmp family, or reaches any of those
//!                  through a call or tail edge.
//!   needs(f)    = f contains a syscall or an indirect call, or some
//!                  *non-tail* call site of f has a callee with L — those
//!                  are the frames a resumed chain can re-enter. A tail
//!                  transfer replaces f's frame, so it never puts f on a
//!                  suspended chain, and conditional branches between the
//!                  pieces of a split function are tail edges for the same
//!                  reason: the pieces share one guest frame, and the
//!                  branched-from piece is not the one re-entered.
//!
//! Everything unknown degrades toward "needs a body" (bytes), never toward
//! "does not" (a dead container): an indirect call site suspends, a direct
//! call whose target no function starts at suspends, an indirect branch
//! that is not a recovered jump table may lead to suspension.
//!
//! The byte counts are x86 extent bytes, a *proxy* for wasm resume-body
//! bytes — the two are roughly proportional, and the proxy is available
//! without emitting anything.
//!
//! Usage: `cargo run --release --example resumability -- <program> <root>`

use std::collections::HashMap;

use anyhow::{Context, Result};
use iced_x86::{FlowControl, Mnemonic, OpKind};

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [program, root] = arguments.as_slice() else {
        anyhow::bail!("usage: resumability <program> <root>");
    };
    let program = std::path::Path::new(program);
    let root = std::path::Path::new(root);

    // The bake's own recipe: the closure, the sweep, the bases, one merged
    // object — so the function population is the one the container carries.
    let mut modules = baker::dynamic::closure(program, root).context("closure")?;
    let tree = baker::tree::Tree::from_directory(root).context("reading the image tree")?;
    modules.extend(baker::dynamic::sweep(&tree, &modules).context("sweep")?);
    let bases = baker::dynamic::assign_bases(&modules).context("bases")?;
    let mut inputs = Vec::new();
    for module in &modules {
        let base = bases[&module.path];
        let object = zaqaru::reader::ObjectFile::parse_at(&module.bytes, base)
            .with_context(|| format!("reading {}", module.path))?;
        inputs.push((module.path.clone(), object));
    }
    let object = match inputs.len() {
        1 => inputs.pop().expect("just checked").1,
        _ => zaqaru::reader::ObjectFile::merge(inputs).context("merge")?,
    };
    let lifted = zaqaru::lifter::lift_object(&object).context("lift")?;

    // Function starts by loaded address, the same mapping `refusals` uses:
    // the decoder is section-relative, so a branch operand plus the
    // section's address is the address the loader will use.
    let mut at: HashMap<u64, usize> = HashMap::new();
    for (index, function) in object.functions.iter().enumerate() {
        at.insert(
            object.sections[function.section].address + function.offset,
            index,
        );
    }

    let total = object.functions.len();
    let mut seed = vec![false; total]; // L seeds
    let mut needs = vec![false; total]; // needs(f) before call-edge closure
    let mut call_edges: Vec<(usize, usize)> = Vec::new(); // non-tail: f calls g
    let mut tail_edges: Vec<(usize, usize)> = Vec::new(); // f's frame becomes g's

    let mut seeded_syscall = 0usize;
    let mut seeded_indirect_call = 0usize;
    let mut seeded_indirect_tail = 0usize;
    let mut seeded_unresolved_call = 0usize;
    let mut seeded_name = 0usize;

    for (index, function) in lifted.iter().enumerate() {
        let section = &object.sections[function.section];
        let lower = function.name.to_ascii_lowercase();
        if lower.contains("setjmp") || lower.contains("longjmp") {
            seed[index] = true;
            seeded_name += 1;
        }
        for (position, instruction) in function.instructions.iter().enumerate() {
            let instruction = &instruction.instruction;
            match instruction.flow_control() {
                FlowControl::Call if instruction.op0_kind() == OpKind::NearBranch64 => {
                    if instruction.mnemonic() == Mnemonic::Syscall {
                        // Rewritten to a call of the seam: a resume site in
                        // this function, and a suspension point outright.
                        seed[index] = true;
                        if !needs[index] {
                            seeded_syscall += 1;
                        }
                        needs[index] = true;
                        continue;
                    }
                    let target = section.address.wrapping_add(instruction.near_branch64());
                    match at.get(&target) {
                        Some(&callee) => call_edges.push((index, callee)),
                        None => {
                            seed[index] = true;
                            if !needs[index] {
                                seeded_unresolved_call += 1;
                            }
                            needs[index] = true;
                        }
                    }
                }
                FlowControl::Call | FlowControl::IndirectCall => {
                    // `syscall` decodes as a Call without a branch operand,
                    // so it lands here on some iced versions; either arm is
                    // a suspension point in this function.
                    seed[index] = true;
                    if !needs[index] {
                        if instruction.mnemonic() == Mnemonic::Syscall {
                            seeded_syscall += 1;
                        } else {
                            seeded_indirect_call += 1;
                        }
                    }
                    needs[index] = true;
                }
                FlowControl::UnconditionalBranch | FlowControl::ConditionalBranch
                    if instruction.op0_kind() == OpKind::NearBranch64 =>
                {
                    let offset = instruction.near_branch64();
                    if function.contains(offset) {
                        continue; // internal control flow
                    }
                    let target = section.address.wrapping_add(offset);
                    match at.get(&target) {
                        Some(&callee) => tail_edges.push((index, callee)),
                        None => {
                            // A branch out of the function to no known
                            // start: unknown continuation, may suspend.
                            if !seed[index] {
                                seeded_indirect_tail += 1;
                            }
                            seed[index] = true;
                        }
                    }
                }
                FlowControl::IndirectBranch => {
                    match function.jump_tables.get(&position) {
                        Some(table) => {
                            for &arm in &table.targets {
                                if function.contains(arm) {
                                    continue;
                                }
                                let target = section.address.wrapping_add(arm);
                                if let Some(&callee) = at.get(&target) {
                                    tail_edges.push((index, callee));
                                }
                            }
                        }
                        None => {
                            // A tail transfer through a pointer: this
                            // frame is replaced, but the chain lives on in
                            // something unknown.
                            if !seed[index] {
                                seeded_indirect_tail += 1;
                            }
                            seed[index] = true;
                        }
                    }
                }
                _ => {}
            }
        }
        // Running off the end continues into whatever begins there — a tail
        // edge no instruction stands for, same rule as `refusals`.
        let leaves = function.instructions.last().is_some_and(|last| {
            matches!(
                last.instruction.flow_control(),
                FlowControl::Return
                    | FlowControl::UnconditionalBranch
                    | FlowControl::IndirectBranch
                    | FlowControl::Exception
            )
        });
        if !leaves {
            let after = section.address + function.offset + function.size;
            if let Some(&next) = at.get(&after) {
                tail_edges.push((index, next));
            }
        }
    }

    // L's fixpoint: propagate from seeds to predecessors over both edge
    // kinds — a caller's chain is live through a call, and a tail
    // transferrer's *caller's* chain is live through a tail.
    let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); total];
    for &(from, to) in call_edges.iter().chain(tail_edges.iter()) {
        predecessors[to].push(from);
    }
    let mut l = seed.clone();
    let mut worklist: Vec<usize> = (0..total).filter(|&f| l[f]).collect();
    while let Some(function) = worklist.pop() {
        for &predecessor in &predecessors[function] {
            if !l[predecessor] {
                l[predecessor] = true;
                worklist.push(predecessor);
            }
        }
    }

    // needs(f): a non-tail call site whose callee has L re-enters f.
    for &(from, to) in &call_edges {
        if l[to] {
            needs[from] = true;
        }
    }

    let module_of = |name: &str| -> String {
        match name.split_once('!') {
            Some((module, _)) => module.to_string(),
            None => "(main)".to_string(),
        }
    };
    #[derive(Default)]
    struct Row {
        functions: usize,
        bytes: u64,
        needs: usize,
        needs_bytes: u64,
    }
    let mut rows: HashMap<String, Row> = HashMap::new();
    let mut all = Row::default();
    for (index, function) in object.functions.iter().enumerate() {
        let row = rows.entry(module_of(&function.name)).or_default();
        row.functions += 1;
        row.bytes += function.size;
        all.functions += 1;
        all.bytes += function.size;
        if needs[index] {
            row.needs += 1;
            row.needs_bytes += function.size;
            all.needs += 1;
            all.needs_bytes += function.size;
        }
    }

    println!(
        "{:40} {:>10} {:>10} {:>6} {:>12} {:>12} {:>6}",
        "module", "functions", "need body", "%", "bytes", "need bytes", "%"
    );
    let mut sorted: Vec<(String, Row)> = rows.into_iter().collect();
    sorted.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes));
    for (module, row) in &sorted {
        println!(
            "{:40} {:>10} {:>10} {:>5.1}% {:>12} {:>12} {:>5.1}%",
            module,
            row.functions,
            row.needs,
            100.0 * row.needs as f64 / row.functions.max(1) as f64,
            row.bytes,
            row.needs_bytes,
            100.0 * row.needs_bytes as f64 / row.bytes.max(1) as f64,
        );
    }
    println!(
        "{:40} {:>10} {:>10} {:>5.1}% {:>12} {:>12} {:>5.1}%",
        "TOTAL",
        all.functions,
        all.needs,
        100.0 * all.needs as f64 / all.functions.max(1) as f64,
        all.bytes,
        all.needs_bytes,
        100.0 * all.needs_bytes as f64 / all.bytes.max(1) as f64,
    );
    println!();
    println!(
        "resume bodies droppable: {} of {} functions, {} of {} x86 bytes ({:.1}%)",
        all.functions - all.needs,
        all.functions,
        all.bytes - all.needs_bytes,
        all.bytes,
        100.0 * (all.bytes - all.needs_bytes) as f64 / all.bytes.max(1) as f64,
    );
    println!(
        "seeds: {seeded_syscall} syscall, {seeded_indirect_call} indirect call, \
         {seeded_indirect_tail} indirect/unresolved tail, \
         {seeded_unresolved_call} unresolved direct call, {seeded_name} setjmp/longjmp by name"
    );
    Ok(())
}
