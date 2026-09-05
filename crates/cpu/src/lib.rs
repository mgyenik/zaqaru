//! The x86-64 machine: an interpreter, and the loop that drives it.
//!
//! This crate is the CPU under the kernel. The bet the container design
//! rests on is that a `syscall` is an ordinary call into more code linked
//! into the same module; the bet *this* crate makes is that the code on the
//! other side of the program counter can be data rather than something a
//! bake had to understand ahead of time. An interpreter decodes at the
//! actual program counter at run time: ground truth, no static analysis —
//! and every instruction the guest can reach is
//! reachable, including bytes it wrote a microsecond ago.
//!
//! The pieces, in the order they matter:
//!
//! - [`state::Tcb`] — the whole machine, as a struct. A context switch is a
//!   pointer swap and a snapshot is a copy.
//! - [`flags`] — the last flag-writing operation, remembered, and read back
//!   one question at a time.
//! - [`space::Space`] — the guest address space: linear memory, with page
//!   permissions and the code bitmap that makes self-modifying code correct.
//! - [`block::BlockCache`] — decoded runs of instructions, invalidated by
//!   any write to the pages they came from.
//! - [`exec::Cpu`] — what each instruction means.
//! - [`bytecode`] — an accelerator over the interpreter: a register-machine
//!   bytecode a block transpiles into, run by a switch-loop that keeps a
//!   loop's back-edge inside itself rather than re-entering the run loop per
//!   block. Anything it does not model defers to [`exec::Cpu`], so it only
//!   ever adds speed. On by default; [`block::BlockCache::interpreting`]
//!   switches it off for a run that wants the plain interpreter.
//! - [`Engine`] — the run loop: fetch a block, execute it, drain
//!   invalidations, count down the quantum.
//!
//! What this crate deliberately does *not* contain is a kernel. A `syscall`
//! stops the loop and hands the caller a `&mut Tcb`; what happens next —
//! every syscall row, the VFS, the overlay, the VMA tree — is the kernel's, and
//! unchanged by any of this.

pub mod block;
pub mod bytecode;
pub mod exec;
pub mod flags;
pub mod histogram;
pub mod profile;
pub mod quick;
pub mod space;
pub mod state;

#[cfg(not(target_arch = "wasm32"))]
pub mod arena;

use block::{BlockCache, FetchError};
use exec::{Cpu, Step, Trap};
use space::Space;
use state::{Staleness, Tcb};

/// How many instructions a thread retires before the loop takes control back.
///
/// Denominated in retired instructions rather than in time, and that is the
/// whole point: scheduling decisions become a pure function of execution, so
/// the same container with the same inputs produces the same interleaving —
/// *including under preemption*. A wall-clock quantum would make the
/// schedule depend on the host, and record and replay would stop agreeing.
pub const QUANTUM: u64 = 100_000;

/// Why [`Engine::run`] returned.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The quantum ran out with the thread still runnable. `rip` names the
    /// next instruction; call again to continue.
    Preempted,
    /// The thread reached a `syscall`. `rip` is past it and `%rcx` and
    /// `%r11` hold what the hardware would have left there, so the caller
    /// can serve the call and resume with nothing else to restore.
    Syscall,
    /// Something the guest can observe — a fault, an undefined instruction,
    /// a divide error — or an instruction the engine does not implement.
    Trap(Trap),
}

/// The interpreter, as mechanism.
///
/// **It holds no machine state at all**, and that is a decision rather than
/// an omission. The thread control block, the address space and the block
/// cache belong to the kernel: the kernel is what schedules threads, what
/// maps and unmaps pages, and what will one day fork a process — so the
/// kernel is what has to own them.
///
/// What that buys is not tidiness. The `mmap` rows have to reach the page
/// bitmaps, and the kernel's guest-memory writes have to reach the
/// invalidation hook, and both want a `&mut Space` from the same owner the
/// scheduler is. Handing the engine its own copy of any of it would mean a
/// second path to the same state, which is the shape every aliasing bug in
/// this project has had.
pub struct Engine;

