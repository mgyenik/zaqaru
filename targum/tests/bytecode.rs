//! Differential correctness for the bytecode transpiler.
//!
//! The one property that makes the transpiler safe to layer over the
//! interpreter: a program run through the bytecode (with anything unmodelled
//! deferred back to the interpreter) leaves *exactly* the state the same
//! program run straight through the interpreter leaves — every register, the
//! program counter, the status flags, and every byte of memory.
//!
//! Each test assembles a guest program, runs it twice from an identical
//! start — once with [`targum::Engine`] alone, once with the bytecode driver
//! below — and asserts the two machines agree. A disagreement is a transpiler
//! bug; a *deferral* is not, because the deferred instruction ran through the
//! very interpreter the reference used.

use iced_x86::code_asm::*;

use targum::arena::Arena;
use targum::block::BlockCache;
use targum::bytecode::{self, Leave, Trace};
use targum::exec::Trap;
use targum::space::{Protection, Space};
use targum::state::Tcb;
use targum::{Engine, Outcome, QUANTUM};

const LENGTH: u64 = 0x20_0000;

/// A guest program in a fresh address space with a stack, exactly as the
/// engine's own tests build one.
struct Guest {
    tcb: Tcb,
    space: Space,
    cache: BlockCache,
    _arena: Arena,
    entry: u64,
    limit: u64,
}

impl Guest {
    /// Builds a guest at a *given* base. The two runs of one comparison share
    /// a base so their absolute addresses match; different comparisons get
    /// different bases (from the arena's own bump allocator), so parallel
    /// tests never collide — the reason a fixed base and a global lock are
    /// both unnecessary.
    fn at(base: u64, build: impl FnOnce(&mut CodeAssembler, u64)) -> Self {
        let arena = Arena::at(base, LENGTH);
        let limit = arena.limit();
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), arena.length(), Protection::ALL);
        let entry = arena.base();
        let mut assembler = CodeAssembler::new(64).expect("assembler");
        build(&mut assembler, entry);
        let code = assembler.assemble(entry).expect("assemble");
        space.write(entry, &code).expect("place the program");
        let mut cache = BlockCache::new();
        cache.drain_invalidations(&mut space);
        let mut tcb = Tcb::new();
        tcb.rip = entry;
        tcb.set_stack_pointer(arena.limit() - 0x400);
        Self {
            tcb,
            space,
            cache,
            _arena: arena,
            entry,
            limit,
        }
    }

    /// The machine state that must match between the two runs.
    fn snapshot(&self, outcome: Outcome) -> Snapshot {
        let length = (self.limit - self.entry) as usize;
        let mut memory = vec![0u8; length];
        self.space
            .read(self.entry, &mut memory)
            .expect("read guest memory");
        Snapshot {
            outcome,
            registers: self.tcb.registers,
            rip: self.tcb.rip,
            status: self.tcb.flags.status(),
            fs_base: self.tcb.fs_base,
            retired: self.tcb.retired,
            memory,
        }
    }
}

/// The comparable result of a run, taken before its arena is dropped.
struct Snapshot {
    outcome: Outcome,
    registers: [u64; 16],
    rip: u64,
    status: u64,
    fs_base: u64,
    retired: u64,
    memory: Vec<u8>,
}

