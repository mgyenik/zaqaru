//! A register-machine bytecode, and a switch-loop interpreter for it, that
//! runs x86-64 faster than interpreting x86 directly.
//!
//! The measurement this rests on is in `docs/bytecode-plan.md` and
//! `tools/bytecode-floor/`: a minimal register-machine bytecode interpreter,
//! under wasmtime, runs a realistic mixed kernel several times faster than
//! the same work interpreted as x86 through [`crate::quick`] — because the
//! stream is flat (a direct branch is `pc = offset`, not a re-entry of the
//! run loop), the dispatch is dense (a `br_table` over a `u8`, not a
//! seventeen-hundred-way match), and the operands are already resolved (no
//! `iced` re-derivation per execution).
//!
//! **This is an accelerator layered over the interpreter, never a
//! replacement.** The one architectural rule, from which the whole safety
//! argument follows: any x86 instruction the [`transpile`] step does not
//! model becomes a [`Op::Defer`], which hands that one instruction back to
//! [`crate::exec::Cpu`]. Correctness is the interpreter's; the bytecode only
//! ever *adds speed* on the ops it covers. A bug in this module can make the
//! engine slower or make a covered op wrong — which is what the differential
//! harness (`tests/bytecode.rs`) exists to catch — but a *missing* op is not
//! a correctness hole, it is a deferral.
//!
//! The state the bytecode runs on is the engine's own: the [`Tcb`]'s
//! registers and flags, and the [`Space`] for memory — so a load faults, a
//! store dirties a code page, and a flag reads back exactly as the
//! interpreter's would, because it *is* the interpreter's.

use crate::flags::{Condition, Rule};
use crate::space::{Fault, Space};
use crate::state::{Tcb, Width};

pub mod transpile;

pub use transpile::transpile;

/// How many registers the bytecode addresses: sixteen guest general-purpose
/// registers, aliased one-to-one onto [`Tcb::registers`], and eight scratch
/// registers the transpiler uses for address arithmetic and constant
/// materialisation within a single guest instruction.
///
/// Five bits of register field, so twenty-four of the thirty-two encodable
/// numbers are live. Scratch registers are dead at every guest-instruction
/// boundary, so no liveness analysis crosses one.
pub const REGISTERS: usize = 24;

/// The first scratch register. Registers below this alias the guest file;
/// registers from here up are the transpiler's temporaries.
pub const SCRATCH: usize = 16;

