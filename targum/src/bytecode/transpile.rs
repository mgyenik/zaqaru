//! Turning a decoded block into a bytecode trace.
//!
//! The transpiler consumes what the block cache already produced — the
//! pre-decoded [`Quick`] beside each [`Instruction`] — and re-shapes it into
//! the flat register-machine stream [`super::run`] executes. It does not
//! re-derive x86; it translates the interpreter's own understanding of an
//! instruction into ops that read the same registers, compute the same
//! results, record the same flags, and fault the same way.
//!
//! **Anything it does not model, it declines.** A declined instruction
//! becomes an [`Op::Defer`], which the run loop hands back to the
//! interpreter — so the trace is always correct, and coverage is only ever a
//! question of speed. The refusals are conservative on purpose: a high-byte
//! register, a 32-bit-wrapped address, a vector operand, anything with a
//! `lock` or `rep` prefix, an op the transpiler has no arm for — all defer.
//!
//! A trace covers exactly one block: a straight run of instructions ending
//! in the block's single control transfer. A branch whose constant target is
//! an instruction *inside* the trace becomes an internal `Br`/`BrIf` — this
//! is the structural win, a loop back-edge that never leaves the interpreter.
//! Every other transfer sets `rip` and exits to the run loop, which resolves
//! it (through the address cache, once that is wired) and re-enters.

use iced_x86::{FlowControl, Instruction, Mnemonic, OpKind, Register};

use super::{Op, Trace, Word, encode};
use crate::quick::{self, Address, Quick, Source};
use crate::state::{STACK_POINTER, Slice, Width};

/// The first scratch register the transpiler hands out. Below it are the
/// sixteen guest registers; from it up, [`super::REGISTERS`] minus sixteen
/// temporaries, dead at every instruction boundary.
const SCRATCH_BASE: u8 = super::SCRATCH as u8;

/// Transpiles one decoded block into a trace, or returns `None` when there
/// is nothing worth running as bytecode — every instruction declined, so the
/// trace would be a list of `Defer`s with no covered op between them.
pub fn transpile(block: &crate::block::Block) -> Option<Trace> {
    let count = block.instructions.len();
    if count == 0 {
        return None;
    }
    let liveness = flag_liveness(block);
    // The addresses a branch inside this block targets — an instruction that
    // is one cannot be fused away, because control could enter at it.
    let targets = internal_targets(block);
    // Address → index, so a fused branch can find whether its taken target
    // reads the producer's flags.
    let index_of: std::collections::HashMap<u64, usize> = block
        .instructions
        .iter()
        .enumerate()
        .map(|(i, instruction)| (instruction.ip(), i))
        .collect();
    let mut emitter = Emitter::new(block.entry, liveness.dead.clone());
    let mut index = 0;
    while index < count {
        let instruction = &block.instructions[index];
        let quick = &block.quick[index];
        emitter.begin(index, instruction.ip());
        let is_last = index + 1 == count;
        // A `syscall` ends its block, and the loop — not the trace — runs it:
        // it defers, so the run loop interprets exactly it and returns the
        // syscall to the kernel, with `rip` past it and `%rcx`/`%r11` set as
        // the interpreter leaves them. Exiting to the syscall's own address
        // instead would loop, since the target is the block just run.
        if is_last && is_syscall(instruction) {
            emitter.defer(instruction);
            break;
        }
        // A block otherwise ends at an *unconditional* transfer, a call, or a
        // return (a block cache invariant: a conditional branch does not end a
        // block — it falls through, so a block is an extended run with
        // conditional branches in its middle). Only the last instruction can
        // be one of those block-ending transfers.
        let terminates = is_last
            && matches!(
                instruction.flow_control(),
                FlowControl::UnconditionalBranch
                    | FlowControl::IndirectBranch
                    | FlowControl::Call
                    | FlowControl::IndirectCall
                    | FlowControl::Return
            );
        if terminates {
            emitter.terminal(quick, instruction);
            break;
        }
        // Fusion: a flag-producer immediately followed by a `jcc` that
        // consumes its flags, and whose address nothing branches to (so no
        // control enters between them), becomes one op. The `jcc`'s flag use
        // is what makes the producer's flags live; `live_after` says whether
        // anything past the `jcc` still needs them.
        if index + 1 < count
            && block.quick[index + 1].op == quick::Op::Jcc
            && !targets.contains(&block.instructions[index + 1].ip())
            && emitter.try_fuse(
                quick,
                instruction,
                &block.quick[index + 1],
                &block.instructions[index + 1],
                // The producer's flags outlive the branch if they are live on
                // its fall-through *or* on its taken target — a static pass
                // over one block sees only the fall-through, so the taken
                // target is looked up (and an external one is conservatively
                // taken as live, since the next block may read the flags).
                fused_flags_live_after(block, &liveness, &index_of, index + 1),
            )
        {
            // If the fused `jcc` was the block's last instruction (a cap that
            // fell on it), the not-taken path still needs a fall-through exit.
            if index + 2 == count {
                emitter.exit_to_address(block.instructions[index + 1].next_ip(), false);
            }
            index += 2;
            continue;
        }
        // An ordinary instruction, including a mid-block conditional branch.
        emitter.regular(quick, instruction);
        if is_last {
            // The block fell through its last instruction — a cap, or a
            // conditional branch not taken. Continue at the block's end; the
            // instruction already retired on its own word, so this does not.
            emitter.exit_to_address(instruction.next_ip(), false);
        }
        index += 1;
    }
    emitter.resolve_branches();
    Some(emitter.finish())
}

/// Whether an instruction is a `syscall`/`sysenter` — the thing a block ends
/// at that the loop, not the trace, owns.
fn is_syscall(instruction: &Instruction) -> bool {
    matches!(instruction.mnemonic(), Mnemonic::Syscall | Mnemonic::Sysenter)
}

/// The constant addresses a branch inside the block targets. An instruction
/// at one of these cannot be fused into its predecessor: control could enter
/// at it, so the predecessor must not be run first.
fn internal_targets(block: &crate::block::Block) -> std::collections::HashSet<u64> {
    let mut targets = std::collections::HashSet::new();
    for instruction in &block.instructions {
        if matches!(
            instruction.flow_control(),
            FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch
        ) && instruction.op0_kind() == OpKind::NearBranch64
        {
            targets.insert(instruction.near_branch64());
        }
    }
    targets
}

/// Whether a fused producer's flags are live after its `jcc` (at `jcc_index`)
/// on *either* successor path — the fall-through, or the taken target. A taken
/// target outside the block is taken as live, since the next block may read
/// the flags.
fn fused_flags_live_after(
    block: &crate::block::Block,
    liveness: &Liveness,
    index_of: &std::collections::HashMap<u64, usize>,
    jcc_index: usize,
) -> bool {
    if liveness.live_after[jcc_index] {
        return true;
    }
    let target = block.instructions[jcc_index].near_branch64();
    match index_of.get(&target) {
        Some(&target_index) => liveness.live_entering[target_index],
        None => true,
    }
}

/// A fused producer's right-hand side.
enum Rhs {
    Register(u8),
    Immediate(u64),
    /// `inc`/`dec`: no explicit right-hand side (an implicit one).
    None,
}