impl Engine {
    /// Runs one thread until the quantum is exhausted, a syscall is
    /// reached, or something traps.
    ///
    /// The shape a junior developer should recognise from any virtual
    /// machine, and deliberately so: fetch a block, execute it, drain the
    /// invalidations any store in it queued, repeat.
    pub fn run(
        tcb: &mut Tcb,
        space: &mut Space,
        cache: &mut BlockCache,
        quantum: u64,
    ) -> Outcome {
        Self::run_with(tcb, space, cache, quantum, false)
    }

    /// Runs exactly `budget` instructions, or until a syscall or a trap,
    /// **interpreting every block**: the accelerator checks its budget only
    /// at a back-edge, so only the interpreter stops on the instruction
    /// asked for. The state at the stop is the state after that instruction
    /// whichever engine would have computed it, so this is how a debugger
    /// reaches an instant. What it also does is record whether the flags at
    /// that instant are the last instruction's — see
    /// [`state::Tcb::flags_staleness`].
    ///
    /// Not for continuing from: a run resumed here would preempt at a
    /// different point than the accelerated run did, and a recorded
    /// schedule would diverge. A debugger resumes from a checkpoint.
    pub fn run_exact(tcb: &mut Tcb, space: &mut Space, cache: &mut BlockCache, budget: u64) -> Outcome {
        Self::run_with(tcb, space, cache, budget, true)
    }