/// One decoded instruction of the bytecode.
///
/// The op is the low byte; the rest of the first word carries the register
/// fields, a width class, a condition, an immediate, and two one-bit
/// modifiers. A handful of ops (`Li64`, and any op needing a full-width
/// constant) spill a second word — see [`Word`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Op {
    /// Leave the trace: `rip = regs[d]`, hand back to the run loop. The
    /// out-of-trace branch, the indirect jump, the `call`, the `ret` — every
    /// transfer whose target the trace does not contain. Retires.
    ExitTo = 0,
    /// Run one guest instruction — the one at the guest address in the
    /// following word — through the interpreter, then re-enter the trace at
    /// `pc`. The escape hatch that makes the module safe: anything the
    /// transpiler declines becomes this. Does *not* retire here; the
    /// interpreter retires it.
    Defer = 1,
    /// `pc = imm`. Unconditional internal branch. A backward one is a
    /// back-edge and is where the preemption budget is checked.
    Br = 2,
    /// `if cond holds: pc = imm`. The condition is read from the live flags,
    /// exactly as the interpreter's `jcc` reads them. Retires.
    BrIf = 3,

    /// `regs[d] = imm` (zero-extended from 32 bits, full-width write).
    Li = 4,
    /// `regs[d] = <next word>` — a full 64-bit constant.
    Li64 = 5,
    /// `regs[d] = regs[a]`, at the op's width (x86 register-write semantics).
    Mov = 6,

    /// The arithmetic and logical core. The right-hand side is `regs[b]`, or
    /// the immediate when the immediate modifier is set. All record flags
    /// unless the no-flags modifier is set (dead-flag elimination). `Cmp`
    /// and `Test` do not write back.
    Add = 7,
    Sub = 8,
    Or = 9,
    Xor = 10,
    And = 11,
    Cmp = 12,
    Test = 13,

    /// `inc`/`dec`/`neg`/`not`. `Inc`/`Dec` preserve carry (their flag rule);
    /// `Neg` is `0 - regs[a]`; `Not` writes no flags.
    Inc = 14,
    Dec = 15,
    Neg = 16,
    Not = 17,

    /// `regs[d] = load(regs[a] + sdisp, width)`, zero-extended. `sdisp` is
    /// the sign-extended 32-bit immediate. Can fault.
    Load = 18,
    /// `store(regs[a] + sdisp, width, regs[b])`. Can fault; dirties a code
    /// page if it lands on one.
    Store = 19,

    /// `movzx`/`movsx`: read `regs[a]` at the *source* width (in the
    /// condition field, reused), write `regs[d]` at the op width, zero- or
    /// sign-extending.
    Widen = 20,
    WidenSigned = 21,

    /// The stack. `Push`: `rsp -= width; store(rsp, width, regs[a])`.
    /// `Pop`: `regs[d] = load(rsp, width); rsp += width`. `rsp` is register
    /// four, addressed like any other. Can fault.
    Push = 22,
    Pop = 23,

    /// `regs[d] = regs[a] + regs[b] * scale + sdisp`, the general effective
    /// address, computed into a scratch register — `lea`, and the prefix the
    /// transpiler emits before a `load`/`store` whose addressing is more than
    /// `base + disp`. Scale is `1 << (condition field)`. Never faults.
    Lea = 24,
    /// `regs[d] = regs[a] & 0xffff_ffff` — the 32-bit address wrap, applied
    /// to a computed address before the access. Never faults.
    Narrow = 25,
    /// `regs[d] = fs_base` — the thread pointer, for `%fs`-relative
    /// addressing. Never faults.
    LoadFs = 26,

    /// `regs[d] = regs[a] << (regs[b] & mask)` and the two right shifts, with
    /// x86's flag semantics reproduced. Present because glibc leans on them;
    /// the count is masked to the width as the architecture masks it.
    Shl = 27,
    Shr = 28,
    Sar = 29,
}

impl Op {
    /// The op a raw byte encodes, or `None` for a byte no op uses — which a
    /// well-formed trace never contains, so the interpreter may treat the
    /// `None` as unreachable.
    #[inline(always)]
    fn from_byte(byte: u8) -> Option<Op> {
        // The discriminants are contiguous 0..=29, so a range check and a
        // transmute is the whole of it — the dense dispatch the format is
        // for.
        (byte <= Op::Sar as u8).then(|| unsafe { core::mem::transmute::<u8, Op>(byte) })
    }
}

/// One word of a trace: the packed first word of an op, or the spilled
/// second word of a two-word op. A `u64` either way, which is what keeps the
/// stream a flat `&[u64]`.
pub type Word = u64;

/// Field positions in an op's first word.
mod field {
    pub const OP: u32 = 0;
    pub const D: u32 = 8;
    pub const A: u32 = 13;
    pub const B: u32 = 18;
    pub const WIDTH: u32 = 23;
    /// The right-hand side is the immediate, not `regs[b]`.
    pub const IMMEDIATE: u32 = 25;
    /// The flags this op would write are dead; do not record them.
    pub const NO_FLAGS: u32 = 26;
    /// The last word of a guest instruction; retire on it.
    pub const RETIRE: u32 = 27;
    /// The condition (`BrIf`), or a small auxiliary field (source width for
    /// the widening moves, the scale log for `Lea`).
    pub const CONDITION: u32 = 28;
    pub const IMM: u32 = 32;
}

/// Builds an op's first word. The transpiler is the only caller; the field
/// packing lives here so the interpreter's unpacking has one counterpart.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn encode(
    op: Op,
    d: u8,
    a: u8,
    b: u8,
    width: Width,
    immediate: bool,
    no_flags: bool,
    retire: bool,
    condition: u8,
    imm: u32,
) -> Word {
    (op as u64) << field::OP
        | (d as u64) << field::D
        | (a as u64) << field::A
        | (b as u64) << field::B
        | (width as u64) << field::WIDTH
        | (immediate as u64) << field::IMMEDIATE
        | (no_flags as u64) << field::NO_FLAGS
        | (retire as u64) << field::RETIRE
        | ((condition as u64) & 0xf) << field::CONDITION
        | (imm as u64) << field::IMM
}