/// How an instruction affects the status flags, for the liveness pass.
enum FlagEffect {
    /// Overwrites all six status flags, reads none — `add`/`sub`/`cmp`/
    /// `and`/`or`/`xor`/`test`/`neg`.
    FullWrite,
    /// Overwrites zero/sign/overflow/parity/adjust, *preserves* carry —
    /// `inc`/`dec`. The carry flows through, so it does not kill a live carry.
    WriteExceptCarry,
    /// Reads the flags a condition names, writes none — `jcc`.
    Read { carry: bool, other: bool },
    /// Writes the flags when its count is non-zero, but *preserves* them when
    /// the count is zero — the shifts and rotates. Because the count is a
    /// run-time value, the pass cannot know which happened: it may decide the
    /// op's own flags are dead (nothing downstream reads them), but it must
    /// never treat the op as killing an upstream producer's flags, since a
    /// zero count would let those flow through. So it leaves liveness
    /// unchanged.
    WriteMaybePreserve,
    /// Touches no status flag — `mov`/`lea`/loads/stores/`push`/`pop`/`nop`/
    /// widening moves/`not`/`jmp`/`call`/`ret`.
    None,
    /// Anything else, run through the interpreter: conservatively assumed to
    /// read every flag, so a producer before it is never eliminated.
    Unknown,
}

/// Classifies an instruction's flag effect from its lowering.
fn flag_effect(quick: &Quick, instruction: &Instruction) -> FlagEffect {
    match quick.op {
        quick::Op::Add
        | quick::Op::Sub
        | quick::Op::Cmp
        | quick::Op::And
        | quick::Op::Or
        | quick::Op::Xor
        | quick::Op::Test => FlagEffect::FullWrite,
        quick::Op::Jcc => {
            let (carry, other) = condition_reads(quick.condition);
            FlagEffect::Read { carry, other }
        }
        quick::Op::Mov
        | quick::Op::Lea
        | quick::Op::Push
        | quick::Op::Pop
        | quick::Op::Widen
        | quick::Op::WidenSigned
        | quick::Op::Nop
        | quick::Op::Jmp
        | quick::Op::Call
        | quick::Op::Ret
        // The vector ops touch no status flag.
        | quick::Op::VecMov
        | quick::Op::VecAnd
        | quick::Op::VecXor
        | quick::Op::VecCmpEqB
        | quick::Op::VecMask => FlagEffect::None,
        // The ops lowered straight from the instruction.
        quick::Op::General => match instruction.mnemonic() {
            // `imul`/`mul` overwrite the flags (only CF/OF are defined, the
            // rest undefined) and read none — a full writer for liveness,
            // whether it is transpiled or deferred.
            Mnemonic::Neg | Mnemonic::Imul | Mnemonic::Mul => FlagEffect::FullWrite,
            Mnemonic::Inc | Mnemonic::Dec => FlagEffect::WriteExceptCarry,
            // Shifts and rotates preserve their flags on a zero count.
            Mnemonic::Shl
            | Mnemonic::Sal
            | Mnemonic::Shr
            | Mnemonic::Sar
            | Mnemonic::Rol
            | Mnemonic::Ror => FlagEffect::WriteMaybePreserve,
            // `not` writes no flags; `div`/`idiv` leave them alone (the
            // interpreter does not touch them). All preserve.
            Mnemonic::Not | Mnemonic::Div | Mnemonic::Idiv => FlagEffect::None,
            // `adc`/`sbb`/`setcc`/`cmov` read the flags; the default (a
            // conservative reader of all of them) keeps their producer live,
            // which is exactly what they need.
            _ => FlagEffect::Unknown,
        },
    }
}

/// Which flag classes a branch condition reads: `(carry, other)`, where
/// "other" is zero/sign/overflow/parity taken together.
fn condition_reads(condition: crate::flags::Condition) -> (bool, bool) {
    use crate::flags::Condition::*;
    match condition {
        Below | AboveOrEqual => (true, false),
        BelowOrEqual | Above => (true, true),
        _ => (false, true),
    }
}

/// A backward liveness pass deciding, for each instruction, whether the flags
/// it writes are dead — overwritten before any read — so the transpiler can
/// skip recording them.
///
/// More precise than the block cache's own pass (`Quick::flags_dead`), which
/// runs before the transpiler exists and so treats `inc`/`dec`/`neg` — which
/// it does not lower — as opaque flag readers. Here they are known writers, so
/// the arithmetic feeding a loop's `dec`/`jne` is correctly seen as dead. The
/// carry is tracked apart from the other flags because `inc`/`dec` preserve
/// it: a carry set before a `dec` and read after it is still live across the
/// `dec`, and the pass must not eliminate its producer.
fn flag_liveness(block: &crate::block::Block) -> Liveness {
    let n = block.instructions.len();
    let mut dead = vec![false; n];
    // Whether the flags are live *after* each instruction — needed by fusion,
    // to know if a producer's flags outlive the branch that consumes them.
    let mut live_after = vec![false; n];
    // Flags are live out of the block — the next block may read them — unless
    // it ends in a `call` or `ret`, across which the ABI makes them volatile.
    let ends_volatile = matches!(
        block.instructions.last().map(|i| i.flow_control()),
        Some(FlowControl::Call | FlowControl::IndirectCall | FlowControl::Return)
    );
    // Whether flags are live *entering* each instruction (before it runs) —
    // which, at a branch target, is whether the taken path reads them.
    let mut live_entering = vec![false; n];
    let mut carry_live = !ends_volatile;
    let mut other_live = !ends_volatile;
    for index in (0..n).rev() {
        // Before processing instruction `index`, the accumulated liveness is
        // exactly what is live entering `index + 1` — i.e. live after `index`.
        live_after[index] = carry_live || other_live;
        match flag_effect(&block.quick[index], &block.instructions[index]) {
            FlagEffect::FullWrite => {
                dead[index] = !(carry_live || other_live);
                carry_live = false;
                other_live = false;
            }
            FlagEffect::WriteExceptCarry => {
                // Its written flags (all but carry) are dead when nothing
                // reads them; the carry it preserves flows on untouched, so a
                // live carry stays live and a dead one stays dead.
                dead[index] = !other_live;
                other_live = false;
            }
            FlagEffect::Read { carry, other } => {
                carry_live |= carry;
                other_live |= other;
            }
            FlagEffect::WriteMaybePreserve => {
                // The op's own flags are dead if nothing downstream reads
                // them; but liveness is left unchanged, never killed, because
                // a zero count preserves an upstream producer's flags.
                dead[index] = !(carry_live || other_live);
            }
            FlagEffect::None => {}
            FlagEffect::Unknown => {
                carry_live = true;
                other_live = true;
            }
        }
        // After processing, the accumulated liveness is what is live entering
        // this instruction.
        live_entering[index] = carry_live || other_live;
    }
    Liveness { dead, live_after, live_entering }
}

/// The result of [`flag_liveness`]: for each instruction, whether its written
/// flags are dead, whether flags are live after it, and whether they are live
/// entering it (what a branch to it inherits).
struct Liveness {
    dead: Vec<bool>,
    live_after: Vec<bool>,
    live_entering: Vec<bool>,
}