    fn run_with(
        tcb: &mut Tcb,
        space: &mut Space,
        cache: &mut BlockCache,
        quantum: u64,
        exact: bool,
    ) -> Outcome {
        let mut budget = quantum;
        tcb.flags_staleness = Staleness::Unknown;
        // The one rule a bytecode trace's defer imposes: the block it names
        // is run interpreted once before the trace is consulted for it
        // again. Without it a block that hands back its own entry is
        // re-entered forever. An exact run interprets everything.
        let mut interpret_next = exact;
        while budget > 0 {
            // Before anything is fetched, so a block decoded from bytes the
            // last block overwrote cannot exist.
            cache.drain_invalidations(space);
            let index = match cache.entry(tcb.rip, space) {
                Ok(index) => index,
                Err(FetchError::Fault(fault)) => return Outcome::Trap(Trap::Fault(fault)),
                Err(FetchError::Undefined { address }) => {
                    return Outcome::Trap(Trap::Undefined { address });
                }
            };
            let block = cache.block(index);
            // The bytecode accelerator, when the cache attached a trace and
            // this block was not just told to interpret (a defer): run the
            // trace, which keeps a loop's back-edge inside itself. It retires
            // into the control block and leaves exactly where the interpreter
            // would — an exit sets `rip` and the loop continues, a defer hands
            // one instruction back by forcing interpretation of the block at
            // `rip`, a fault traps, a preemption stops. Anything it cannot run
            // it declined at transpile time, so the interpreter below still
            // owns correctness.
            if !interpret_next
                && let Some(trace) = &block.trace
            {
                let before = tcb.retired;
                // The address cache is the block cache itself: an indirect
                // transfer whose target is already transpiled stays inside the
                // interpreter, so a `call`/`ret` and CPython's computed goto do
                // not round-trip this loop.
                let leave = crate::bytecode::run(
                    trace,
                    0,
                    tcb,
                    space,
                    budget,
                    crate::bytecode::Resolver::Cache(cache),
                );
                // What ran via the accelerator, so the run's statistics report
                // the bytecode's coverage — which says how much of a real
                // workload it covers and how much still defers.
                let ran = tcb.retired.wrapping_sub(before);
                tcb.accelerated = tcb.accelerated.wrapping_add(ran);
                budget = budget.saturating_sub(ran);
                match leave {
                    crate::bytecode::Leave::Exit | crate::bytecode::Leave::Preempted => continue,
                    crate::bytecode::Leave::Fault(fault) => {
                        return Outcome::Trap(Trap::Fault(fault));
                    }
                    crate::bytecode::Leave::Defer { .. } => {
                        // `rip` names the one instruction the trace could not
                        // run; interpret the block there once, then resume.
                        interpret_next = true;
                        continue;
                    }
                }
            }
            interpret_next = exact;
            let mut cpu = Cpu::new(tcb, space);
            let mut position = 0usize;
            // The last instruction of this block that retired, for the
            // staleness an exact run reports at its stop.
            let mut last: Option<usize> = None;
            while position < block.instructions.len() {
                let instruction = &block.instructions[position];
                if budget == 0 {
                    // Which `advance` may have left stale, and which is
                    // always this instruction's own address: whatever ran
                    // before it either fell through to here or branched
                    // here.
                    cpu.tcb.rip = instruction.ip();
                    break;
                }
                budget -= 1;
                // Anything that goes to the next instruction, which is
                // most of them. An extended block has conditional branches
                // in its middle, so this is decided per instruction rather
                // than once for the block.
                if !block.quick[position].checks_rip {
                    match cpu.advance(&block.quick[position], instruction) {
                        Ok(()) => {}
                        Err(trap) => {
                            cpu.tcb.rip = match trap {
                                Trap::Breakpoint { .. } => instruction.next_ip(),
                                _ => {
                                    cpu.tcb.retired -= 1;
                                    instruction.ip()
                                }
                            };
                            return Outcome::Trap(trap);
                        }
                    }
                    last = Some(position);
                    if cpu.space.has_dirty_code() {
                        // Fell through, so this is where execution is.
                        cpu.tcb.rip = instruction.next_ip();
                        break;
                    }
                    position += 1;
                    // A block that ends on a straight instruction falls out
                    // of its last one, and `advance` left `rip` behind.
                    if position == block.instructions.len() {
                        cpu.tcb.rip = instruction.next_ip();
                    }
                    continue;
                }
                match cpu.run(&block.quick[position], instruction) {
                    Ok(Step::Retired) => last = Some(position),
                    Ok(Step::Syscall) => return Outcome::Syscall,
                    Err(trap) => {
                        // Where `%rip` is left is the difference between a
                        // fault and a trap, and it is observable: a handler
                        // for a fault-class exception returns to the
                        // instruction that faulted and re-runs it, which is
                        // how a guard page or a copy-on-write mapping is
                        // made to work at all. A breakpoint is the other
                        // kind — `int3` completes, and the frame it builds
                        // points past itself.
                        cpu.tcb.rip = match trap {
                            Trap::Breakpoint { .. } => instruction.next_ip(),
                            _ => {
                                // And an instruction that faulted did not
                                // retire.
                                cpu.tcb.retired -= 1;
                                instruction.ip()
                            }
                        };
                        return Outcome::Trap(trap);
                    }
                }
                // A store has landed on a page some cached block was
                // decoded from — possibly this one. Leave now, whatever the
                // program counter did: the drain at the top of the loop is
                // what makes the next fetch see current bytes.
                //
                // Checked *before* the program counter is consulted, and
                // that order is the whole of it. A repeated string
                // instruction stays put, so a version of this test that
                // sits below the staying-put arm never runs for one — and a
                // `rep stosb` writing over its own instruction bytes would
                // go on executing the cached decode for the rest of its
                // count, which is exactly the stale execution this engine
                // promises is impossible. Hardware takes the new bytes at
                // the next iteration boundary.
                if cpu.space.has_dirty_code() {
                    break;
                }
                // Where the program counter landed says what to do next, and
                // says it without a second return channel. Staying put is a
                // repeated string instruction with iterations left; the next
                // address in the block is a fall-through; anything else is a
                // transfer, and the loop looks the target up.
                if cpu.tcb.rip == instruction.ip() {
                    continue;
                }
                if cpu.tcb.rip != instruction.next_ip() {
                    break;
                }
                position += 1;
            }
            if exact && budget == 0 {
                tcb.flags_staleness = staleness(block, last);
            }
        }
        Outcome::Preempted
    }
}

