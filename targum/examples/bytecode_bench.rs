//! The go/no-go measurement for the bytecode transpiler.
//!
//! One hot loop — a masked, permission-checked load and store with an
//! accumulate, ending in a `dec`/`jnz` back-edge — run to the same
//! instruction count two ways: straight through the x86 interpreter, and
//! through the transpiled bytecode. The ratio is the multiplier the design
//! rests on.
//!
//! **This is a native measurement.** The design's 7.7× floor was measured
//! under wasmtime, where the dispatch and bounds-check costs differ from
//! native; a native ratio is a real signal but not the wasm one, and the
//! next measurement is the same kernel under wasmtime. Run with
//! `cargo run --release -p targum --example bytecode_bench`.

use std::time::Instant;

use iced_x86::code_asm::*;

use targum::arena::Arena;
use targum::block::BlockCache;
use targum::bytecode::{self, Leave};
use targum::space::{Protection, Space};
use targum::state::Tcb;
use targum::{Engine, Outcome};

/// The loop body, assembled at `entry`. `rsi` is a 16 KB buffer, `rcx` the
/// iteration counter and index source, `rax` an accumulator. Every op is one
/// the transpiler covers, so the whole loop is a single block whose back-edge
/// stays inside the bytecode.
fn build(assembler: &mut CodeAssembler, buffer: u64, iterations: u64) {
    assembler.mov(rsi, buffer).unwrap();
    assembler.mov(rcx, iterations).unwrap();
    assembler.xor(rax, rax).unwrap();
    let mut top = assembler.create_label();
    assembler.set_label(&mut top).unwrap();
    assembler.mov(r8, rcx).unwrap();
    assembler.and(r8d, 0x3ff8u32).unwrap(); // mask into the buffer, 8-aligned
    assembler.mov(r9, qword_ptr(rsi + r8)).unwrap(); // checked load
    assembler.xor(rax, r9).unwrap();
    assembler.add(r9, rax).unwrap();
    assembler.mov(qword_ptr(rsi + r8), r9).unwrap(); // checked store
    assembler.dec(rcx).unwrap();
    assembler.jnz(top).unwrap();
    assembler.syscall().unwrap();
}

struct Guest {
    tcb: Tcb,
    space: Space,
    cache: BlockCache,
    _arena: Arena,
}

impl Guest {
    fn new(base: u64, iterations: u64) -> Self {
        let arena = Arena::at(base, 0x8_0000);
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), arena.length(), Protection::ALL);
        let entry = arena.base();
        let buffer = entry + 0x2_0000;
        let mut assembler = CodeAssembler::new(64).expect("assembler");
        build(&mut assembler, buffer, iterations);
        let code = assembler.assemble(entry).expect("assemble");
        space.write(entry, &code).expect("place the program");
        // Seed the buffer so the load/xor/store loop does real work rather
        // than churning zeros.
        for i in 0..2048u64 {
            let v = i.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1);
            space.store(buffer + i * 8, targum::state::Width::Qword, v).unwrap();
        }
        let mut cache = BlockCache::new();
        cache.drain_invalidations(&mut space);
        let mut tcb = Tcb::new();
        tcb.rip = entry;
        tcb.set_stack_pointer(arena.limit() - 0x400);
        Self { tcb, space, cache, _arena: arena }
    }
}

fn main() {
    let iterations: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000_000);

    // Two separate bases so the two guests never contend for one range.
    let base_a = Arena::new(0x8_0000).base();
    let base_b = Arena::new(0x8_0000).base();

    // --- the interpreter ---
    let mut interp = Guest::new(base_a, iterations);
    let start = Instant::now();
    let outcome = Engine::run(&mut interp.tcb, &mut interp.space, &mut interp.cache, u64::MAX);
    let interp_secs = start.elapsed().as_secs_f64();
    assert_eq!(outcome, Outcome::Syscall, "the interpreter run did not reach the syscall");
    let interp_retired = interp.tcb.retired;

    // --- the bytecode ---
    let mut byte = Guest::new(base_b, iterations);
    // Transpile the loop block once, outside the timing: the whole loop runs
    // inside one `bytecode::run` because the back-edge is internal.
    byte.cache.drain_invalidations(&mut byte.space);
    let index = byte.cache.entry(byte.tcb.rip, &mut byte.space).expect("decode the loop block");
    let trace = bytecode::transpile(byte.cache.block(index)).expect("transpile the loop block");

    let start = Instant::now();
    // The loop exits (rcx == 0) straight to the syscall's address.
    let mut leaves = 0u64;
    loop {
        match bytecode::run(&trace, 0, &mut byte.tcb, &mut byte.space, u64::MAX) {
            // The loop falls through to the terminal `syscall`, which defers —
            // that is the loop finishing, and the end of the timed region.
            Leave::Defer { .. } | Leave::Exit => break,
            Leave::Preempted => {
                leaves += 1;
                continue;
            }
            other => panic!("unexpected bytecode leave: {other:?}"),
        }
    }
    let byte_secs = start.elapsed().as_secs_f64();
    let byte_retired = byte.tcb.retired;

    let _ = leaves;
    // The interpreter run also retires the terminal syscall; the bytecode run
    // stops as it defers to it. So the counts differ by at most that one, and
    // the real check is that the loop's work — the accumulator — matches.
    assert!(
        interp_retired.abs_diff(byte_retired) <= 2,
        "the two runs retired very different counts: {interp_retired} vs {byte_retired}"
    );
    assert_eq!(
        interp.tcb.registers[0], byte.tcb.registers[0],
        "the two runs left a different accumulator: {:#x} vs {:#x}",
        interp.tcb.registers[0], byte.tcb.registers[0],
    );

    let interp_mips = interp_retired as f64 / interp_secs / 1e6;
    let byte_mips = byte_retired as f64 / byte_secs / 1e6;
    println!(
        "loop: {} guest instructions, accumulator {:#x}",
        interp_retired, interp.tcb.registers[0]
    );
    println!("  interpreter: {interp_secs:.3}s = {interp_mips:8.1} MIPS");
    println!("  bytecode:    {byte_secs:.3}s = {byte_mips:8.1} MIPS");
    println!("  multiplier:  {:.2}x  (native — see the module header)", byte_mips / interp_mips);
}