struct Emitter {
    entry: u64,
    code: Vec<Word>,
    ip: Vec<u64>,
    /// The guest address and stream offset of each instruction's first word,
    /// so a branch to a constant target inside the trace resolves to an
    /// internal offset. Only backward targets occur within one block — the
    /// block ends at its first transfer — so an entry is always present
    /// before it is looked up.
    starts: Vec<(u64, usize)>,
    /// The guest address of the instruction currently being emitted, written
    /// into `ip` for every word it produces.
    current: u64,
    /// The next scratch register to hand out, reset at each instruction.
    scratch: u8,
    /// Conditional branches awaiting their target. Each is `(word offset of
    /// the `BrIf`, taken guest address, the branch instruction's address)`.
    /// Resolved after the whole block is emitted, when every internal target
    /// is known: an internal target patches the `BrIf` to that offset, an
    /// external one to an exit stub. Deferred because a forward branch names a
    /// target not yet emitted.
    branches: Vec<(usize, u64, u64)>,
    /// Fused-branch targets awaiting resolution: `(offset of the control
    /// word, taken guest address, the branch's address)`. Like `branches`,
    /// but the target sits in the low 32 bits of the second (control) word.
    fused_branches: Vec<(usize, u64, u64)>,
    /// Whether each instruction's flag write is dead, from [`flag_liveness`].
    /// Indexed by instruction position; read by the ALU emissions to set the
    /// no-flags modifier.
    flags_dead: Vec<bool>,
    /// The position of the instruction being emitted, into `flags_dead`.
    index: usize,
}

impl Emitter {
    fn new(entry: u64, flags_dead: Vec<bool>) -> Self {
        Self {
            entry,
            code: Vec::new(),
            ip: Vec::new(),
            starts: Vec::new(),
            current: entry,
            scratch: SCRATCH_BASE,
            branches: Vec::new(),
            fused_branches: Vec::new(),
            flags_dead,
            index: 0,
        }
    }

    /// Attempts to fuse a flag-producer and the `jcc` that follows it into one
    /// `FusedBranch`. Returns whether it did — `false` leaves both to be
    /// emitted separately. Only register/immediate operands with a register
    /// destination fuse; a memory operand, a high-byte register, or an
    /// immediate too wide for the op declines.
    fn try_fuse(
        &mut self,
        producer_quick: &Quick,
        producer: &Instruction,
        jcc_quick: &Quick,
        _jcc: &Instruction,
        live_after: bool,
    ) -> bool {
        let target = match jcc_quick.source {
            Source::Immediate(target) => target,
            _ => return false,
        };
        let Some((producer_op, width, dest, rhs)) = fusible_producer(producer_quick, producer)
        else {
            return false;
        };
        let (immediate, b, imm) = match rhs {
            Rhs::Register(number) => (false, number, 0u32),
            Rhs::Immediate(value) => {
                if !fits_immediate(width, value) {
                    return false;
                }
                (true, 0, value as u32)
            }
            // `inc`/`dec`: no right-hand side; the op supplies its own one.
            Rhs::None => (true, 0, 0),
        };
        let condition = jcc_quick.condition as u8;
        // Word one: the producer's operands and the branch condition. `d` and
        // `a` are both the destination, since x86's two-operand form makes the
        // destination the left operand.
        self.push(encode(
            Op::FusedBranch,
            dest,
            dest,
            b,
            width,
            immediate,
            false,
            true,
            condition,
            imm,
        ));
        // Word two: the target (patched later), the producer op-code, and
        // whether the flags outlive the branch.
        let control = ((producer_op as u64) << 32) | ((live_after as u64) << 40);
        let control_offset = self.code.len();
        self.push(control);
        self.fused_branches.push((control_offset, target, self.current));
        true
    }

    /// Resolves every conditional branch to a stream offset — an internal
    /// target directly, an external one through an exit stub appended after
    /// the body. Called once, after the whole block is emitted.
    fn resolve_branches(&mut self) {
        // The same resolution serves both branch kinds — an internal target is
        // a stream offset, an external one an exit stub — differing only in
        // which field of which word the offset is written to.
        let branches = std::mem::take(&mut self.branches);
        for (offset, target, at) in branches {
            let destination = self.resolve_target(target, at);
            self.patch_target(offset, destination);
        }
        let fused = std::mem::take(&mut self.fused_branches);
        for (offset, target, at) in fused {
            let destination = self.resolve_target(target, at);
            // The fused target sits in the low 32 bits of the control word.
            self.code[offset] = (self.code[offset] & !0xffff_ffffu64) | destination as u64;
        }
    }

    /// The stream offset a branch target resolves to: an internal instruction
    /// directly, or an exit stub (appended here) for an external one.
    fn resolve_target(&mut self, target: u64, at: u64) -> usize {
        match self.internal(target) {
            Some(internal) => internal,
            None => {
                // Reached only when the branch is taken (it already retired),
                // so the stub does not retire.
                self.current = at;
                self.scratch = SCRATCH_BASE;
                let stub = self.code.len();
                self.exit_to_address(target, false);
                stub
            }
        }
    }

    /// Rewrites the immediate (branch target) field of an already-emitted
    /// word.
    fn patch_target(&mut self, offset: usize, destination: usize) {
        self.code[offset] = (self.code[offset] & !(0xffff_ffffu64 << super::field_imm()))
            | ((destination as u64) << super::field_imm());
    }

    fn finish(self) -> Trace {
        Trace {
            entry: self.entry,
            code: self.code,
            ip: self.ip,
        }
    }

    /// Begins a guest instruction: records where its bytecode starts and
    /// resets the scratch allocator, since scratch is dead across the
    /// boundary.
    fn begin(&mut self, index: usize, address: u64) {
        self.index = index;
        self.current = address;
        self.scratch = SCRATCH_BASE;
        self.starts.push((address, self.code.len()));
    }

    /// A fresh scratch register, round-robin within one instruction.
    fn temp(&mut self) -> u8 {
        let register = self.scratch;
        self.scratch += 1;
        debug_assert!(
            (self.scratch as usize) <= super::REGISTERS,
            "one instruction asked for more than the scratch file holds"
        );
        register
    }

    /// Pushes one word, tagged with the current instruction's address.
    fn push(&mut self, word: Word) {
        self.code.push(word);
        self.ip.push(self.current);
    }

    /// The stream offset of a guest address inside this trace, if it is an
    /// instruction start already emitted.
    fn internal(&self, address: u64) -> Option<usize> {
        self.starts
            .iter()
            .find(|(at, _)| *at == address)
            .map(|(_, offset)| *offset)
    }

    // ---- primitive emissions --------------------------------------------

    fn li(&mut self, d: u8, value: u64, retire: bool) {
        if value <= u64::from(u32::MAX) {
            self.push(encode(Op::Li, d, 0, 0, Width::Qword, false, false, retire, 0, value as u32));
        } else {
            self.push(encode(Op::Li64, d, 0, 0, Width::Qword, false, false, retire, 0, 0));
            // The full constant rides in the following word; `push` tags it
            // with the current instruction's address like any other.
            self.push(value);
        }
    }