/// The bit position of the immediate field, for the transpiler's own
/// patching of already-emitted words.
pub(crate) const fn field_imm() -> u32 {
    field::IMM
}

/// A width from its two-bit class. Total, because the class is exactly two
/// bits and every value is a width.
#[inline(always)]
fn width_of(word: Word) -> Width {
    match (word >> field::WIDTH) & 0b11 {
        0 => Width::Byte,
        1 => Width::Word,
        2 => Width::Dword,
        _ => Width::Qword,
    }
}

/// A finished trace: a flat stream of words, and the guest address each word
/// belongs to so a fault can name the faulting instruction.
///
/// One trace covers one decoded block (`crate::block::Block`) — a run from
/// an entry address to the first transfer the transpiler could not keep
/// internal. `entry` is the guest address the trace begins at, the key the
/// address cache will find it by.
pub struct Trace {
    /// The guest address the trace is entered at.
    pub entry: u64,
    /// The bytecode, first word of each op followed by any spilled word.
    pub code: Vec<Word>,
    /// Parallel to `code`: the guest instruction address each word serves,
    /// so a fault at word `pc` reports `rip = ip[pc]`. A side table rather
    /// than a field on the op, so the op stays one `u64`; the cost is a `u64`
    /// per word, which a trace of a few hundred words can afford.
    pub ip: Vec<u64>,
}

/// Why [`run`] returned to the caller. Each variant leaves the [`Tcb`] in the
/// state the interpreter would leave it in at the same point, so the run loop
/// serves it with the same code that serves a block.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Leave {
    /// Control left the trace with `rip` set to the target. The run loop
    /// looks the target up — in the address cache, then the transpiler, then
    /// the interpreter — exactly as it looks up a block's successor.
    Exit,
    /// The trace reached a `Defer`. `rip` is the guest address of the one
    /// instruction to run through the interpreter; after it, re-enter the
    /// trace. Carries the stream offset to resume at.
    Defer { resume: usize },
    /// The preemption budget ran out at a back-edge. `rip` names the next
    /// instruction.
    Preempted,
    /// A load or store faulted. `rip` names the faulting instruction, which
    /// did not retire.
    Fault(Fault),
}