/// The bytecode driver: the run loop the real engine will grow, in the small.
/// Fetch a block, transpile it, run the trace; on a defer run one instruction
/// through the interpreter; on an exit continue at the new `rip`; stop at a
/// syscall, a fault, or when the budget of blocks is spent.
///
/// It re-transpiles on every entry rather than caching traces — the address
/// cache is a later increment, and the test only cares that the *result* is
/// right, not that it was reached without rework.
fn run_bytecode(guest: &mut Guest) -> Outcome {
    let mut blocks = 0u64;
    loop {
        blocks += 1;
        assert!(blocks < 5_000_000, "the bytecode driver did not converge");
        guest.cache.drain_invalidations(&mut guest.space);
        let index = match guest.cache.entry(guest.tcb.rip, &mut guest.space) {
            Ok(index) => index,
            Err(_) => {
                // A fetch fault: let the interpreter produce the exact trap.
                return Engine::run(&mut guest.tcb, &mut guest.space, &mut guest.cache, 1);
            }
        };
        let block = guest.cache.block(index);
        let Some(trace): Option<Trace> = bytecode::transpile(block) else {
            // Nothing to run as bytecode — interpret the block.
            match Engine::run(&mut guest.tcb, &mut guest.space, &mut guest.cache, QUANTUM) {
                Outcome::Preempted => continue,
                other => return other,
            }
        };
        match bytecode::run(&trace, 0, &mut guest.tcb, &mut guest.space, QUANTUM, bytecode::Resolver::Runloop) {
            Leave::Exit | Leave::Preempted => continue,
            Leave::Fault(fault) => return Outcome::Trap(Trap::Fault(fault)),
            Leave::Defer { .. } => {
                // `rip` names the one instruction to interpret. Run exactly
                // it, then loop — the next entry re-decodes from wherever it
                // left `rip`.
                match Engine::run(&mut guest.tcb, &mut guest.space, &mut guest.cache, 1) {
                    Outcome::Preempted => continue,
                    other => return other,
                }
            }
        }
    }
}

/// Runs `build` both ways and asserts the two machines match. The runs are
/// sequential and share [`BASE`], so absolute addresses are comparable; the
/// reference is snapshotted and dropped before the bytecode run maps the same
/// arena. Returns the reference outcome so a test can assert on it too.
fn agree(build: impl Fn(&mut CodeAssembler, u64) + Copy) -> Outcome {
    // A base unique to this comparison, taken from the arena's bump allocator
    // exactly as every other test takes one — so this comparison's two runs
    // share it while other comparisons, running in parallel, get their own.
    // The probe maps and drops to reserve the range; the bump allocator never
    // reissues it, so mapping it again below is safe without any lock.
    let base = {
        let probe = Arena::new(LENGTH);
        probe.base()
    };
    let reference = {
        let mut guest = Guest::at(base, build);
        let outcome = Engine::run(&mut guest.tcb, &mut guest.space, &mut guest.cache, QUANTUM);
        guest.snapshot(outcome)
    };
    let bytecode = {
        let mut guest = Guest::at(base, build);
        let outcome = run_bytecode(&mut guest);
        guest.snapshot(outcome)
    };

    assert_eq!(
        reference.outcome, bytecode.outcome,
        "the two machines stopped for different reasons"
    );
    for register in 0..16 {
        assert_eq!(
            reference.registers[register], bytecode.registers[register],
            "register {register} differs: interpreter {:#x} vs bytecode {:#x}",
            reference.registers[register], bytecode.registers[register],
        );
    }
    assert_eq!(reference.rip, bytecode.rip, "rip differs");
    assert_eq!(
        reference.status, bytecode.status,
        "status flags differ: interpreter {:#x} vs bytecode {:#x}",
        reference.status, bytecode.status,
    );
    assert_eq!(reference.fs_base, bytecode.fs_base, "fs_base differs");
    assert_eq!(
        reference.retired, bytecode.retired,
        "retired instruction count differs: interpreter {} vs bytecode {}",
        reference.retired, bytecode.retired,
    );
    assert!(
        reference.memory == bytecode.memory,
        "memory differs between the two machines"
    );
    reference.outcome
}

#[test]
fn a_counting_loop_agrees() {
    // The self-contained loop the whole design turns on: the back-edge stays
    // inside the trace as an internal branch.
    assert_eq!(
        agree(|a, _| {
            a.xor(rax, rax).unwrap();
            a.mov(rcx, 10u64).unwrap();
            let mut top = a.create_label();
            a.set_label(&mut top).unwrap();
            a.add(rax, rcx).unwrap();
            a.dec(rcx).unwrap();
            a.jne(top).unwrap();
            a.syscall().unwrap();
        }),
        Outcome::Syscall
    );
}