/// Whether the flags record at a stop is the last retired instruction's.
///
/// Walking back from the last instruction of the block that retired: a
/// writer whose flags were dead means the record was not written and is an
/// earlier one's; a full writer whose flags were live means the record is
/// its own; anything the fast path does not lower may or may not have
/// written flags, and so may the block before this one, which is where
/// the walk gives up.
fn staleness(block: &block::Block, last: Option<usize>) -> Staleness {
    let Some(last) = last else {
        return Staleness::Unknown;
    };
    for position in (0..=last).rev() {
        let quick = &block.quick[position];
        if quick.flags_dead {
            return Staleness::Stale;
        }
        if block::writes_flags(quick.op) {
            return Staleness::Fresh;
        }
        if quick.op == quick::Op::General {
            return Staleness::Unknown;
        }
    }
    Staleness::Unknown
}

#[cfg(test)]
mod tests {
    use iced_x86::code_asm::*;

    use super::*;
    use crate::arena::Arena;
    use crate::space::{PAGE_SIZE, Protection};
    use crate::state::Width;

    /// A guest program in a fresh address space, with a stack.
    ///
    /// The fixture holds the three pieces separately because that is what
    /// the kernel will do: the engine owns none of them.
    struct Guest {
        tcb: Tcb,
        space: Space,
        cache: BlockCache,
        _arena: Arena,
        entry: u64,
    }

    impl Guest {
        /// The builder is handed the address the program will live at,
        /// because a guest that reaches for an absolute address has to reach
        /// for one that exists — the arena is somewhere in the low address
        /// space, not at a constant.
        fn new(build: impl FnOnce(&mut CodeAssembler, u64)) -> Self {
            let arena = Arena::new(0x10_0000);
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
            // A stack at the top of the arena, sixteen-byte aligned as the
            // ABI leaves it.
            tcb.set_stack_pointer(arena.limit() - 0x100);
            Self {
                tcb,
                space,
                cache,
                _arena: arena,
                entry,
            }
        }

        fn run(&mut self) -> Outcome {
            self.step(QUANTUM)
        }

        fn step(&mut self, quantum: u64) -> Outcome {
            Engine::run(&mut self.tcb, &mut self.space, &mut self.cache, quantum)
        }

        fn rax(&self) -> u64 {
            self.tcb.registers[0]
        }
    }

    #[test]
    fn a_loop_runs_to_completion_and_leaves_the_answer_in_a_register() {
        // Sum one to ten.
        let mut guest = Guest::new(|assembler, _| {
            assembler.xor(rax, rax).unwrap();
            assembler.mov(rcx, 10u64).unwrap();
            let mut top = assembler.create_label();
            assembler.set_label(&mut top).unwrap();
            assembler.add(rax, rcx).unwrap();
            assembler.dec(rcx).unwrap();
            assembler.jne(top).unwrap();
            assembler.syscall().unwrap();
        });
        assert_eq!(guest.run(), Outcome::Syscall);
        assert_eq!(guest.rax(), 55);
    }

    #[test]
    fn a_call_and_a_return_walk_the_guest_stack() {
        let mut guest = Guest::new(|assembler, _| {
            let mut callee = assembler.create_label();
            let mut done = assembler.create_label();
            assembler.mov(rdi, 7u64).unwrap();
            assembler.call(callee).unwrap();
            assembler.jmp(done).unwrap();
            assembler.set_label(&mut callee).unwrap();
            assembler.mov(rax, rdi).unwrap();
            assembler.add(rax, rax).unwrap();
            assembler.ret().unwrap();
            assembler.set_label(&mut done).unwrap();
            assembler.syscall().unwrap();
        });
        assert_eq!(guest.run(), Outcome::Syscall);
        assert_eq!(guest.rax(), 14);
    }

    #[test]
    fn a_wild_store_is_a_fault_naming_the_address() {
        let mut guest = Guest::new(|assembler, _| {
            assembler.mov(rax, 0u64).unwrap();
            assembler.mov(qword_ptr(rax), rbx).unwrap();
        });
        assert_eq!(
            guest.run(),
            Outcome::Trap(Trap::Fault(space::Fault {
                address: 0,
                access: space::Access::Write,
            }))
        );
    }