    /// `regs[d] = regs[a]`, full width — the transpiler's own moves between
    /// scratch and guest registers, always qword.
    fn mov_full(&mut self, d: u8, a: u8) {
        self.push(encode(Op::Mov, d, a, 0, Width::Qword, false, false, false, 0, 0));
    }

    fn exit_to_register(&mut self, register: u8, retire: bool) {
        self.push(encode(Op::ExitTo, register, 0, 0, Width::Qword, false, false, retire, 0, 0));
    }

    /// Leaves the trace with `rip` set to a constant guest address.
    fn exit_to_address(&mut self, address: u64, retire: bool) {
        let target = self.temp();
        self.li(target, address, false);
        self.exit_to_register(target, retire);
    }

    // ---- addressing ------------------------------------------------------

    /// Computes a memory operand's effective address, and returns a
    /// `(base_register, displacement)` pair the caller hands to `Load`/
    /// `Store` — the address is `regs[base_register] + sign_extend(disp)`.
    ///
    /// The common `base + disp` case (no index, no segment, small
    /// displacement) is returned as-is, so the load carries the displacement
    /// and no scratch is spent. Anything else is materialised into a scratch
    /// register and returned as `(scratch, 0)`. Returns `None` for an
    /// addressing form the transpiler declines — a 32-bit-wrapped address, a
    /// base or index that is not a full register — so the caller defers.
    fn address(&mut self, quick: &Quick) -> Option<(u8, i32)> {
        match quick.address {
            Address::Fixed(at) if !quick.segmented => {
                // A fixed (absolute or `%rip`-relative) address: materialise
                // it into a scratch and load from `[scratch + 0]`.
                let base = self.temp();
                self.li(base, at, false);
                Some((base, 0))
            }
            Address::Fixed(at) => {
                // `%fs`-relative fixed address: base + fs.
                let base = self.temp();
                self.li(base, at, false);
                self.add_fs(base);
                Some((base, 0))
            }
            Address::Computed { displacement, base, index, scale, narrow } => {
                if narrow {
                    // The 32-bit address wrap reads the base and index at
                    // their own width and masks the sum; deferred for now.
                    return None;
                }
                let base_slice = full_register(base)?;
                let index_slice = full_register(index)?;

                // The simple shape: one base register and a displacement,
                // nothing else. The load rides the displacement.
                if index_slice.is_none()
                    && !quick.segmented
                    && let Some(base) = base_slice
                    && let Ok(disp) = i32::try_from(displacement as i64)
                {
                    return Some((base.number, disp));
                }

                let target = self.temp();
                self.li(target, displacement, false);
                if let Some(base) = base_slice {
                    self.push(encode(Op::Add, target, target, base.number, Width::Qword, false, true, false, 0, 0));
                }
                if let Some(index) = index_slice {
                    let log = match scale {
                        1 => 0,
                        2 => 1,
                        4 => 2,
                        8 => 3,
                        _ => return None,
                    };
                    self.push(encode(Op::Lea, target, target, index.number, Width::Qword, false, false, false, log, 0));
                }
                if quick.segmented {
                    self.add_fs(target);
                }
                Some((target, 0))
            }
        }
    }

    /// `regs[register] += fs_base`.
    fn add_fs(&mut self, register: u8) {
        let fs = self.temp();
        self.push(encode(Op::LoadFs, fs, 0, 0, Width::Qword, false, false, false, 0, 0));
        self.push(encode(Op::Add, register, register, fs, Width::Qword, false, true, false, 0, 0));
    }

    // ---- regular instructions -------------------------------------------

    /// Emits one non-transfer instruction, or a `Defer` if it is not
    /// modelled.
    fn regular(&mut self, quick: &Quick, instruction: &Instruction) {
        if self.try_regular(quick, instruction).is_none() {
            self.defer(instruction);
        }
    }

    /// Hands one guest instruction back to the interpreter.
    fn defer(&mut self, instruction: &Instruction) {
        // Rewind anything a half-finished lowering emitted for this
        // instruction, so a `Defer` is the instruction's only trace and the
        // interpreter runs it from a clean register file.
        self.rewind_current();
        self.push(encode(Op::Defer, 0, 0, 0, Width::Qword, false, false, false, 0, 0));
        // The following word carries the guest address to interpret.
        self.push(instruction.ip());
    }

    /// Drops every word emitted for the current instruction — used when a
    /// lowering gives up partway and defers instead.
    fn rewind_current(&mut self) {
        let start = self
            .starts
            .last()
            .map(|(_, offset)| *offset)
            .unwrap_or(0);
        self.code.truncate(start);
        self.ip.truncate(start);
    }

    /// Attempts a non-transfer instruction. `None` means "declined, defer".
    fn try_regular(&mut self, quick: &Quick, instruction: &Instruction) -> Option<()> {
        match quick.op {
            quick::Op::Nop => {
                // Retire without an effect: a scratch self-move.
                let scratch = self.temp();
                self.push(encode(Op::Mov, scratch, scratch, 0, Width::Qword, false, false, true, 0, 0));
                Some(())
            }
            quick::Op::Mov => self.emit_mov(quick),
            quick::Op::Lea => self.emit_lea(quick),
            quick::Op::Add => self.emit_alu(quick, Op::Add),
            quick::Op::Sub => self.emit_alu(quick, Op::Sub),
            quick::Op::Or => self.emit_alu(quick, Op::Or),
            quick::Op::Xor => self.emit_alu(quick, Op::Xor),
            quick::Op::And => self.emit_alu(quick, Op::And),
            quick::Op::Cmp => self.emit_alu(quick, Op::Cmp),
            quick::Op::Test => self.emit_alu(quick, Op::Test),
            quick::Op::Push => self.emit_push(quick),
            quick::Op::Pop => self.emit_pop(quick),
            quick::Op::Widen => self.emit_widen(quick, Op::Widen),
            quick::Op::WidenSigned => self.emit_widen(quick, Op::WidenSigned),
            // A conditional branch in the block's middle (or a capped block's
            // last instruction): a `BrIf` whose target is resolved later.
            quick::Op::Jcc => self.emit_jcc_branch(quick),
            // The SSE2 subset glibc's string/memory routines use.
            quick::Op::VecMov => self.emit_vec_move(quick),
            quick::Op::VecAnd => self.emit_vec_binary(quick, Op::VecAnd),
            quick::Op::VecXor => self.emit_vec_binary(quick, Op::VecXor),
            quick::Op::VecCmpEqB => self.emit_vec_binary(quick, Op::VecCmpEqB),
            quick::Op::VecMask => self.emit_vec_mask(quick),
            // Not lowered by `Quick`, but simple enough to lower straight
            // from the instruction.
            quick::Op::General => self
                .emit_extra(instruction)
                .or_else(|| self.emit_mul(instruction))
                .or_else(|| self.emit_shift(instruction))
                .or_else(|| self.emit_rotate(instruction))
                .or_else(|| self.emit_setcc(instruction))
                .or_else(|| self.emit_cmov(instruction))
                .or_else(|| self.emit_carrying(instruction))
                .or_else(|| self.emit_divide(instruction)),
            // Vector ops and everything else defer.
            _ => None,
        }
    }

    /// The register a `Source` names, if it is a plain general register with
    /// no high-byte complication; otherwise `None` (defer).
    fn register(&self, source: Source) -> Option<Slice> {
        match source {
            Source::Register(slice) if !slice.high_byte => Some(slice),
            _ => None,
        }
    }