/// Runs a trace from stream offset `start`, on the engine's own state, until
/// it leaves.
///
/// The register file is copied in once and copied back at every leave, so
/// the `Tcb` is current wherever execution stops — a fault, a defer, a
/// preemption, an exit — and the hot path in between touches a contiguous
/// local array rather than reaching through the control block per operand.
/// Flags and memory are *not* copied: the flags are recorded straight into
/// `tcb.flags` and loads and stores go straight through `space`, because a
/// faithful fault and a faithful flag are worth more than avoiding the
/// indirection, and neither is on the hottest path.
pub fn run(trace: &Trace, start: usize, tcb: &mut Tcb, space: &mut Space, budget: u64) -> Leave {
    let code = &trace.code;
    let mut regs = [0u64; REGISTERS];
    regs[..crate::state::REGISTER_COUNT].copy_from_slice(&tcb.registers);
    let mut pc = start;
    let mut spent: u64 = 0;

    // Reading a register slice at a width — zero-extended, no high-byte case
    // (the transpiler defers `%ah`/`%bh`/… for now). Scratch registers are
    // always read full-width.
    macro_rules! read {
        ($idx:expr, $w:expr) => {
            regs[$idx as usize] & $w.mask()
        };
    }
    // Writing a register slice with x86's width semantics: a qword or
    // scratch write is whole, a dword write zero-extends, a narrower write
    // preserves the rest. Identical to `Tcb::write_register` minus the high
    // byte.
    macro_rules! write {
        ($idx:expr, $w:expr, $val:expr) => {{
            let slot = &mut regs[$idx as usize];
            match $w {
                Width::Qword => *slot = $val,
                Width::Dword => *slot = $val & 0xffff_ffff,
                w => {
                    let mask = w.mask();
                    *slot = (*slot & !mask) | ($val & mask);
                }
            }
        }};
    }
    // Copy the register file back to the control block. Called at every
    // leave, so the `Tcb` a fault or a syscall sees is current.
    macro_rules! sync {
        () => {
            tcb.registers.copy_from_slice(&regs[..crate::state::REGISTER_COUNT]);
        };
    }

    loop {
        let word = code[pc];
        let op = match Op::from_byte((word >> field::OP) as u8) {
            Some(op) => op,
            // A well-formed trace never holds a byte no op uses.
            None => unreachable!("a trace holds only encoded ops"),
        };
        let d = ((word >> field::D) & 0x1f) as usize;
        let a = ((word >> field::A) & 0x1f) as usize;
        let b = ((word >> field::B) & 0x1f) as usize;
        let imm = (word >> field::IMM) as u32;
        let retire = (word >> field::RETIRE) & 1 != 0;
        pc += 1;

        match op {
            Op::ExitTo => {
                tcb.retired += 1;
                tcb.rip = regs[d];
                sync!();
                return Leave::Exit;
            }
            Op::Defer => {
                // The guest address of the one instruction to interpret is
                // the following word; `rip` points at it and the caller runs
                // it, then re-enters at the word after this one.
                let address = code[pc];
                pc += 1;
                tcb.rip = address;
                sync!();
                return Leave::Defer { resume: pc };
            }
            Op::Br => {
                let target = imm as usize;
                // A back-edge is where the budget is spent and checked —
                // once per loop body, not per op. Retirement stays exact
                // (every retiring op counts itself); only the check is
                // coarsened.
                if target <= pc {
                    spent += 1;
                    if spent >= budget {
                        // The run loop resumes at the branch target: that is
                        // where execution is, and the trace is re-entered
                        // there rather than through the front.
                        tcb.rip = trace.ip[target];
                        sync!();
                        return Leave::Preempted;
                    }
                }
                pc = target;
            }
            Op::BrIf => {
                let condition = Condition::from_code(((word >> field::CONDITION) & 0xf) as u8)
                    .expect("a four-bit condition is always one of sixteen");
                if retire {
                    tcb.retired += 1;
                }
                if condition.holds(&tcb.flags) {
                    pc = imm as usize;
                }
            }
            Op::Li => {
                regs[d] = imm as u64;
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Li64 => {
                regs[d] = code[pc];
                pc += 1;
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Mov => {
                let width = width_of(word);
                let value = read!(a, width);
                write!(d, width, value);
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Add
            | Op::Sub
            | Op::Or
            | Op::Xor
            | Op::And
            | Op::Cmp
            | Op::Test => {
                let width = width_of(word);
                let left = read!(a, width);
                let right = if (word >> field::IMMEDIATE) & 1 != 0 {
                    imm as u64 & width.mask()
                } else {
                    read!(b, width)
                };
                let result = width.truncate(match op {
                    Op::Add => left.wrapping_add(right),
                    Op::Sub | Op::Cmp => left.wrapping_sub(right),
                    Op::Or => left | right,
                    Op::Xor => left ^ right,
                    _ => left & right,
                });
                if (word >> field::NO_FLAGS) & 1 == 0 {
                    let rule = match op {
                        Op::Add => Rule::Add,
                        Op::Sub | Op::Cmp => Rule::Sub,
                        _ => Rule::Logic,
                    };
                    tcb.flags.record(rule, width, left, right, result);
                }
                if !matches!(op, Op::Cmp | Op::Test) {
                    write!(d, width, result);
                }
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Inc | Op::Dec => {
                let width = width_of(word);
                let left = read!(a, width);
                let (result, rule) = match op {
                    Op::Inc => (left.wrapping_add(1), Rule::Increment),
                    _ => (left.wrapping_sub(1), Rule::Decrement),
                };
                let result = width.truncate(result);
                if (word >> field::NO_FLAGS) & 1 == 0 {
                    tcb.flags.record(rule, width, left, 1, result);
                }
                write!(d, width, result);
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Neg => {
                let width = width_of(word);
                let value = read!(a, width);
                let result = width.truncate(0u64.wrapping_sub(value));
                if (word >> field::NO_FLAGS) & 1 == 0 {
                    tcb.flags.record(Rule::Sub, width, 0, value, result);
                }
                write!(d, width, result);
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Not => {
                let width = width_of(word);
                let value = read!(a, width);
                write!(d, width, width.truncate(!value));
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Load => {
                let width = width_of(word);
                let address = regs[a].wrapping_add(imm as i32 as i64 as u64);
                match space.load(address, width) {
                    Ok(value) => write!(d, width, value),
                    Err(fault) => {
                        tcb.rip = trace.ip[pc - 1];
                        sync!();
                        return Leave::Fault(fault);
                    }
                }
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Store => {
                let width = width_of(word);
                let address = regs[a].wrapping_add(imm as i32 as i64 as u64);
                let value = read!(b, width);
                if let Err(fault) = space.store(address, width, value) {
                    tcb.rip = trace.ip[pc - 1];
                    sync!();
                    return Leave::Fault(fault);
                }
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Widen | Op::WidenSigned => {
                let width = width_of(word);
                let source_width = match (word >> field::CONDITION) & 0b11 {
                    0 => Width::Byte,
                    1 => Width::Word,
                    2 => Width::Dword,
                    _ => Width::Qword,
                };
                let value = read!(a, source_width);
                let widened = match op {
                    Op::Widen => value,
                    _ => source_width.sign_extend(value),
                };
                write!(d, width, widened);
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Push => {
                let width = width_of(word);
                let value = read!(a, width);
                let at = regs[crate::state::STACK_POINTER]
                    .wrapping_sub(u64::from(width.bytes()));
                if let Err(fault) = space.store(at, width, value) {
                    tcb.rip = trace.ip[pc - 1];
                    sync!();
                    return Leave::Fault(fault);
                }
                regs[crate::state::STACK_POINTER] = at;
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Pop => {
                let width = width_of(word);
                let at = regs[crate::state::STACK_POINTER];
                match space.load(at, width) {
                    Ok(value) => {
                        regs[crate::state::STACK_POINTER] =
                            at.wrapping_add(u64::from(width.bytes()));
                        write!(d, width, value);
                    }
                    Err(fault) => {
                        tcb.rip = trace.ip[pc - 1];
                        sync!();
                        return Leave::Fault(fault);
                    }
                }
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Lea => {
                let scale = 1u64 << ((word >> field::CONDITION) & 0b11);
                let value = regs[a]
                    .wrapping_add(regs[b].wrapping_mul(scale))
                    .wrapping_add(imm as i32 as i64 as u64);
                regs[d] = value;
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Narrow => {
                regs[d] = regs[a] & 0xffff_ffff;
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::LoadFs => {
                regs[d] = tcb.fs_base;
                if retire {
                    tcb.retired += 1;
                }
            }
            Op::Shl | Op::Shr | Op::Sar => {
                let width = width_of(word);
                let count = if (word >> field::IMMEDIATE) & 1 != 0 {
                    imm as u64
                } else {
                    regs[b]
                } & u64::from(width.bits() - 1);
                let left = read!(a, width);
                let result = width.truncate(match op {
                    Op::Shl => left.wrapping_shl(count as u32),
                    Op::Shr => left.wrapping_shr(count as u32),
                    _ => (width.sign_extend(left) as i64 >> count) as u64,
                });
                // A shift by zero touches no flags; otherwise x86 leaves them
                // as though the result had been produced by a logical op for
                // ZF/SF/PF, which is what the interpreter's own shift does.
                // Flag fidelity for the carry and overflow of a shift is
                // deferred with the count-carrying edge cases: the transpiler
                // only emits a shift whose flags are dead, so `NO_FLAGS` is
                // always set here and this records nothing.
                debug_assert!(
                    (word >> field::NO_FLAGS) & 1 == 1,
                    "a shift is only transpiled when its flags are dead"
                );
                write!(d, width, result);
                if retire {
                    tcb.retired += 1;
                }
            }
        }
    }
}