    #[test]
    fn a_repeated_string_move_copies_and_stays_interruptible() {
        let mut guest = Guest::new(|assembler, _| {
            assembler.cld().unwrap();
            assembler.rep().movsb().unwrap();
            // Kept where the syscall cannot reach it: `syscall` writes the
            // return address into `%rcx`, exactly as hardware does.
            assembler.mov(r15, rcx).unwrap();
            assembler.syscall().unwrap();
        });
        let base = guest.entry + 0x1000;
        let source = base;
        let destination = base + 0x100;
        for offset in 0..16u64 {
            guest
                .space
                .store(source + offset, Width::Byte, offset)
                .unwrap();
        }
        guest.tcb.registers[6] = source;
        guest.tcb.registers[7] = destination;
        guest.tcb.registers[1] = 16;
        // A quantum small enough to land in the middle of the copy proves
        // the instruction is interruptible: `rip` is still on the `rep`.
        assert_eq!(guest.step(6), Outcome::Preempted);
        assert!(guest.tcb.registers[1] > 0);
        assert_eq!(guest.run(), Outcome::Syscall);
        assert_eq!(guest.tcb.registers[15], 0, "the count ran out");
        for offset in 0..16u64 {
            assert_eq!(
                guest.space.load(destination + offset, Width::Byte),
                Ok(offset)
            );
        }
    }

    /// A repeated instruction that overwrites *itself*.
    ///
    /// The narrow case the general one hides. A `rep stosb` stays put — the
    /// program counter does not move until the count runs out — so a
    /// staleness check placed after the "where did `%rip` land" arms never
    /// runs for one, and the instruction goes on executing its own cached
    /// decode over bytes it has already replaced. The engine's promise is
    /// that this cannot happen, and the promise is only worth what pins it.
    ///
    /// Here the store turns the `rep stosb` into a `ret`. Correct behaviour
    /// is that exactly *one* iteration retires: the store dirties the page,
    /// the loop leaves, the drain drops the block, and the re-fetch decodes
    /// the `ret` that is now there. With the check in the wrong place the
    /// count runs to zero instead, which is what the assertions below say.
    #[test]
    fn a_repeated_store_over_its_own_instruction_is_seen_at_once() {
        /// Enough iterations that finishing the run is unmistakable.
        const COUNT: u64 = 100;

        // The program has to name its own `rep stosb`, whose address is not
        // known until the arena is allocated. So it is assembled with a
        // placeholder and the immediate is patched afterwards — the
        // alternative, assembling twice, would put the two passes in
        // different arenas and aim the store at the wrong one.
        let mut guest = Guest::new(|assembler, _entry| {
            let mut victim = assembler.create_label();
            assembler.call(victim).unwrap();
            // Where the `ret` lands. The counters are copied out of harm's
            // way first: `syscall` writes the return address into `%rcx`,
            // exactly as hardware does.
            assembler.mov(r15, rcx).unwrap();
            assembler.mov(r14, rdi).unwrap();
            assembler.syscall().unwrap();
            assembler.set_label(&mut victim).unwrap();
            assembler.cld().unwrap();
            // `ret`, one byte, which is what the instruction below is about
            // to become.
            assembler.mov(al, 0xc3u32).unwrap();
            assembler.mov(rcx, COUNT).unwrap();
            assembler.mov(rdi, 0u64).unwrap();
            assembler.rep().stosb().unwrap();
            // Reached only if the `ret` never happens.
            assembler.ud2().unwrap();
        });

        let entry = guest.entry;
        let code = guest.space.fetch(entry, 1 << 12).unwrap().to_vec();
        let offset = code
            .windows(2)
            .position(|pair| pair == [0xf3, 0xaa])
            .expect("the program contains a `rep stosb`") as u64;
        let repeated = entry + offset;
        // `mov rdi, imm64` is `48 bf` and eight bytes of immediate, and it
        // is the instruction before the repeat.
        assert_eq!(
            &code[(offset - 10) as usize..(offset - 8) as usize],
            &[0x48, 0xbf],
            "the instruction before the repeat is not the one being patched"
        );
        guest
            .space
            .write(repeated - 8, &repeated.to_le_bytes())
            .expect("patch the target address");
        // The harness placing bytes is not the guest storing them.
        guest
            .cache
            .drain_invalidations(&mut guest.space);
        assert_eq!(guest.run(), Outcome::Syscall);
        assert_eq!(
            guest.tcb.registers[15],
            COUNT - 1,
            "the repeated store went on running after replacing itself"
        );
        assert_eq!(
            guest.tcb.registers[14],
            repeated + 1,
            "exactly one byte should have been stored"
        );
        // And what is there now is the `ret` that ran.
        assert_eq!(
            guest.space.load(repeated, Width::Byte),
            Ok(0xc3),
            "the store did not land"
        );
    }

