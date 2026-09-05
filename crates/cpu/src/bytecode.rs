//! A register-machine bytecode, and a switch-loop interpreter for it, that
//! runs x86-64 faster than interpreting x86 directly on the loops it covers.
//!
//! The idea, and the measurement that justified it, are in
//! `docs/performance.md`: a flat register-machine bytecode a block transpiles
//! into, run by a dense switch-loop, because the stream is flat (a direct
//! branch is `pc = offset`, not a re-entry of the run loop), the dispatch is
//! dense (a `br_table` over a `u8`, not a seventeen-hundred-way match), and
//! the operands are already resolved (no `iced` re-derivation per
//! execution). The floor prototype put
//! that at 7.7× idealised; the faithful transpiler here, carrying x86's
//! widths, flags, and the engine's `Space`, measures **1.3–2.1×** under
//! wasmtime on fully-covered hot loops (`docs/performance.md`) — and a
//! *loss* on any loop it does not cover end to end, since a [`Op::Defer`]
//! costs more than interpreting. So the win is real, modest, and entirely a
//! function of coverage; the staged optimisations (op fusion, tail-call
//! threading) that would lift it toward the 3–6× estimate are not built.
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

    /// `regs[d] = regs[a] << count` and the two right shifts (`Sar`
    /// arithmetic). The count is `regs[b]` or the immediate, masked by 0x3f
    /// for a 64-bit operand and 0x1f otherwise, as x86 masks it. Writes no
    /// flags — the transpiler emits a shift only when its flags are dead
    /// (shift flags are count-dependent and fiddly), else it defers.
    Shl = 27,
    Shr = 28,
    Sar = 29,

    /// `regs[d] = trunc(regs[a] * rhs)` — the low half of `imul`, the only
    /// half a two- or three-operand `imul` keeps. The right-hand side is
    /// `regs[b]` or the immediate. Writes no flags: the transpiler emits it
    /// only when `imul`'s flags are dead (else it defers), so `CF`/`OF` — the
    /// only flags `imul` defines — need not be computed.
    Mul = 30,

    /// `rol`/`ror` by `regs[b]` or the immediate, count masked as x86 masks
    /// it. Writes no flags (emitted only when the rotate's `CF`/`OF` are
    /// dead), so the value is all it computes. `rcl`/`rcr` (through the carry)
    /// defer.
    Rol = 31,
    Ror = 32,

    /// `setcc`: `regs[d]`'s low byte becomes one when the condition (in the
    /// condition field) holds, zero otherwise; the rest of the register is
    /// preserved. Reads the live flags, writes none.
    Setcc = 33,
    /// `cmovcc`: `regs[d] = holds ? regs[a] : regs[d]`, written at the op
    /// width *either way* — a 32-bit `cmov` clears the upper half whether or
    /// not it moves, which code depends on. Reads the live flags.
    Cmov = 34,

    /// `adc`/`sbb`: `regs[a] ± rhs ± carry`, reading the carry from the live
    /// flags and recording the carrying rule back. The right-hand side is
    /// `regs[b]` or the immediate. These write flags (a carry chain reads
    /// them), so they always record.
    Adc = 35,
    Sbb = 36,

    /// A fused flag-producer and conditional branch — `cmp`/`test`/`sub`/
    /// `add`/`and`/`or`/`xor`/`inc`/`dec` immediately followed by a `jcc` that
    /// consumes its flags, in one op. Two words: the first is the producer's
    /// operands (`a`, `b`-or-immediate, width) and the branch's condition; the
    /// second is `[target:32][producer op:8][flags-live-after:1]`. It computes
    /// the result, evaluates the condition without touching the control block,
    /// writes the result back if the producer does, updates the trace's flags
    /// only if they are read later, and branches — and it retires *two* guest
    /// instructions. The lazy-flags record on the hot conditional, gone.
    FusedBranch = 37,

    /// `div`/`idiv` at dword or qword width: `RDX:RAX / regs[a]`, quotient to
    /// `RAX`, remainder to `RDX`, leaving the flags alone as the interpreter
    /// does. The `#DE` cases — a zero divisor, or a quotient that does not fit
    /// — defer back to the interpreter, which raises the fault; the common
    /// case is done here. The byte form's `AX`/`AH` shape and a memory divisor
    /// defer at transpile time.
    Div = 38,
    Idiv = 39,

    /// The SSE2 subset glibc's string and memory routines lean on, operating
    /// on the 128-bit XMM file in the control block. Everything else vector
    /// defers to the interpreter's vector unit.
    ///
    /// `VecMov`: `xmm[d]` ← `xmm[b]` or the 16 bytes at `regs[a] + imm` (the
    /// immediate modifier selects the memory source) — `movups`/`movaps`/
    /// `movdqu`/`movdqa`. `VecStore`: the 16 bytes at `regs[a] + imm` ←
    /// `xmm[b]`. `VecAnd`/`VecXor`/`VecCmpEqB`: `xmm[d]` combined with `xmm[b]`
    /// or memory — `pand`/`pxor`/`pcmpeqb`. `VecMask`: `regs[d]` ← the sixteen
    /// byte sign bits of `xmm[b]` — `pmovmskb`. A memory operand can fault.
    VecMov = 40,
    VecStore = 41,
    VecAnd = 42,
    VecXor = 43,
    VecCmpEqB = 44,
    VecMask = 45,

    /// A read-modify-write on memory in one op — `add`/`sub`/`or`/`xor`/`and`
    /// `[mem], reg`, and `inc`/`dec [mem]` — where the interpreter would emit a
    /// load, an ALU op, and a store (three dispatches). The address is
    /// `regs[a] + sdisp`; the operation is in the condition field (0 add, 1
    /// sub, 2 or, 3 xor, 4 and, 5 inc, 6 dec); the right-hand side is `regs[b]`
    /// (`inc`/`dec` supply their own one). Records flags unless the no-flags
    /// modifier is set. Can fault; dirties a code page like any store. This is
    /// the superinstruction CPython's reference counting turns on.
    MemRmw = 46,

    /// An indexed load and store — `regs[d] = load(regs[a] + regs[b]*scale +
    /// sdisp)` and its store — where the interpreter would materialise the
    /// address with a `Li`, an `Add`, and a `Lea` before a plain `Load`/
    /// `Store`, four dispatches for one guest access. Scale is `1 <<
    /// (condition field)`; the width is the op width; `sdisp` is the
    /// sign-extended 32-bit immediate. `StoreX`'s value is `regs[d]`. This is
    /// the addressing x86 folds into every array and object access; folding it
    /// here is the memory superinstruction. `%fs`-relative and 32-bit-wrapped
    /// addressing still materialise. Can fault; `StoreX` dirties code pages.
    LoadX = 47,
    StoreX = 48,
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
        (byte <= Op::StoreX as u8).then(|| unsafe { core::mem::transmute::<u8, Op>(byte) })
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