    fn emit_mov(&mut self, quick: &Quick) -> Option<()> {
        let width = quick.width;
        match (quick.destination, quick.source) {
            (Source::Register(_), Source::Memory) => {
                let dst = self.register(quick.destination)?;
                let (base, disp) = self.address(quick)?;
                self.push(encode(Op::Load, dst.number, base, 0, width, false, false, true, 0, disp as u32));
                Some(())
            }
            (Source::Memory, source) => {
                let value = self.value_into_temp(source, width)?;
                let (base, disp) = self.address(quick)?;
                self.push(encode(Op::Store, 0, base, value, width, false, false, true, 0, disp as u32));
                Some(())
            }
            (Source::Register(_), Source::Register(_)) => {
                let dst = self.register(quick.destination)?;
                let src = self.register(quick.source)?;
                self.push(encode(Op::Mov, dst.number, src.number, 0, width, false, false, true, 0, 0));
                Some(())
            }
            (Source::Register(_), Source::Immediate(value)) => {
                let dst = self.register(quick.destination)?;
                match width {
                    Width::Qword | Width::Dword => self.li(dst.number, value, true),
                    // A byte- or word-wide immediate move preserves the rest
                    // of the register, so it goes through a width-aware move.
                    _ => {
                        let scratch = self.temp();
                        self.li(scratch, value, false);
                        self.push(encode(Op::Mov, dst.number, scratch, 0, width, false, false, true, 0, 0));
                    }
                }
                Some(())
            }
            _ => None,
        }
    }

    fn emit_lea(&mut self, quick: &Quick) -> Option<()> {
        let dst = self.register(quick.destination)?;
        let (base, disp) = self.address(quick)?;
        if disp == 0 {
            self.push(encode(Op::Mov, dst.number, base, 0, Width::Qword, false, false, true, 0, 0));
        } else {
            // `regs[dst] = regs[base] + sign_extend(disp)`. There is no zero
            // register to feed `Lea`'s index, so the displacement — which is
            // signed, and would be zero-extended by an immediate `Add` — is
            // materialised full-width and added. (`Add` with the no-flags
            // modifier, since `lea` touches no flags.)
            let scratch = self.temp();
            self.li(scratch, disp as i64 as u64, false);
            self.push(encode(Op::Add, dst.number, base, scratch, Width::Qword, false, true, true, 0, 0));
        }
        Some(())
    }

    fn emit_alu(&mut self, quick: &Quick, op: Op) -> Option<()> {
        let width = quick.width;
        let no_flags = self.flags_dead[self.index];
        // At most one operand is memory. Three shapes cover the rest.
        match (quick.destination, quick.source) {
            // `op reg, [mem]` — load the source into a scratch, compute into
            // the destination register.
            (Source::Register(_), Source::Memory) => {
                let dst = self.register(quick.destination)?;
                let (base, disp) = self.address(quick)?;
                let value = self.temp();
                self.push(encode(Op::Load, value, base, 0, width, false, false, false, 0, disp as u32));
                self.push(encode(op, dst.number, dst.number, value, width, false, no_flags, true, 0, 0));
                Some(())
            }
            // `op [mem], reg/imm`. A write-back op is a read-modify-write, so
            // it fuses into one `MemRmw` — the superinstruction reference
            // counting turns on. `cmp`/`test` do not write back, so they stay
            // a load and a compare.
            (Source::Memory, source) => {
                let (base, disp) = self.address(quick)?;
                if let Some(sub) = mem_rmw_sub(op) {
                    let rhs = match source {
                        Source::Register(slice) if !slice.high_byte => slice.number,
                        Source::Immediate(value) => {
                            let scratch = self.temp();
                            self.li(scratch, value, false);
                            scratch
                        }
                        _ => return None,
                    };
                    self.push(encode(Op::MemRmw, 0, base, rhs, width, false, no_flags, true, sub, disp as u32));
                    return Some(());
                }
                // `cmp`/`test [mem], …`: load then compare, no store.
                let value = self.temp();
                self.push(encode(Op::Load, value, base, 0, width, false, false, false, 0, disp as u32));
                self.emit_alu_reg(op, value, value, source, width, no_flags, true)
            }
            // `op reg, reg/imm`.
            (Source::Register(_), _) => {
                let dst = self.register(quick.destination)?;
                self.emit_alu_reg(op, dst.number, dst.number, quick.source, width, no_flags, true)
            }
            _ => None,
        }
    }

    /// Emits an ALU op `d = a OP rhs` where `rhs` is a register or immediate
    /// (never memory — the caller has already resolved a memory operand into
    /// a register).
    fn emit_alu_reg(
        &mut self,
        op: Op,
        d: u8,
        a: u8,
        rhs: Source,
        width: Width,
        no_flags: bool,
        retire: bool,
    ) -> Option<()> {
        match rhs {
            Source::Register(slice) if !slice.high_byte => {
                self.push(encode(op, d, a, slice.number, width, false, no_flags, retire, 0, 0));
                Some(())
            }
            Source::Immediate(value) => {
                if fits_immediate(width, value) {
                    self.push(encode(op, d, a, 0, width, true, no_flags, retire, 0, value as u32));
                } else {
                    let scratch = self.temp();
                    self.li(scratch, value, false);
                    self.push(encode(op, d, a, scratch, width, false, no_flags, retire, 0, 0));
                }
                Some(())
            }
            _ => None,
        }
    }

    /// Loads a source (register, immediate, or memory) into a fresh scratch
    /// register at the given width, returning the register.
    fn value_into_temp(&mut self, source: Source, _width: Width) -> Option<u8> {
        match source {
            Source::Register(slice) if !slice.high_byte => Some(slice.number),
            Source::Immediate(value) => {
                let scratch = self.temp();
                self.li(scratch, value, false);
                Some(scratch)
            }
            _ => None,
        }
    }

    fn emit_push(&mut self, quick: &Quick) -> Option<()> {
        let width = quick.width;
        let value = match quick.source {
            Source::Register(slice) if !slice.high_byte => slice.number,
            Source::Immediate(value) => {
                let scratch = self.temp();
                self.li(scratch, value, false);
                scratch
            }
            Source::Memory => {
                let (base, disp) = self.address(quick)?;
                let scratch = self.temp();
                self.push(encode(Op::Load, scratch, base, 0, width, false, false, false, 0, disp as u32));
                scratch
            }
            _ => return None,
        };
        self.push(encode(Op::Push, 0, value, 0, width, false, false, true, 0, 0));
        Some(())
    }

    fn emit_pop(&mut self, quick: &Quick) -> Option<()> {
        let width = quick.width;
        match quick.destination {
            Source::Register(slice) if !slice.high_byte => {
                self.push(encode(Op::Pop, slice.number, 0, 0, width, false, false, true, 0, 0));
                Some(())
            }
            Source::Memory => {
                let scratch = self.temp();
                self.push(encode(Op::Pop, scratch, 0, 0, width, false, false, false, 0, 0));
                let (base, disp) = self.address(quick)?;
                self.push(encode(Op::Store, 0, base, scratch, width, false, false, true, 0, disp as u32));
                Some(())
            }
            _ => None,
        }
    }