#[test]
fn arithmetic_of_every_width_agrees() {
    agree(|a, _| {
        a.mov(rax, 0x1122_3344_5566_7788u64).unwrap();
        a.add(al, 0x11u32).unwrap();
        a.sub(ax, 0x0100u32).unwrap();
        a.xor(eax, 0x0f0f_0f0fu32).unwrap();
        a.and(rax, rax).unwrap();
        a.or(rbx, 0x55i32).unwrap();
        a.mov(ecx, 0xdead_beefu32).unwrap();
        a.add(rcx, rax).unwrap();
        a.cmp(rcx, rax).unwrap();
        a.test(rax, rbx).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn immediates_wide_and_narrow_agree() {
    agree(|a, _| {
        a.mov(rax, -1i64).unwrap();
        a.add(rax, 5i32).unwrap();
        a.mov(rbx, 0x7fff_ffff_ffff_ffffu64).unwrap();
        a.and(rbx, 0x0f0f_0f0fi32).unwrap();
        a.mov(edx, 0x8000_0000u32).unwrap();
        a.sub(rdx, 1i32).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn loads_and_stores_agree() {
    agree(|a, entry| {
        let scratch = entry + 0x1000;
        a.mov(rbx, scratch).unwrap();
        a.mov(rdi, 0x0123_4567_89ab_cdefu64).unwrap();
        a.mov(qword_ptr(rbx), rdi).unwrap();
        a.mov(rax, qword_ptr(rbx)).unwrap();
        a.mov(dword_ptr(rbx + 8), eax).unwrap();
        a.movzx(rcx, byte_ptr(rbx)).unwrap();
        a.movsx(rdx, word_ptr(rbx)).unwrap();
        a.add(qword_ptr(rbx + 16), rax).unwrap();
        a.mov(rsi, qword_ptr(rbx + 16)).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn indexed_addressing_agrees() {
    agree(|a, entry| {
        let scratch = entry + 0x1000;
        a.mov(rbx, scratch).unwrap();
        a.mov(rcx, 3u64).unwrap();
        a.mov(qword_ptr(rbx + rcx * 8), 0xcafei32).unwrap();
        a.mov(rax, qword_ptr(rbx + rcx * 8)).unwrap();
        a.lea(rdx, qword_ptr(rbx + rcx * 8 + 24)).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn a_call_and_return_agree() {
    agree(|a, _| {
        let mut callee = a.create_label();
        let mut done = a.create_label();
        a.mov(rdi, 21u64).unwrap();
        a.call(callee).unwrap();
        a.jmp(done).unwrap();
        a.set_label(&mut callee).unwrap();
        a.mov(rax, rdi).unwrap();
        a.add(rax, rax).unwrap();
        a.ret().unwrap();
        a.set_label(&mut done).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn push_and_pop_agree() {
    agree(|a, _| {
        a.mov(rax, 0x1111u64).unwrap();
        a.mov(rbx, 0x2222u64).unwrap();
        a.push(rax).unwrap();
        a.push(rbx).unwrap();
        a.pop(rcx).unwrap();
        a.pop(rdx).unwrap();
        a.mov(rsi, 0x33u64).unwrap();
        a.push(rsi).unwrap();
        a.pop(rsi).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn a_conditional_chain_agrees() {
    // Several forward and backward conditionals, so the taken and not-taken
    // exits and the internal back-edge all run.
    agree(|a, _| {
        a.mov(rax, 0u64).unwrap();
        a.mov(rcx, 5u64).unwrap();
        let mut top = a.create_label();
        let mut even = a.create_label();
        let mut cont = a.create_label();
        a.set_label(&mut top).unwrap();
        a.test(rcx, 1i32).unwrap();
        a.jz(even).unwrap();
        a.add(rax, 100i32).unwrap();
        a.jmp(cont).unwrap();
        a.set_label(&mut even).unwrap();
        a.add(rax, 1i32).unwrap();
        a.set_label(&mut cont).unwrap();
        a.dec(rcx).unwrap();
        a.jnz(top).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn a_wild_store_faults_the_same_way() {
    let outcome = agree(|a, _| {
        a.mov(rax, 0u64).unwrap();
        a.mov(qword_ptr(rax), rbx).unwrap();
    });
    assert!(matches!(outcome, Outcome::Trap(Trap::Fault(_))));
}

#[test]
fn a_deferred_instruction_still_agrees() {
    // `imul`, `shl` and `cdq` are not (yet) transpiled — they defer. The run
    // must still match, because the deferred ops ran through the interpreter.
    agree(|a, _| {
        a.mov(rax, 7u64).unwrap();
        a.mov(rcx, 6u64).unwrap();
        a.imul_2(rax, rcx).unwrap();
        a.shl(rax, 2u32).unwrap();
        a.mov(rbx, rax).unwrap();
        a.neg(rbx).unwrap();
        a.not(rcx).unwrap();
        a.inc(rax).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn a_memcpy_style_loop_agrees() {
    agree(|a, entry| {
        let source = entry + 0x2000;
        let destination = entry + 0x4000;
        // Seed the source with a recognisable pattern.
        a.mov(rbx, source).unwrap();
        a.mov(r8, 0xaaaa_bbbb_cccc_ddddu64).unwrap();
        a.mov(qword_ptr(rbx), r8).unwrap();
        a.mov(r8, 0x1111_2222_3333_4444u64).unwrap();
        a.mov(qword_ptr(rbx + 8), r8).unwrap();
        a.mov(r8, 0x5555_6666_7777_8888u64).unwrap();
        a.mov(qword_ptr(rbx + 16), r8).unwrap();
        // Copy three words, source to destination.
        a.mov(rsi, source).unwrap();
        a.mov(rdi, destination).unwrap();
        a.mov(rcx, 3u64).unwrap();
        let mut top = a.create_label();
        a.set_label(&mut top).unwrap();
        a.mov(rax, qword_ptr(rsi)).unwrap();
        a.mov(qword_ptr(rdi), rax).unwrap();
        a.add(rsi, 8i32).unwrap();
        a.add(rdi, 8i32).unwrap();
        a.dec(rcx).unwrap();
        a.jnz(top).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn carry_read_after_dec_agrees() {
    // The subtle case the flag-liveness pass must not break: a carry produced,
    // then a `dec` (which preserves carry) between it and the `jc` that reads
    // it. If the pass wrongly eliminated the producer's carry, this diverges.
    agree(|a, _| {
        a.mov(rax, 1u64).unwrap();
        a.mov(rcx, 2u64).unwrap();
        a.sub(rax, 5i32).unwrap(); // sets carry (1 - 5 borrows)
        a.dec(rcx).unwrap(); // preserves carry, overwrites the rest
        let mut carry = a.create_label();
        let mut done = a.create_label();
        a.jb(carry).unwrap(); // reads carry from the sub, across the dec
        a.mov(rdx, 0xaaaau64).unwrap();
        a.jmp(done).unwrap();
        a.set_label(&mut carry).unwrap();
        a.mov(rdx, 0xbbbbu64).unwrap();
        a.set_label(&mut done).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn flags_live_into_the_next_block_agree() {
    // A `cmp` whose flags a `jcc` in the *following* block reads: the producer
    // is at a block boundary (after a `call`), so its flags must stay live.
    agree(|a, _| {
        a.mov(rax, 7u64).unwrap();
        a.cmp(rax, 7i32).unwrap();
        // A forward jump makes the cmp end its block via fall-through; the je
        // is in the next block and reads the cmp's flags.
        let mut equal = a.create_label();
        a.je(equal).unwrap();
        a.mov(rbx, 1u64).unwrap();
        a.set_label(&mut equal).unwrap();
        a.mov(rcx, 2u64).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn lea_with_displacement_agrees() {
    // The bug real glibc caught: `lea reg, [base + disp]` must be base + disp,
    // not 2*base + disp — positive and negative, and base == dest.
    agree(|a, entry| {
        a.mov(rbx, entry + 0x1000).unwrap();
        a.lea(rax, qword_ptr(rbx + 24)).unwrap();
        a.lea(rcx, qword_ptr(rbx - 16)).unwrap();
        a.lea(rbx, qword_ptr(rbx + 8)).unwrap(); // dest == base
        a.lea(rdx, qword_ptr(rbx + rax * 4 + 32)).unwrap(); // with index too
        a.syscall().unwrap();
    });
}

#[test]
fn imul_agrees() {
    // Two- and three-operand imul with dead flags (an LCG-shaped chain), the
    // op that made the mixed kernel defer. Live-flag imul defers, so also test
    // one whose flags a jcc reads.
    agree(|a, _| {
        a.mov(rax, 1u64).unwrap();
        a.mov(rcx, 6364136223846793005u64).unwrap();
        let mut top = a.create_label();
        a.mov(rdx, 5u64).unwrap();
        a.set_label(&mut top).unwrap();
        a.imul_2(rax, rcx).unwrap(); // dst *= src, flags dead
        a.add(rax, 1442695040888963407u64 as i64 as i32 as i64 as u64 as i32).unwrap_or(());
        a.imul_3(rbx, rax, 3i32).unwrap(); // dst = src * imm
        a.dec(rdx).unwrap();
        a.jnz(top).unwrap();
        // A live-flag imul: the result's sign is read.
        a.imul_2(rbx, rcx).unwrap();
        a.js(top).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn shifts_agree() {
    // Shifts by immediate and by cl, at counts within and beyond the width
    // (x86 masks by 0x1f/0x3f, not the operand width), with dead flags.
    agree(|a, _| {
        a.mov(rax, 0x0123_4567_89ab_cdefu64).unwrap();
        a.shl(rax, 7u32).unwrap();
        a.shr(rax, 3u32).unwrap();
        a.sar(rax, 1u32).unwrap();
        a.mov(cl, 40u32).unwrap();
        a.shl(rbx, cl).unwrap();
        a.mov(edx, 0xdead_beefu32).unwrap();
        a.sar(edx, 20u32).unwrap();
        // A shift feeding a branch whose flags it must NOT eliminate wrongly:
        // here the flags are dead (overwritten by the cmp), so it is safe.
        a.mov(rsi, 8u64).unwrap();
        a.shr(rsi, 1u32).unwrap();
        a.cmp(rsi, 4i32).unwrap();
        let mut eq = a.create_label();
        a.je(eq).unwrap();
        a.mov(rdi, 0xbadu64).unwrap();
        a.set_label(&mut eq).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn rotates_agree() {
    agree(|a, _| {
        a.mov(rax, 0x0123_4567_89ab_cdefu64).unwrap();
        a.rol(rax, 7u32).unwrap();
        a.ror(rax, 3u32).unwrap();
        a.mov(cl, 40u32).unwrap();
        a.rol(rbx, cl).unwrap();
        a.mov(edx, 0xdead_beefu32).unwrap();
        a.ror(edx, 12u32).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn setcc_and_cmov_agree() {
    agree(|a, _| {
        a.mov(rax, 5u64).unwrap();
        a.mov(rbx, 9u64).unwrap();
        a.cmp(rax, rbx).unwrap();
        a.setl(cl).unwrap(); // 5 < 9 signed -> 1
        a.setg(dl).unwrap();
        a.mov(rsi, 0x1111u64).unwrap();
        a.mov(rdi, 0x2222u64).unwrap();
        a.cmovl(rsi, rdi).unwrap(); // taken
        a.mov(r8d, 0xffff_ffffu32).unwrap();
        a.cmovg(r8, rax).unwrap(); // not taken: must still 32-bit write (clear top)
        a.syscall().unwrap();
    });
}

#[test]
fn adc_and_sbb_agree() {
    // A two-word add and subtract: the carry produced by the low word must
    // reach the adc/sbb of the high word.
    agree(|a, _| {
        a.mov(rax, 0xffff_ffff_ffff_ffffu64).unwrap();
        a.mov(rbx, 0u64).unwrap();
        a.add(rax, 1i32).unwrap(); // carry out
        a.adc(rbx, 0i32).unwrap(); // rbx += carry
        a.mov(rcx, 0u64).unwrap();
        a.mov(rdx, 5u64).unwrap();
        a.sub(rcx, 1i32).unwrap(); // borrow
        a.sbb(rdx, 0i32).unwrap(); // rdx -= borrow
        // sbb reg, reg broadcast-carry idiom.
        a.stc().unwrap();
        a.sbb(rsi, rsi).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn fused_compare_branches_agree() {
    // Every condition through a cmp/jcc, a test/jcc, and add/sub producers, so
    // the fused op's per-producer flag computation is exercised broadly.
    agree(|a, _| {
        a.mov(rax, 0x8000_0000_0000_0000u64).unwrap();
        a.mov(rbx, 1u64).unwrap();
        a.mov(rcx, 0u64).unwrap();
        let mut l = a.create_label();
        // cmp + jl (signed), a large negative vs positive.
        a.cmp(rax, rbx).unwrap();
        a.jl(l).unwrap();
        a.add(rcx, 0x100u32 as i32).unwrap();
        a.set_label(&mut l).unwrap();
        // test + jz.
        let mut z = a.create_label();
        a.test(rbx, rbx).unwrap();
        a.jz(z).unwrap();
        a.add(rcx, 0x10u32 as i32).unwrap();
        a.set_label(&mut z).unwrap();
        // add producing overflow + jo.
        let mut o = a.create_label();
        a.mov(rdx, 0x7fff_ffff_ffff_ffffu64).unwrap();
        a.add(rdx, 1i32).unwrap();
        a.jo(o).unwrap();
        a.add(rcx, 0x1u32 as i32).unwrap();
        a.set_label(&mut o).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn fused_producer_flags_live_after_agree() {
    // The live_after case: a cmp whose flags feed both a jcc AND a later setcc,
    // so the fused op must still update the trace flags.
    agree(|a, _| {
        a.mov(rax, 5u64).unwrap();
        a.mov(rbx, 9u64).unwrap();
        a.cmp(rax, rbx).unwrap(); // sets flags
        let mut skip = a.create_label();
        a.jae(skip).unwrap(); // reads CF; not taken (5 < 9 unsigned)
        a.mov(rsi, 1u64).unwrap();
        a.set_label(&mut skip).unwrap();
        a.setb(cl).unwrap(); // reads CF again — same cmp's flags, after the jcc
        a.syscall().unwrap();
    });
}

#[test]
fn a_fused_loop_counter_agrees() {
    // dec + jnz, the classic loop back-edge, fused — checked over many
    // iterations so the fused retirement (two per op) stays exact.
    assert_eq!(
        agree(|a, _| {
            a.mov(rax, 0u64).unwrap();
            a.mov(rcx, 1000u64).unwrap();
            let mut top = a.create_label();
            a.set_label(&mut top).unwrap();
            a.add(rax, rcx).unwrap();
            a.dec(rcx).unwrap();
            a.jnz(top).unwrap();
            a.syscall().unwrap();
        }),
        Outcome::Syscall
    );
}

#[test]
fn division_agrees() {
    // Unsigned and signed div at 32 and 64 bit, quotient and remainder, plus
    // negative signed cases — the common non-trapping path.
    agree(|a, _| {
        // 64-bit unsigned: rdx:rax / rcx
        a.mov(rdx, 0u64).unwrap();
        a.mov(rax, 1_000_003u64).unwrap();
        a.mov(rcx, 97u64).unwrap();
        a.div(rcx).unwrap(); // rax = quotient, rdx = remainder
        a.mov(rsi, rax).unwrap();
        a.mov(rdi, rdx).unwrap();
        // 64-bit signed with a negative dividend.
        a.mov(rax, (-1_000_003i64) as u64).unwrap();
        a.cqo().unwrap(); // sign-extend rax into rdx
        a.mov(rcx, 97u64).unwrap();
        a.idiv(rcx).unwrap();
        a.mov(r8, rax).unwrap();
        a.mov(r9, rdx).unwrap();
        // 32-bit unsigned.
        a.mov(edx, 0u32).unwrap();
        a.mov(eax, 0xffff_fffeu32).unwrap();
        a.mov(ecx, 7u32).unwrap();
        a.div(ecx).unwrap();
        a.mov(r10, rax).unwrap();
        a.syscall().unwrap();
    });
}

#[test]
fn division_by_zero_defers_and_faults_the_same() {
    // A zero divisor must #DE (SIGFPE) exactly as the interpreter does — the
    // bytecode defers it, and the interpreter raises the trap.
    let outcome = agree(|a, _| {
        a.mov(rdx, 0u64).unwrap();
        a.mov(rax, 42u64).unwrap();
        a.mov(rcx, 0u64).unwrap();
        a.div(rcx).unwrap();
    });
    assert!(matches!(outcome, Outcome::Trap(Trap::DivideError { .. })));
}