    /// Self-modifying code: a guest writes a function into memory, calls
    /// it, rewrites it, and calls
    /// it again. Both calls must run the bytes that were there at the time.
    ///
    /// The second half is the whole test. With page invalidation removed,
    /// the second call re-runs the *cached* block and answers one — which is
    /// the stale-execution bug this machinery exists to make impossible, and
    /// the reason the assertion says so in its message.
    #[test]
    fn code_written_at_run_time_is_executed_as_written() {
        let mut guest = Guest::new(|assembler, entry| {
            // A page well clear of the program itself, so the writes below
            // cannot be mistaken for the program invalidating its own block.
            let scratch = entry + 4 * PAGE_SIZE;
            assembler.mov(rbx, scratch).unwrap();
            // `mov eax, 1` is b8 01 00 00 00, and `ret` is c3.
            assembler.mov(dword_ptr(rbx), 0x0000_01b8u32).unwrap();
            assembler.mov(byte_ptr(rbx + 4), 0x00u32).unwrap();
            assembler.mov(byte_ptr(rbx + 5), 0xc3u32).unwrap();
            assembler.call(rbx).unwrap();
            assembler.mov(rsi, rax).unwrap();
            // Rewrite the immediate and call the same address again.
            assembler.mov(dword_ptr(rbx), 0x0000_02b8u32).unwrap();
            assembler.call(rbx).unwrap();
            assembler.syscall().unwrap();
        });
        assert_eq!(guest.run(), Outcome::Syscall);
        assert_eq!(guest.tcb.registers[6], 1, "the first call");
        assert_eq!(
            guest.rax(),
            2,
            "the second call ran the bytes the first call left behind"
        );
    }

    /// Deep recursion — a call and a return per level — which the bytecode
    /// runs with the address cache keeping each `call` and `ret` inside the
    /// interpreter. The answer proves the whole chain of internal dispatches
    /// stayed faithful; with the `bytecode` feature it exercises the address
    /// cache, and without it the interpreter, and both must agree with the
    /// closed form.
    #[test]
    fn recursion_returns_the_right_answer() {
        // A recursive sum: f(n) = n + f(n-1), f(0) = 0, so f(n) = n(n+1)/2.
        let mut guest = Guest::new(|assembler, _| {
            let mut function = assembler.create_label();
            let mut base = assembler.create_label();
            // rdi = 100; call f; result in rax; syscall.
            assembler.mov(rdi, 100u64).unwrap();
            assembler.call(function).unwrap();
            assembler.syscall().unwrap();
            // f(n): if n == 0 return 0; else return n + f(n-1).
            assembler.set_label(&mut function).unwrap();
            assembler.test(rdi, rdi).unwrap();
            assembler.jz(base).unwrap();
            assembler.push(rdi).unwrap();
            assembler.dec(rdi).unwrap();
            assembler.call(function).unwrap();
            assembler.pop(rdi).unwrap();
            assembler.add(rax, rdi).unwrap();
            assembler.ret().unwrap();
            assembler.set_label(&mut base).unwrap();
            assembler.xor(rax, rax).unwrap();
            assembler.ret().unwrap();
        });
        assert_eq!(guest.run(), Outcome::Syscall);
        assert_eq!(guest.rax(), 100 * 101 / 2);
    }
}