/// How an indirect transfer out of a trace is resolved — the address cache.
///
/// When the interpreter would leave a trace (a computed goto, a `ret`, an
/// indirect or out-of-trace call), it asks the resolver for the target's
/// trace. A hit keeps execution *inside* the interpreter — the register file,
/// the flags, the retirement counter all carry straight over, only the stream
/// and program counter switch — which is what keeps a `call`/`ret` and
/// CPython's per-bytecode `jmp *reg` from round-tripping the run loop. A miss
/// exits to the run loop, which decodes and transpiles the target (warming the
/// cache) and re-enters.
pub enum Resolver<'a> {
    /// No address cache: every indirect transfer leaves to the run loop. What
    /// the differential harness and the single-block benchmark use.
    Runloop,
    /// Probe this block cache's transpiled traces, staying internal on a hit.
    Cache(&'a crate::block::BlockCache),
}

impl<'a> Resolver<'a> {
    #[inline]
    fn resolve(&self, address: u64) -> Option<&'a Trace> {
        match self {
            Resolver::Runloop => None,
            Resolver::Cache(cache) => cache.resolve_trace(address),
        }
    }
}

/// Runs a trace from stream offset `start`, on the engine's own state, until
/// it leaves — following indirect transfers into other traces through the
/// `resolver` (the address cache) rather than returning to the run loop, for
/// as long as their targets are cached.
///
/// The register file is copied in once and copied back at every leave, so
/// the `Tcb` is current wherever execution stops — a fault, a defer, a
/// preemption, an exit — and the hot path in between touches a contiguous
/// local array rather than reaching through the control block per operand.
/// Memory goes straight through `space`; the flags are held in a local and
/// flushed at a leave, because a faithful fault and a faithful flag are worth
/// more than avoiding the indirection, and neither is on the hottest path.
pub fn run<'a>(
    mut trace: &'a Trace,
    start: usize,
    tcb: &mut Tcb,
    space: &mut Space,
    budget: u64,
    resolver: Resolver<'a>,
) -> Leave {
    let mut code = &trace.code;
    let mut regs = [0u64; REGISTERS];
    regs[..crate::state::REGISTER_COUNT].copy_from_slice(&tcb.registers);
    // The lazy-flags record, held in a local copy through the trace and
    // flushed to the control block only at a leave — so a `cmp`/`jcc` pair, an
    // `adc` chain, a `setcc`, touch a register-resident struct the compiler
    // can keep in place rather than the control-block pointer on every op.
    let mut flags = tcb.flags;
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
    // Leave the interpreter: flush the locally accumulated retirement to the
    // control block and copy the register file back, so the `Tcb` a fault,
    // syscall, or preemption sees is current. Retirement is kept in a local
    // (`spent`) and written once here rather than through the control-block
    // pointer on every op — the hot path touches only the local.
    macro_rules! flush {
        () => {{
            tcb.retired = tcb.retired.wrapping_add(spent);
            tcb.registers.copy_from_slice(&regs[..crate::state::REGISTER_COUNT]);
            tcb.flags = flags;
        }};
    }
    // Self-modifying code: a store that landed on a page some cached block —
    // possibly this very trace — was decoded from must stop execution, so the
    // run loop's drain sees current bytes before the next fetch, exactly as
    // the interpreter breaks its block loop on `has_dirty_code`. Checked only
    // after a store, and only when the next word belongs to a *different*
    // guest instruction: when it belongs to this instruction's own
    // fall-through exit stub (the store was the block's last instruction), the
    // stub already sets `rip` to the right place, so leaving is its job.
    macro_rules! break_on_dirty {
        () => {
            if space.has_dirty_code() && trace.ip[pc] != trace.ip[pc - 1] {
                tcb.rip = trace.ip[pc];
                flush!();
                return Leave::Exit;
            }
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
                // A retiring `ExitTo` is a guest transfer (jmp/call/ret);
                // a non-retiring one is a fall-through or stub exit that stands
                // for no instruction, so it must not count. The budget is
                // checked here — an indirect transfer is a trace boundary, one
                // of the two places (with a back-edge) the quantum is honoured
                // — and then the address cache is probed: a hit switches trace
                // and continues inside the interpreter, a miss leaves to the
                // run loop.
                if retire {
                    spent += 1;
                }
                let target = regs[d];
                if spent >= budget {
                    tcb.rip = target;
                    flush!();
                    return Leave::Preempted;
                }
                match resolver.resolve(target) {
                    Some(next) => {
                        trace = next;
                        code = &trace.code;
                        pc = 0;
                    }
                    None => {
                        tcb.rip = target;
                        flush!();
                        return Leave::Exit;
                    }
                }
            }
            Op::Defer => {
                // The guest address of the one instruction to interpret is
                // the following word; `rip` points at it and the caller runs
                // it, then re-enters at the word after this one.
                let address = code[pc];
                pc += 1;
                tcb.rip = address;
                flush!();
                return Leave::Defer { resume: pc };
            }
            Op::Br => {
                // The guest `jmp` retires; then, on a back-edge, the budget is
                // checked — once per loop body, not per op. `spent` counts
                // retired instructions, so the check is in the same units as
                // the quantum, and the over-run is bounded by one loop body.
                if retire {
                    spent += 1;
                }
                let target = imm as usize;
                if target <= pc && spent >= budget {
                    // Resume where execution is: at the branch target.
                    tcb.rip = trace.ip[target];
                    flush!();
                    return Leave::Preempted;
                }
                pc = target;
            }
            Op::BrIf => {
                let condition = Condition::from_code(((word >> field::CONDITION) & 0xf) as u8)
                    .expect("a four-bit condition is always one of sixteen");
                if retire {
                    spent += 1;
                }
                if condition.holds(&flags) {
                    let target = imm as usize;
                    // A taken back-edge is a loop iteration boundary: check the
                    // budget there, in retired-instruction units.
                    if target <= pc && spent >= budget {
                        tcb.rip = trace.ip[target];
                        flush!();
                        return Leave::Preempted;
                    }
                    pc = target;
                }
            }
            Op::Li => {
                regs[d] = imm as u64;
                if retire {
                    spent += 1;
                }
            }
            Op::Li64 => {
                regs[d] = code[pc];
                pc += 1;
                if retire {
                    spent += 1;
                }
            }
            Op::Mov => {
                let width = width_of(word);
                let value = read!(a, width);
                write!(d, width, value);
                if retire {
                    spent += 1;
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
                    flags.record(rule, width, left, right, result);
                }
                if !matches!(op, Op::Cmp | Op::Test) {
                    write!(d, width, result);
                }
                if retire {
                    spent += 1;
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
                    flags.record(rule, width, left, 1, result);
                }
                write!(d, width, result);
                if retire {
                    spent += 1;
                }
            }
            Op::Neg => {
                let width = width_of(word);
                let value = read!(a, width);
                let result = width.truncate(0u64.wrapping_sub(value));
                if (word >> field::NO_FLAGS) & 1 == 0 {
                    flags.record(Rule::Sub, width, 0, value, result);
                }
                write!(d, width, result);
                if retire {
                    spent += 1;
                }
            }
            Op::Not => {
                let width = width_of(word);
                let value = read!(a, width);
                write!(d, width, width.truncate(!value));
                if retire {
                    spent += 1;
                }
            }
            Op::Load => {
                let width = width_of(word);
                let address = regs[a].wrapping_add(imm as i32 as i64 as u64);
                match space.load(address, width) {
                    Ok(value) => write!(d, width, value),
                    Err(fault) => {
                        tcb.rip = trace.ip[pc - 1];
                        flush!();
                        return Leave::Fault(fault);
                    }
                }
                if retire {
                    spent += 1;
                }
            }
            Op::Store => {
                let width = width_of(word);
                let address = regs[a].wrapping_add(imm as i32 as i64 as u64);
                let value = read!(b, width);
                if let Err(fault) = space.store(address, width, value) {
                    tcb.rip = trace.ip[pc - 1];
                    flush!();
                    return Leave::Fault(fault);
                }
                if retire {
                    spent += 1;
                }
                break_on_dirty!();
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
                    spent += 1;
                }
            }
            Op::Push => {
                let width = width_of(word);
                let value = read!(a, width);
                let at = regs[crate::state::STACK_POINTER]
                    .wrapping_sub(u64::from(width.bytes()));
                if let Err(fault) = space.store(at, width, value) {
                    tcb.rip = trace.ip[pc - 1];
                    flush!();
                    return Leave::Fault(fault);
                }
                regs[crate::state::STACK_POINTER] = at;
                if retire {
                    spent += 1;
                }
                break_on_dirty!();
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
                        flush!();
                        return Leave::Fault(fault);
                    }
                }
                if retire {
                    spent += 1;
                }
            }
            Op::Lea => {
                let scale = 1u64 << ((word >> field::CONDITION) & 0b11);
                let value = regs[a]
                    .wrapping_add(regs[b].wrapping_mul(scale))
                    .wrapping_add(imm as i32 as i64 as u64);
                regs[d] = value;
                if retire {
                    spent += 1;
                }
            }
            Op::Narrow => {
                regs[d] = regs[a] & 0xffff_ffff;
                if retire {
                    spent += 1;
                }
            }
            Op::LoadFs => {
                regs[d] = tcb.fs_base;
                if retire {
                    spent += 1;
                }
            }
            Op::Shl | Op::Shr | Op::Sar => {
                let width = width_of(word);
                // x86 masks the shift count by 0x3f for a 64-bit operand and
                // by 0x1f for every narrower one — not by the operand width, so
                // a byte shift by 20 is a shift by 20 (all bits gone), not by 4.
                let count = (if (word >> field::IMMEDIATE) & 1 != 0 {
                    imm as u64
                } else {
                    regs[b]
                }) & if width == Width::Qword { 0x3f } else { 0x1f };
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
                    spent += 1;
                }
            }
            Op::Mul => {
                let width = width_of(word);
                let left = read!(a, width);
                let right = if (word >> field::IMMEDIATE) & 1 != 0 {
                    imm as u64 & width.mask()
                } else {
                    read!(b, width)
                };
                write!(d, width, width.truncate(left.wrapping_mul(right)));
                if retire {
                    spent += 1;
                }
            }
            Op::Rol | Op::Ror => {
                let width = width_of(word);
                let bits = u64::from(width.bits());
                let raw = if (word >> field::IMMEDIATE) & 1 != 0 {
                    imm as u64
                } else {
                    regs[b]
                };
                let count = raw & if width == Width::Qword { 0x3f } else { 0x1f };
                let value = read!(a, width);
                let turns = count % bits;
                // The interpreter's own rotate, value only — the `(bits -
                // turns) % bits` keeps both shifts below the width even when
                // `turns` is zero (a masked count of zero is a no-op).
                let result = width.truncate(match op {
                    Op::Rol => (value << turns) | (value >> ((bits - turns) % bits)),
                    _ => (value >> turns) | (value << ((bits - turns) % bits)),
                });
                write!(d, width, result);
                if retire {
                    spent += 1;
                }
            }
            Op::Setcc => {
                let condition = Condition::from_code(((word >> field::CONDITION) & 0xf) as u8)
                    .expect("a four-bit condition is one of sixteen");
                // A byte write: the low byte becomes zero or one, the rest of
                // the register preserved.
                write!(d, Width::Byte, u64::from(condition.holds(&flags)));
                if retire {
                    spent += 1;
                }
            }
            Op::Cmov => {
                let width = width_of(word);
                let condition = Condition::from_code(((word >> field::CONDITION) & 0xf) as u8)
                    .expect("a four-bit condition is one of sixteen");
                // Written either way — the read of the destination for the
                // not-taken case is what makes a 32-bit `cmov` clear the upper
                // half whether or not it moves.
                let value = if condition.holds(&flags) {
                    read!(a, width)
                } else {
                    read!(d, width)
                };
                write!(d, width, value);
                if retire {
                    spent += 1;
                }
            }
            Op::Adc | Op::Sbb => {
                let width = width_of(word);
                let left = read!(a, width);
                let right = if (word >> field::IMMEDIATE) & 1 != 0 {
                    imm as u64 & width.mask()
                } else {
                    read!(b, width)
                };
                let carry = u64::from(flags.carry());
                let (result, rule) = match op {
                    Op::Adc => (left.wrapping_add(right).wrapping_add(carry), Rule::AddCarry),
                    _ => (left.wrapping_sub(right).wrapping_sub(carry), Rule::SubBorrow),
                };
                let result = width.truncate(result);
                flags
                    .record_with_carry(rule, width, left, right, result, carry == 1);
                write!(d, width, result);
                if retire {
                    spent += 1;
                }
            }
            Op::FusedBranch => {
                let control = code[pc];
                pc += 1;
                let width = width_of(word);
                let condition = Condition::from_code(((word >> field::CONDITION) & 0xf) as u8)
                    .expect("a four-bit condition is one of sixteen");
                let left = read!(a, width);
                let right = if (word >> field::IMMEDIATE) & 1 != 0 {
                    imm as u64 & width.mask()
                } else {
                    read!(b, width)
                };
                let producer = Op::from_byte((control >> 32) as u8)
                    .expect("the fused producer is a real op");
                let target = (control & 0xffff_ffff) as usize;
                let live_after = (control >> 40) & 1 != 0;
                // The producer decides the result, the flag rule, and whether
                // it writes back — exactly the interpreter's own arms.
                let (value, rule, writes_back) = match producer {
                    Op::Add => (left.wrapping_add(right), Rule::Add, true),
                    Op::Sub => (left.wrapping_sub(right), Rule::Sub, true),
                    Op::Cmp => (left.wrapping_sub(right), Rule::Sub, false),
                    Op::And => (left & right, Rule::Logic, true),
                    Op::Test => (left & right, Rule::Logic, false),
                    Op::Or => (left | right, Rule::Logic, true),
                    Op::Xor => (left ^ right, Rule::Logic, true),
                    Op::Inc => (left.wrapping_add(1), Rule::Increment, true),
                    Op::Dec => (left.wrapping_sub(1), Rule::Decrement, true),
                    _ => unreachable!("the fused producer is a flag-setting op"),
                };
                let result = width.truncate(value);
                // `inc`/`dec` record a right-hand side of one; the rest their
                // real one. A throwaway record seeded from the current flags —
                // so `inc`/`dec` preserve the carry — evaluated for just this
                // condition; the compiler drops the fields the condition does
                // not read when the record does not escape (`live_after`).
                let record_right = match producer {
                    Op::Inc | Op::Dec => 1,
                    _ => right,
                };
                let mut evaluated = flags;
                evaluated.record(rule, width, left, record_right, result);
                let holds = condition.holds(&evaluated);
                if writes_back {
                    write!(d, width, result);
                }
                if live_after {
                    flags = evaluated;
                }
                // Two guest instructions — the producer and the branch.
                spent += 2;
                if holds {
                    if target <= pc && spent >= budget {
                        tcb.rip = trace.ip[target];
                        flush!();
                        return Leave::Preempted;
                    }
                    pc = target;
                }
            }
            Op::Div | Op::Idiv => {
                let width = width_of(word);
                let divisor = read!(a, width);
                // The `#DE` cases are handed to the interpreter, which raises
                // the fault at the right address: `rip` names this instruction
                // and it did not retire, so re-interpreting it is exact.
                if divisor == 0 {
                    tcb.rip = trace.ip[pc - 1];
                    flush!();
                    return Leave::Defer { resume: pc };
                }
                let bits = u64::from(width.bits());
                let low = regs[0] & width.mask(); // RAX
                let high = regs[2] & width.mask(); // RDX
                let dividend = (u128::from(high) << bits) | u128::from(low);
                let (quotient, remainder, overflows) = if matches!(op, Op::Idiv) {
                    let shift = 128 - bits * 2;
                    let dividend = ((dividend << shift) as i128) >> shift;
                    let divisor = width.sign_extend(divisor) as i64 as i128;
                    match dividend.checked_div(divisor) {
                        None => (0, 0, true), // the most negative over minus one
                        Some(quotient) => {
                            let bound = 1i128 << (bits - 1);
                            let fits = quotient >= -bound && quotient < bound;
                            (quotient as u128, (dividend % divisor) as u128, !fits)
                        }
                    }
                } else {
                    let divisor = u128::from(divisor);
                    let quotient = dividend / divisor;
                    (quotient, dividend % divisor, quotient > u128::from(width.mask()))
                };
                if overflows {
                    tcb.rip = trace.ip[pc - 1];
                    flush!();
                    return Leave::Defer { resume: pc };
                }
                write!(0, width, width.truncate(quotient as u64)); // RAX = quotient
                write!(2, width, width.truncate(remainder as u64)); // RDX = remainder
                if retire {
                    spent += 1;
                }
            }
            Op::VecMov
            | Op::VecStore
            | Op::VecAnd
            | Op::VecXor
            | Op::VecCmpEqB
            | Op::VecMask => {
                // The source's sixteen bytes: another XMM register, or memory
                // at `regs[a] + imm` (the immediate modifier selects it). A
                // memory access faults like any other.
                let from_memory = (word >> field::IMMEDIATE) & 1 != 0;
                let source: [u8; 16] = if from_memory && !matches!(op, Op::VecStore) {
                    let address = regs[a].wrapping_add(imm as i32 as i64 as u64);
                    let mut bytes = [0u8; 16];
                    if let Err(fault) = space.read(address, &mut bytes) {
                        tcb.rip = trace.ip[pc - 1];
                        flush!();
                        return Leave::Fault(fault);
                    }
                    bytes
                } else if matches!(op, Op::VecStore) {
                    // The value to store is `xmm[b]`.
                    vector_bytes(&tcb.vectors[b])
                } else {
                    vector_bytes(&tcb.vectors[b])
                };
                match op {
                    Op::VecStore => {
                        let address = regs[a].wrapping_add(imm as i32 as i64 as u64);
                        if let Err(fault) = space.write(address, &source) {
                            tcb.rip = trace.ip[pc - 1];
                            flush!();
                            return Leave::Fault(fault);
                        }
                    }
                    Op::VecMov => tcb.vectors[d] = vector_words(&source),
                    Op::VecAnd => {
                        tcb.vectors[d][0] &= vector_words(&source)[0];
                        tcb.vectors[d][1] &= vector_words(&source)[1];
                    }
                    Op::VecXor => {
                        tcb.vectors[d][0] ^= vector_words(&source)[0];
                        tcb.vectors[d][1] ^= vector_words(&source)[1];
                    }
                    Op::VecCmpEqB => {
                        let mut destination = vector_bytes(&tcb.vectors[d]);
                        for (byte, &other) in destination.iter_mut().zip(source.iter()) {
                            *byte = if *byte == other { 0xff } else { 0x00 };
                        }
                        tcb.vectors[d] = vector_words(&destination);
                    }
                    Op::VecMask => {
                        // The high bit of each of the source's sixteen bytes,
                        // packed low to high — `pmovmskb`.
                        let mut mask = 0u64;
                        for (index, &byte) in source.iter().enumerate() {
                            mask |= u64::from(byte >> 7) << index;
                        }
                        write!(d, Width::Qword, mask);
                    }
                    _ => unreachable!(),
                }
                if retire {
                    spent += 1;
                }
            }
            Op::MemRmw => {
                let width = width_of(word);
                let address = regs[a].wrapping_add(imm as i32 as i64 as u64);
                let left = match space.load(address, width) {
                    Ok(value) => value,
                    Err(fault) => {
                        tcb.rip = trace.ip[pc - 1];
                        flush!();
                        return Leave::Fault(fault);
                    }
                };
                let sub = (word >> field::CONDITION) & 0xf;
                // `inc`/`dec` supply their own right-hand side of one.
                let right = if sub >= 5 { 1 } else { read!(b, width) };
                let (value, rule) = match sub {
                    0 => (left.wrapping_add(right), Rule::Add),
                    1 => (left.wrapping_sub(right), Rule::Sub),
                    2 => (left | right, Rule::Logic),
                    3 => (left ^ right, Rule::Logic),
                    4 => (left & right, Rule::Logic),
                    5 => (left.wrapping_add(1), Rule::Increment),
                    _ => (left.wrapping_sub(1), Rule::Decrement),
                };
                let result = width.truncate(value);
                if (word >> field::NO_FLAGS) & 1 == 0 {
                    flags.record(rule, width, left, right, result);
                }
                if let Err(fault) = space.store(address, width, result) {
                    tcb.rip = trace.ip[pc - 1];
                    flush!();
                    return Leave::Fault(fault);
                }
                if retire {
                    spent += 1;
                }
                break_on_dirty!();
            }
            Op::LoadX => {
                let width = width_of(word);
                let scale = 1u64 << ((word >> field::CONDITION) & 0b11);
                let address = regs[a]
                    .wrapping_add(regs[b].wrapping_mul(scale))
                    .wrapping_add(imm as i32 as i64 as u64);
                match space.load(address, width) {
                    Ok(value) => write!(d, width, value),
                    Err(fault) => {
                        tcb.rip = trace.ip[pc - 1];
                        flush!();
                        return Leave::Fault(fault);
                    }
                }
                if retire {
                    spent += 1;
                }
            }
            Op::StoreX => {
                let width = width_of(word);
                let scale = 1u64 << ((word >> field::CONDITION) & 0b11);
                let address = regs[a]
                    .wrapping_add(regs[b].wrapping_mul(scale))
                    .wrapping_add(imm as i32 as i64 as u64);
                let value = read!(d, width);
                if let Err(fault) = space.store(address, width, value) {
                    tcb.rip = trace.ip[pc - 1];
                    flush!();
                    return Leave::Fault(fault);
                }
                if retire {
                    spent += 1;
                }
                break_on_dirty!();
            }
        }
    }
}

/// The sixteen bytes of a 128-bit XMM register, little-endian — the order a
/// `v128.load` uses, so this matches how the interpreter and the compiler
/// read the file.
#[inline]
fn vector_bytes(words: &[u64; 2]) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&words[0].to_le_bytes());
    bytes[8..].copy_from_slice(&words[1].to_le_bytes());
    bytes
}

/// The inverse of [`vector_bytes`].
#[inline]
fn vector_words(bytes: &[u8; 16]) -> [u64; 2] {
    [
        u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        u64::from_le_bytes(bytes[8..].try_into().unwrap()),
    ]
}