    fn emit_widen(&mut self, quick: &Quick, op: Op) -> Option<()> {
        let dst = self.register(quick.destination)?;
        let source_class = quick.source_width as u8;
        match quick.source {
            Source::Register(slice) if !slice.high_byte => {
                self.push(encode(op, dst.number, slice.number, 0, quick.width, false, false, true, source_class, 0));
                Some(())
            }
            Source::Memory => {
                let (base, disp) = self.address(quick)?;
                let scratch = self.temp();
                self.push(encode(Op::Load, scratch, base, 0, quick.source_width, false, false, false, 0, disp as u32));
                self.push(encode(op, dst.number, scratch, 0, quick.width, false, false, true, source_class, 0));
                Some(())
            }
            _ => None,
        }
    }

    /// The ops `Quick` declines but that are cheap to lower straight from the
    /// instruction: `inc`, `dec`, `neg`, `not`.
    fn emit_extra(&mut self, instruction: &Instruction) -> Option<()> {
        if instruction.has_lock_prefix()
            || instruction.has_rep_prefix()
            || instruction.op_count() != 1
        {
            return None;
        }
        let op = match instruction.mnemonic() {
            Mnemonic::Inc => Op::Inc,
            Mnemonic::Dec => Op::Dec,
            Mnemonic::Neg => Op::Neg,
            Mnemonic::Not => Op::Not,
            _ => return None,
        };
        let width = Width::from_bytes(match instruction.op0_kind() {
            OpKind::Register => instruction.op_register(0).size(),
            OpKind::Memory => instruction.memory_size().size(),
            _ => return None,
        })?;
        // `inc`/`dec`/`neg` write flags; `not` writes none (its interpreter
        // arm ignores the modifier). The liveness pass says whether they die.
        let no_flags = self.flags_dead[self.index];
        match instruction.op0_kind() {
            OpKind::Register => {
                let slice = Slice::of(instruction.op_register(0))?;
                if slice.high_byte {
                    return None;
                }
                self.push(encode(op, slice.number, slice.number, 0, width, false, no_flags, true, 0, 0));
                Some(())
            }
            OpKind::Memory => {
                let quick = Quick::lower(instruction);
                // `inc`/`dec` on memory build the address from a synthetic
                // lowering of just the operand, then fuse into one `MemRmw` —
                // the reference-counting superinstruction. `neg`/`not` on
                // memory (no `MemRmw` sub-op) stay a load, op, and store.
                let (base, disp) = self.address_of_instruction(instruction, &quick)?;
                match op {
                    Op::Inc | Op::Dec => {
                        let sub = if matches!(op, Op::Inc) { 5 } else { 6 };
                        self.push(encode(Op::MemRmw, 0, base, 0, width, false, no_flags, true, sub, disp as u32));
                    }
                    _ => {
                        let scratch = self.temp();
                        self.push(encode(Op::Load, scratch, base, 0, width, false, false, false, 0, disp as u32));
                        self.push(encode(op, scratch, scratch, 0, width, false, no_flags, false, 0, 0));
                        self.push(encode(Op::Store, 0, base, scratch, width, false, false, true, 0, disp as u32));
                    }
                }
                Some(())
            }
            _ => None,
        }
    }

    /// Two- and three-operand `imul`, straight from the instruction (`Quick`
    /// declines it). `imul dst, src` is `dst = dst * src`; `imul dst, src,
    /// imm` is `dst = src * imm`. Only the low half is kept, so it is a plain
    /// truncating multiply — but only when `imul`'s flags (CF/OF) are dead,
    /// since [`Op::Mul`] does not compute them; a live-flag `imul`, and any
    /// memory operand or the one-operand `RDX:RAX` form, defers.
    fn emit_mul(&mut self, instruction: &Instruction) -> Option<()> {
        if instruction.mnemonic() != Mnemonic::Imul
            || instruction.has_lock_prefix()
            || !self.flags_dead[self.index]
        {
            return None;
        }
        let dst = Slice::of(instruction.op_register(0))?;
        if dst.high_byte {
            return None;
        }
        let width = Width::from_bytes(instruction.op_register(0).size())?;
        match instruction.op_count() {
            2 => {
                // `dst = dst * src`, src a register (memory defers).
                if instruction.op1_kind() != OpKind::Register {
                    return None;
                }
                let src = Slice::of(instruction.op_register(1))?;
                if src.high_byte {
                    return None;
                }
                self.push(encode(Op::Mul, dst.number, dst.number, src.number, width, false, false, true, 0, 0));
                Some(())
            }
            3 => {
                // `dst = src * imm`, src a register (memory defers).
                if instruction.op1_kind() != OpKind::Register {
                    return None;
                }
                let src = Slice::of(instruction.op_register(1))?;
                if src.high_byte {
                    return None;
                }
                let value = width.truncate(instruction.immediate(2));
                if fits_immediate(width, value) {
                    self.push(encode(Op::Mul, dst.number, src.number, 0, width, true, false, true, 0, value as u32));
                } else {
                    let scratch = self.temp();
                    self.li(scratch, value, false);
                    self.push(encode(Op::Mul, dst.number, src.number, scratch, width, false, false, true, 0, 0));
                }
                Some(())
            }
            _ => None,
        }
    }

    /// `shl`/`sal`/`shr`/`sar` on a register, by an immediate or by `cl`.
    /// Only when the shift's flags are dead — [`Op::Shl`] and its kin do not
    /// compute the (fiddly, count-dependent) shift flags — so a live-flag
    /// shift, a memory destination, or a high-byte register defers.
    fn emit_shift(&mut self, instruction: &Instruction) -> Option<()> {
        let op = match instruction.mnemonic() {
            Mnemonic::Shl | Mnemonic::Sal => Op::Shl,
            Mnemonic::Shr => Op::Shr,
            Mnemonic::Sar => Op::Sar,
            _ => return None,
        };
        if instruction.has_lock_prefix() || !self.flags_dead[self.index] {
            return None;
        }
        if instruction.op0_kind() != OpKind::Register {
            return None;
        }
        let dst = Slice::of(instruction.op_register(0))?;
        if dst.high_byte {
            return None;
        }
        let width = Width::from_bytes(instruction.op_register(0).size())?;
        // The count: an immediate, or `cl` (register one), or an implicit one.
        match instruction.op_count() {
            1 => {
                // `shl reg` — the implicit count of one.
                self.push(encode(op, dst.number, dst.number, 0, width, true, true, true, 0, 1));
                Some(())
            }
            2 => match instruction.op1_kind() {
                OpKind::Immediate8 => {
                    let count = instruction.immediate8() as u32;
                    self.push(encode(op, dst.number, dst.number, 0, width, true, true, true, 0, count));
                    Some(())
                }
                OpKind::Register if instruction.op_register(1) == Register::CL => {
                    // Count in `cl` — register one, read masked by the op.
                    self.push(encode(op, dst.number, dst.number, 1, width, false, true, true, 0, 0));
                    Some(())
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// `rol`/`ror` on a register, by an immediate or `cl`, when the rotate's
    /// flags (CF/OF) are dead. `rcl`/`rcr` (through the carry) and memory
    /// destinations defer.
    fn emit_rotate(&mut self, instruction: &Instruction) -> Option<()> {
        let op = match instruction.mnemonic() {
            Mnemonic::Rol => Op::Rol,
            Mnemonic::Ror => Op::Ror,
            _ => return None,
        };
        if instruction.has_lock_prefix()
            || !self.flags_dead[self.index]
            || instruction.op0_kind() != OpKind::Register
        {
            return None;
        }
        let dst = Slice::of(instruction.op_register(0))?;
        if dst.high_byte {
            return None;
        }
        let width = Width::from_bytes(instruction.op_register(0).size())?;
        match instruction.op_count() {
            1 => {
                self.push(encode(op, dst.number, dst.number, 0, width, true, false, true, 0, 1));
                Some(())
            }
            2 => match instruction.op1_kind() {
                OpKind::Immediate8 => {
                    let count = instruction.immediate8() as u32;
                    self.push(encode(op, dst.number, dst.number, 0, width, true, false, true, 0, count));
                    Some(())
                }
                OpKind::Register if instruction.op_register(1) == Register::CL => {
                    self.push(encode(op, dst.number, dst.number, 1, width, false, false, true, 0, 0));
                    Some(())
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// `setcc` into a byte register — memory destinations defer. The condition
    /// comes from the interpreter's own mnemonic table.
    fn emit_setcc(&mut self, instruction: &Instruction) -> Option<()> {
        let (kind, condition) = crate::exec::conditional_of(instruction.mnemonic())?;
        if kind != crate::exec::Conditional::Set
            || instruction.has_lock_prefix()
            || instruction.op0_kind() != OpKind::Register
        {
            return None;
        }
        let dst = Slice::of(instruction.op_register(0))?;
        if dst.high_byte {
            return None;
        }
        self.push(encode(Op::Setcc, dst.number, 0, 0, Width::Byte, false, false, true, condition as u8, 0));
        Some(())
    }

    /// `cmovcc dst, src` with a register source — a memory source (which
    /// `Quick` does not give an address for here) defers.
    fn emit_cmov(&mut self, instruction: &Instruction) -> Option<()> {
        let (kind, condition) = crate::exec::conditional_of(instruction.mnemonic())?;
        if kind != crate::exec::Conditional::Move
            || instruction.has_lock_prefix()
            || instruction.op1_kind() != OpKind::Register
        {
            return None;
        }
        let dst = Slice::of(instruction.op_register(0))?;
        let src = Slice::of(instruction.op_register(1))?;
        if dst.high_byte || src.high_byte {
            return None;
        }
        let width = Width::from_bytes(instruction.op_register(0).size())?;
        self.push(encode(Op::Cmov, dst.number, src.number, 0, width, false, false, true, condition as u8, 0));
        Some(())
    }

    /// `adc`/`sbb dst, src` with a register destination and a register or
    /// immediate source — memory operands defer. Always records flags (a
    /// carry chain reads them).
    fn emit_carrying(&mut self, instruction: &Instruction) -> Option<()> {
        let op = match instruction.mnemonic() {
            Mnemonic::Adc => Op::Adc,
            Mnemonic::Sbb => Op::Sbb,
            _ => return None,
        };
        if instruction.has_lock_prefix()
            || instruction.op_count() != 2
            || instruction.op0_kind() != OpKind::Register
        {
            return None;
        }
        let dst = Slice::of(instruction.op_register(0))?;
        if dst.high_byte {
            return None;
        }
        let width = Width::from_bytes(instruction.op_register(0).size())?;
        match instruction.op1_kind() {
            OpKind::Register => {
                let src = Slice::of(instruction.op_register(1))?;
                if src.high_byte {
                    return None;
                }
                self.push(encode(op, dst.number, dst.number, src.number, width, false, false, true, 0, 0));
                Some(())
            }
            OpKind::Memory => None,
            _ => {
                let value = width.truncate(instruction.immediate(1));
                if fits_immediate(width, value) {
                    self.push(encode(op, dst.number, dst.number, 0, width, true, false, true, 0, value as u32));
                } else {
                    let scratch = self.temp();
                    self.li(scratch, value, false);
                    self.push(encode(op, dst.number, dst.number, scratch, width, false, false, true, 0, 0));
                }
                Some(())
            }
        }
    }

    /// `movups`/`movaps`/`movdqu`/`movdqa`: an XMM register from another XMM
    /// register or memory, or memory from an XMM register.
    fn emit_vec_move(&mut self, quick: &Quick) -> Option<()> {
        match (quick.destination, quick.source) {
            (Source::Vector(dst), Source::Vector(src)) => {
                self.push(encode(Op::VecMov, dst, 0, src, Width::Qword, false, false, true, 0, 0));
                Some(())
            }
            (Source::Vector(dst), Source::Memory) => {
                let (base, disp) = self.address(quick)?;
                self.push(encode(Op::VecMov, dst, base, 0, Width::Qword, true, false, true, 0, disp as u32));
                Some(())
            }
            (Source::Memory, Source::Vector(src)) => {
                let (base, disp) = self.address(quick)?;
                self.push(encode(Op::VecStore, 0, base, src, Width::Qword, false, false, true, 0, disp as u32));
                Some(())
            }
            _ => None,
        }
    }

    /// `pand`/`pxor`/`pcmpeqb`: an XMM destination combined with an XMM
    /// register or memory.
    fn emit_vec_binary(&mut self, quick: &Quick, op: Op) -> Option<()> {
        let Source::Vector(dst) = quick.destination else {
            return None;
        };
        match quick.source {
            Source::Vector(src) => {
                self.push(encode(op, dst, 0, src, Width::Qword, false, false, true, 0, 0));
                Some(())
            }
            Source::Memory => {
                let (base, disp) = self.address(quick)?;
                self.push(encode(op, dst, base, 0, Width::Qword, true, false, true, 0, disp as u32));
                Some(())
            }
            _ => None,
        }
    }

    /// `pmovmskb`: the sixteen byte sign bits of an XMM register into a general
    /// register.
    fn emit_vec_mask(&mut self, quick: &Quick) -> Option<()> {
        let dst = self.register(quick.destination)?;
        let Source::Vector(src) = quick.source else {
            return None;
        };
        self.push(encode(Op::VecMask, dst.number, 0, src, Width::Qword, false, false, true, 0, 0));
        Some(())
    }

    /// `div`/`idiv` at dword or qword width with a register divisor. The byte
    /// and word forms (with their `AX`/`AH` shape) and a memory divisor defer.
    fn emit_divide(&mut self, instruction: &Instruction) -> Option<()> {
        let op = match instruction.mnemonic() {
            Mnemonic::Div => Op::Div,
            Mnemonic::Idiv => Op::Idiv,
            _ => return None,
        };
        if instruction.has_lock_prefix()
            || instruction.op_count() != 1
            || instruction.op0_kind() != OpKind::Register
        {
            return None;
        }
        let divisor = Slice::of(instruction.op_register(0))?;
        if divisor.high_byte {
            return None;
        }
        let width = Width::from_bytes(instruction.op_register(0).size())?;
        if !matches!(width, Width::Dword | Width::Qword) {
            return None;
        }
        self.push(encode(op, 0, divisor.number, 0, width, false, false, true, 0, 0));
        Some(())
    }

    /// The address of a memory operand when the caller holds the instruction
    /// rather than a `Quick` whose address field is populated — the extra-op
    /// path. Rebuilds the `Quick` address form and reuses [`Self::address`].
    fn address_of_instruction(&mut self, instruction: &Instruction, quick: &Quick) -> Option<(u8, i32)> {
        let _ = instruction;
        if quick.op == quick::Op::General {
            return None;
        }
        self.address(quick)
    }

    // ---- terminal transfers ---------------------------------------------

    fn terminal(&mut self, quick: &Quick, instruction: &Instruction) {
        let ok = match quick.op {
            quick::Op::Jmp => self.emit_jmp(quick),
            quick::Op::Call => self.emit_call(quick, instruction),
            quick::Op::Ret => self.emit_ret(quick),
            _ => None,
        };
        if ok.is_none() {
            // A transfer the transpiler cannot lower — an indirect branch
            // through an addressing form it declines, say. Defer it: the
            // interpreter performs the transfer and sets `rip`, and the run
            // loop resolves the target.
            self.defer(instruction);
        }
    }

    fn emit_jmp(&mut self, quick: &Quick) -> Option<()> {
        match quick.source {
            Source::Immediate(target) => {
                if let Some(offset) = self.internal(target) {
                    self.push(encode(Op::Br, 0, 0, 0, Width::Qword, false, false, true, 0, offset as u32));
                } else {
                    self.exit_to_address(target, true);
                }
                Some(())
            }
            Source::Register(slice) if !slice.high_byte => {
                let scratch = self.temp();
                self.mov_full(scratch, slice.number);
                self.exit_to_register(scratch, true);
                Some(())
            }
            Source::Memory => {
                let (base, disp) = self.address(quick)?;
                let scratch = self.temp();
                self.push(encode(Op::Load, scratch, base, 0, Width::Qword, false, false, false, 0, disp as u32));
                self.exit_to_register(scratch, true);
                Some(())
            }
            _ => None,
        }
    }

    /// A conditional branch anywhere in the block. Emits a `BrIf` whose taken
    /// target is filled in by [`Self::resolve_branches`] once every internal
    /// target is known; the not-taken path simply falls through to the next
    /// word, which is the next instruction — the block's fall-through.
    fn emit_jcc_branch(&mut self, quick: &Quick) -> Option<()> {
        let target = match quick.source {
            Source::Immediate(target) => target,
            _ => return None,
        };
        let condition = quick.condition as u8;
        let offset = self.code.len();
        self.push(encode(Op::BrIf, 0, 0, 0, Width::Qword, false, false, true, condition, 0));
        self.branches.push((offset, target, self.current));
        Some(())
    }

    fn emit_call(&mut self, quick: &Quick, instruction: &Instruction) -> Option<()> {
        let return_address = instruction.next_ip();
        // Resolve the target first — an indirect call through memory can name
        // a location the pushed return address would overwrite.
        let target = self.temp();
        match quick.source {
            Source::Immediate(address) => self.li(target, address, false),
            Source::Register(slice) if !slice.high_byte => self.mov_full(target, slice.number),
            Source::Memory => {
                let (base, disp) = self.address(quick)?;
                self.push(encode(Op::Load, target, base, 0, Width::Qword, false, false, false, 0, disp as u32));
            }
            _ => return None,
        }
        let return_register = self.temp();
        self.li(return_register, return_address, false);
        self.push(encode(Op::Push, 0, return_register, 0, Width::Qword, false, false, false, 0, 0));
        self.exit_to_register(target, true);
        Some(())
    }

    fn emit_ret(&mut self, quick: &Quick) -> Option<()> {
        let target = self.temp();
        self.push(encode(Op::Pop, target, 0, 0, Width::Qword, false, false, false, 0, 0));
        if let Source::Immediate(extra) = quick.source
            && extra != 0
        {
            if !fits_immediate(Width::Qword, extra) {
                return None;
            }
            self.push(encode(Op::Add, STACK_POINTER as u8, STACK_POINTER as u8, 0, Width::Qword, true, true, false, 0, extra as u32));
        }
        self.exit_to_register(target, true);
        Some(())
    }
}

/// Classifies a fusion candidate — the producer half of a `producer; jcc`
/// pair — into its `FusedBranch` op-code, width, destination register, and
/// right-hand side, or `None` when it cannot fuse (a memory operand, a
/// high-byte register, or an op that is not a fusible flag producer).
fn fusible_producer(quick: &Quick, instruction: &Instruction) -> Option<(Op, Width, u8, Rhs)> {
    let op = match quick.op {
        quick::Op::Add => Op::Add,
        quick::Op::Sub => Op::Sub,
        quick::Op::Cmp => Op::Cmp,
        quick::Op::And => Op::And,
        quick::Op::Or => Op::Or,
        quick::Op::Xor => Op::Xor,
        quick::Op::Test => Op::Test,
        // `inc`/`dec` come straight from the instruction — a register operand
        // only, no right-hand side.
        quick::Op::General => {
            let op = match instruction.mnemonic() {
                Mnemonic::Inc => Op::Inc,
                Mnemonic::Dec => Op::Dec,
                _ => return None,
            };
            if instruction.op0_kind() != OpKind::Register {
                return None;
            }
            let dest = Slice::of(instruction.op_register(0))?;
            if dest.high_byte {
                return None;
            }
            let width = Width::from_bytes(instruction.op_register(0).size())?;
            return Some((op, width, dest.number, Rhs::None));
        }
        _ => return None,
    };
    let dest = match quick.destination {
        Source::Register(slice) if !slice.high_byte => slice.number,
        _ => return None,
    };
    let rhs = match quick.source {
        Source::Register(slice) if !slice.high_byte => Rhs::Register(slice.number),
        Source::Immediate(value) => Rhs::Immediate(value),
        _ => return None,
    };
    Some((op, quick.width, dest, rhs))
}

/// A source's register when it is a full-width (qword) general register — the
/// only form an address base or index may take here; a narrower one means a
/// 32-bit-wrapped address, which the caller declines.
fn full_register(register: Option<Slice>) -> Option<Option<Slice>> {
    match register {
        None => Some(None),
        Some(slice) if slice.width == Width::Qword && !slice.high_byte => Some(Some(slice)),
        Some(_) => None,
    }
}

/// The `MemRmw` sub-op code for a write-back ALU op, or `None` for `cmp`/
/// `test`, which do not write back and so are not read-modify-writes.
fn mem_rmw_sub(op: Op) -> Option<u8> {
    Some(match op {
        Op::Add => 0,
        Op::Sub => 1,
        Op::Or => 2,
        Op::Xor => 3,
        Op::And => 4,
        _ => return None,
    })
}

/// Whether a width-truncated immediate fits the 32-bit immediate field
/// zero-extended: always for widths up to a dword, and for a qword only when
/// the value is within 32 bits. A value outside it is materialised with
/// `Li64` instead.
fn fits_immediate(width: Width, value: u64) -> bool {
    match width {
        Width::Byte | Width::Word | Width::Dword => true,
        Width::Qword => value <= u64::from(u32::MAX),
    }
}

