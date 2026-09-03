//! Execution: what each instruction means, in Rust.
//!
//! The semantics here are not new. They are the same facts the translator
//! encodes — which flags an operation writes and by what rule, `dec`
//! preserving the carry, `xchg` reading both operands before writing either,
//! a 32-bit write clearing the upper half, the segment override on `lea`
//! being ineffectual, `cmov` writing its destination whether or not it
//! moves — ported from emitted wasm into direct Rust, and checked by the
//! same differential corpus that hardened them the first time.
//!
//! The breadth grind is the same campaign shape as SSE and x87 were: **an
//! instruction this engine cannot execute is a loud error naming itself**,
//! never a silent approximation, and the worklist is driven by what real
//! programs actually reach for.
//!
//! Two structural rules hold everywhere in this module:
//!
//! - **The interpreter's Rust stack is not the guest's.** No guest value
//!   lives on it and nothing about guest `%rsp` constrains it. The moment
//!   the two mix — a helper "borrowing" guest stack for scratch — the red
//!   zone class returns from the dead.
//! - **Every guest memory access goes through [`Space`].** Not for tidiness:
//!   it is where the permission check and the code-page test live, and an
//!   access that reaches around it is a missing `SIGSEGV` or a stale block.

pub mod vector;
pub mod x87;

use iced_x86::{Code, Instruction, Mnemonic, OpKind, Register};

use crate::flags::{Condition, Rule, bit};
use crate::quick::{Address, Op, Quick, Source};
use crate::space::{Fault, Space};
use crate::state::{Slice, Tcb, Width};

/// The register file indices the interpreter names directly, because the
/// architecture does: an implicit accumulator, the counter every repeated
/// instruction reads, the pair the string instructions walk, and the two the
/// `syscall` instruction writes.
mod number {
    pub const RAX: usize = 0;
    pub const RCX: usize = 1;
    pub const RDX: usize = 2;
    pub const RBP: usize = 5;
    pub const RSI: usize = 6;
    pub const RDI: usize = 7;
    pub const R11: usize = 11;
}

/// An instruction the engine does not implement, named well enough to be a
/// worklist entry on its own.
///
/// The `Code` is the exact encoding, not just the mnemonic: "`Mov`" sends
/// someone back to the trace, "`Mov_rm64_imm32`" does not. `detail` is for
/// the case where the instruction *is* implemented and one of its forms is
/// not — a segment nobody has needed, an operand kind that has not come up.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Unsupported {
    pub address: u64,
    pub code: Code,
    pub mnemonic: Mnemonic,
    pub detail: Option<&'static str>,
}

impl Unsupported {
    fn at(instruction: &Instruction, detail: Option<&'static str>) -> Self {
        Self {
            address: instruction.ip(),
            code: instruction.code(),
            mnemonic: instruction.mnemonic(),
            detail,
        }
    }
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            out,
            "vm: {:?} ({:?}) at {:#x} is not implemented",
            self.mnemonic, self.code, self.address
        )?;
        if let Some(detail) = self.detail {
            write!(out, ": {detail}")?;
        }
        Ok(())
    }
}

/// Why execution stopped short of retiring an instruction.
///
/// Everything except [`Trap::Unsupported`] is a condition the guest can
/// observe and, given a handler, survive — which is the fidelity class the
/// ahead-of-time design documents as impossible, arriving because there is a
/// fetch and a load and a store to hang the check on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Trap {
    /// An access the address space refused: `SIGSEGV`, with `si_addr`.
    Fault(Fault),
    /// Bytes that are not an instruction: `SIGILL`.
    Undefined { address: u64 },
    /// An instruction userspace may not execute: `SIGSEGV`, as Linux
    /// delivers for a general-protection fault.
    Privileged { address: u64 },
    /// An aligned vector access on an address that is not aligned. Also a
    /// general-protection fault, and also `SIGSEGV` — kept apart from
    /// [`Trap::Privileged`] only so a report says which of the two it was.
    Misaligned { address: u64 },
    /// `int3`: `SIGTRAP`.
    Breakpoint { address: u64 },
    /// Divide by zero, or a quotient that does not fit: `SIGFPE`.
    DivideError { address: u64 },
    /// Not a guest-visible condition at all. The engine is incomplete, and
    /// says which instruction made that visible.
    Unsupported(Unsupported),
}

impl From<Fault> for Trap {
    fn from(fault: Fault) -> Self {
        Trap::Fault(fault)
    }
}

/// What retiring an instruction leaves for the caller to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// Nothing. `rip` names whatever runs next — which may be the same
    /// instruction again, for a `rep` that has not finished.
    Retired,
    /// The instruction was a `syscall`. `rip` is already past it, and
    /// `%rcx` and `%r11` already hold what the hardware would have put
    /// there, so the kernel call is the caller's to make.
    Syscall,
}

/// The machine, for as long as it takes to run some instructions.
///
/// A transient view rather than an owning struct, because the block cache
/// has to stay reachable while a block executes: the two are disjoint
/// fields of the engine, and this is what says so to the compiler.
pub struct Cpu<'a> {
    pub tcb: &'a mut Tcb,
    pub space: &'a mut Space,
}

impl<'a> Cpu<'a> {
    pub fn new(tcb: &'a mut Tcb, space: &'a mut Space) -> Self {
        Self { tcb, space }
    }

    // ---- operands --------------------------------------------------------

    /// The register file slice an operand names, or a loud error saying
    /// which register was not one.
    fn slice(instruction: &Instruction, register: Register) -> Result<Slice, Trap> {
        Slice::of(register).ok_or_else(|| {
            Trap::Unsupported(Unsupported::at(
                instruction,
                Some("a register outside the general-purpose file"),
            ))
        })
    }

    /// The width an operand is accessed at.
    fn width(instruction: &Instruction, operand: u32) -> Result<Width, Trap> {
        let bytes = match instruction.op_kind(operand) {
            OpKind::Register => instruction.op_register(operand).size(),
            OpKind::Memory => instruction.memory_size().size(),
            _ => {
                // An immediate has no width of its own — it takes the
                // instruction's, which is operand zero's.
                return Self::width(instruction, 0);
            }
        };
        Width::from_bytes(bytes).ok_or_else(|| {
            Trap::Unsupported(Unsupported::at(instruction, Some("an operand width")))
        })
    }

    /// The address a memory operand computes, *without* a segment base.
    ///
    /// The address-size prefix is not decoration: glibc reaches for it as
    /// arithmetic rather than as addressing — `lea edx, [ecx-1]` is a
    /// subtract that wraps at four gigabytes and is a byte shorter than the
    /// subtract — and `__strrchr_sse2` is one of the functions that does. So
    /// the whole sum is computed in thirty-two bits and truncated, applied
    /// to the sum rather than to each term, because that is where the wrap
    /// happens.
    fn unsegmented_address(&self, instruction: &Instruction) -> Result<u64, Trap> {
        let base = instruction.memory_base();
        let index = instruction.memory_index();

        // `%rip`-relative: iced has already folded the instruction's own
        // length into the displacement, so the displacement *is* the
        // address and the base contributes nothing further.
        if matches!(base, Register::RIP | Register::EIP) {
            return Ok(instruction.memory_displacement64());
        }

        let narrow = base.size() == 4 || (index != Register::None && index.size() == 4);
        let mut address = instruction.memory_displacement64();
        if base != Register::None {
            if base.size() != 8 && !narrow {
                return Err(Trap::Unsupported(Unsupported::at(
                    instruction,
                    Some("an address base that is neither four nor eight bytes"),
                )));
            }
            address = address.wrapping_add(self.tcb.read_register(Self::slice(instruction, base)?));
        }
        if index != Register::None {
            if index.size() != 8 && !narrow {
                return Err(Trap::Unsupported(Unsupported::at(
                    instruction,
                    Some("an address index that is neither four nor eight bytes"),
                )));
            }
            let scaled = self
                .tcb
                .read_register(Self::slice(instruction, index)?)
                .wrapping_mul(u64::from(instruction.memory_index_scale()));
            address = address.wrapping_add(scaled);
        }
        Ok(match narrow {
            true => address & 0xffff_ffff,
            false => address,
        })
    }

    /// The effective address of a memory operand, segment base included.
    ///
    /// In 64-bit mode only `%fs` and `%gs` have a base at all: the
    /// architecture forces `%cs`, `%ds`, `%es` and `%ss` to zero and ignores
    /// their contents, so a prefix naming one changes nothing. That is not a
    /// corner — `notrack`, the hint that says an indirect branch need not
    /// land on an `endbr64`, is *encoded* as a `%ds` prefix, so every switch
    /// in a control-flow-protected binary carries one.
    ///
    /// `%gs` stays a loud error, and now at one site rather than at every
    /// translation site: it genuinely has a base, nothing on this path uses
    /// one, and a libc that reached for it would be a libc nothing here has
    /// been tested against.
    fn address(&self, instruction: &Instruction) -> Result<u64, Trap> {
        let address = self.unsegmented_address(instruction)?;
        match instruction.segment_prefix() {
            Register::FS => Ok(address.wrapping_add(self.tcb.fs_base)),
            Register::None | Register::CS | Register::DS | Register::ES | Register::SS => {
                Ok(address)
            }
            _ => Err(Trap::Unsupported(Unsupported::at(
                instruction,
                Some("a `%gs`-prefixed memory operand"),
            ))),
        }
    }

    /// Runs a pre-decoded instruction, or falls back to the general path.
    ///
    /// The fallback is not a fast-path failure mode — it is the design.
    /// [`crate::quick`] lowers what it fully understands and declines
    /// everything else, so this arm carries the instructions nobody has got
    /// to yet at exactly the speed they ran before.
    /// Retires one instruction that is known to fall through, without
    /// touching `rip`.
    ///
    /// The run loop used to write `rip` after every instruction and read it
    /// back twice — against the instruction's own address and against the
    /// next one — to work out whether it fell through, branched, or stayed
    /// put. For every instruction but a block's last, that was settled when
    /// the block was decoded: blocks end at control transfers, so nothing
    /// in the middle can branch, and [`crate::block::Block::simple`] says
    /// whether anything in the middle carries a repeat prefix either.
    ///
    /// So this leaves `rip` stale on purpose and the loop makes it good on
    /// the way out — at a trap, at the end of a quantum, or when a store
    /// lands on cached code. All three are cold. What it saves is a store
    /// and two reads of the forty-byte decode per instruction, on the path
    /// that runs a thousand million times, and it was worth 8% of the
    /// engine.
    pub fn advance(&mut self, quick: &Quick, instruction: &Instruction) -> Result<(), Trap> {
        crate::histogram::record(instruction, quick.op != Op::General);
        crate::profile::record(instruction.ip());
        if quick.op.defers_to_step() {
            // The general path counts and sets `rip` itself. Nothing in a
            // straight-through prefix reads it, so letting it write one is
            // cheaper than keeping a second copy of that function. The
            // vector ops go here too — the interpreter's own vector unit
            // owns them, and only the compiler lowers them.
            self.step(instruction)?;
            return Ok(());
        }
        self.tcb.retired += 1;
        self.quick(quick).map(|_| ())
    }

    pub fn run(&mut self, quick: &Quick, instruction: &Instruction) -> Result<Step, Trap> {
        crate::histogram::record(instruction, quick.op != Op::General);
        crate::profile::record(instruction.ip());
        if quick.op.defers_to_step() {
            return self.step(instruction);
        }
        // The same two lines `step` opens with, and they are not
        // bookkeeping: the run loop reads `rip` afterwards to decide whether
        // the instruction fell through, branched, or stayed put, so a fast
        // path that left it alone would re-run the same instruction until
        // the quantum expired. Nothing lowered here transfers control, so
        // the answer is always the next instruction.
        self.tcb.retired += 1;
        self.tcb.rip = instruction.next_ip();
        self.quick(quick)
    }

    /// The whole fast path: a handful of arms over a `u8`, with every
    /// operand already resolved.
    fn quick(&mut self, quick: &Quick) -> Result<Step, Trap> {
        let width = quick.width;
        // The two ops here that do not read their destination, and they
        // have to be taken before the read rather than after it. A `mov`
        // whose destination is a bad address must fault as a *write* — a
        // read fault names the wrong access, and a handler that resolves
        // faults by mapping the page would map it readable. The load would
        // also be real work nobody asked for.
        match quick.op {
            // `lea` never touches memory at all: its source *is* the
            // address, which is why it is not a load.
            Op::Lea => {
                let at = self.quick_address(quick)?;
                return self.quick_store(quick, quick.destination, width, at);
            }
            Op::Mov => {
                let value = self.quick_load(quick, quick.source, width)?;
                return self.quick_store(quick, quick.destination, width, value);
            }
            Op::Push => {
                let value = self.quick_load(quick, quick.source, width)?;
                return self.push_inline(width, value);
            }
            // The value first, then the destination — so that `pop` into a
            // memory operand computes its address after the stack pointer
            // has moved, which is the order the general path uses and the
            // order the architecture specifies.
            Op::Pop => {
                let value = self.pop_inline(width)?;
                return self.quick_store(quick, quick.destination, width, value);
            }
            Op::Nop => return Ok(Step::Retired),
            // `rip` already names the next instruction — `run` set it — so
            // a branch not taken has nothing left to do, and a branch taken
            // only has to disagree.
            Op::Jcc => {
                if quick.condition.holds(&self.tcb.flags) {
                    self.tcb.rip = match quick.source {
                        Source::Immediate(target) => target,
                        _ => unreachable!("a conditional branch target is always an immediate"),
                    };
                }
                return Ok(Step::Retired);
            }
            Op::Jmp => {
                self.tcb.rip = self.quick_load(quick, quick.source, Width::Qword)?;
                return Ok(Step::Retired);
            }
            // The target before the push, because an indirect call through
            // memory can name a location the push would move — `call
            // *(%rsp)` reads the stack the return address is about to go on.
            Op::Call => {
                let target = self.quick_load(quick, quick.source, Width::Qword)?;
                // Which `run` put there, and is exactly the return address.
                let ret = self.tcb.rip;
                self.push_inline(Width::Qword, ret)?;
                self.tcb.rip = target;
                return Ok(Step::Retired);
            }
            Op::Ret => {
                let target = self.pop_inline(Width::Qword)?;
                if let Source::Immediate(extra) = quick.source
                    && extra != 0
                {
                    self.tcb
                        .set_stack_pointer(self.tcb.stack_pointer().wrapping_add(extra));
                }
                self.tcb.rip = target;
                return Ok(Step::Retired);
            }
            Op::Widen | Op::WidenSigned => {
                let value = self.quick_load(quick, quick.source, quick.source_width)?;
                let widened = match quick.op {
                    Op::Widen => value,
                    _ => quick.source_width.sign_extend(value),
                };
                return self.quick_store(quick, quick.destination, width, widened);
            }
            _ => {}
        }
        let left = self.quick_load(quick, quick.destination, width)?;
        let right = self.quick_load(quick, quick.source, width)?;
        let result = width.truncate(match quick.op {
            Op::Add => left.wrapping_add(right),
            Op::Sub | Op::Cmp => left.wrapping_sub(right),
            Op::Or => left | right,
            Op::Xor => left ^ right,
            // `and` and `test`, which differ only in the write-back.
            _ => left & right,
        });
        self.tcb.flags.record(quick.rule(), width, left, right, result);
        match quick.writes_back() {
            true => self.quick_store(quick, quick.destination, width, result),
            false => Ok(Step::Retired),
        }
    }

    /// The address a lowered memory operand computes, segment base included.
    ///
    /// The same arithmetic [`Self::unsegmented_address`] does, with the
    /// constant half already folded and the register lookups already
    /// resolved to slices — and the thirty-two bit wrap applied to the sum
    /// rather than to each term, which is where it happens.
    #[inline(always)]
    fn quick_address(&self, quick: &Quick) -> Result<u64, Trap> {
        let address = match quick.address {
            Address::Fixed(at) => at,
            Address::Computed { displacement, base, index, scale, narrow } => {
                let mut at = displacement;
                if let Some(base) = base {
                    at = at.wrapping_add(self.tcb.read_register(base));
                }
                if let Some(index) = index {
                    at = at.wrapping_add(self.tcb.read_register(index).wrapping_mul(u64::from(scale)));
                }
                match narrow {
                    true => at & 0xffff_ffff,
                    false => at,
                }
            }
        };
        Ok(match quick.segmented {
            true => address.wrapping_add(self.tcb.fs_base),
            false => address,
        })
    }

    #[inline(always)]
    fn quick_load(&mut self, quick: &Quick, from: Source, width: Width) -> Result<u64, Trap> {
        match from {
            Source::Register(slice) => Ok(self.tcb.read_register(slice)),
            Source::Immediate(value) => Ok(value),
            Source::Memory => {
                let at = self.quick_address(quick)?;
                Ok(self.space.load(at, width)?)
            }
            Source::Vector(_) => unreachable!("a vector operand is deferred to step"),
        }
    }

    #[inline(always)]
    fn quick_store(
        &mut self,
        quick: &Quick,
        into: Source,
        width: Width,
        value: u64,
    ) -> Result<Step, Trap> {
        match into {
            Source::Register(slice) => {
                self.tcb.write_register(slice, value);
                Ok(Step::Retired)
            }
            Source::Memory => {
                let at = self.quick_address(quick)?;
                self.space.store(at, width, value)?;
                Ok(Step::Retired)
            }
            // An immediate destination is not a shape the lowering makes.
            Source::Immediate(_) => unreachable!("an immediate is never a destination"),
            Source::Vector(_) => unreachable!("a vector operand is deferred to step"),
        }
    }

    /// Deliberately *not* `#[inline(always)]`, unlike its fast-path
    /// counterpart. Inlined into `step` — which is a match over the whole
    /// of `Mnemonic` — it takes `alu` from 2.59x to 1.49x: the function
    /// becomes one enormous wasm body and Cranelift, which cannot split it
    /// back up, allocates registers for the whole thing at once. Small hot
    /// helpers want forcing inline; this one wants the opposite.
    fn read(&mut self, instruction: &Instruction, operand: u32, width: Width) -> Result<u64, Trap> {
        match instruction.op_kind(operand) {
            OpKind::Register => Ok(self
                .tcb
                .read_register(Self::slice(instruction, instruction.op_register(operand))?)),
            OpKind::Memory => {
                let at = self.address(instruction)?;
                Ok(self.space.load(at, width)?)
            }
            OpKind::Immediate8
            | OpKind::Immediate8_2nd
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64 => Ok(width.truncate(instruction.immediate(operand))),
            _ => Err(Trap::Unsupported(Unsupported::at(
                instruction,
                Some("an operand kind with no read path"),
            ))),
        }
    }

    /// Writes an operand, and reports that the instruction retired.
    ///
    /// The [`Step`] in the return type is not decoration: the overwhelming
    /// majority of instruction arms end by writing their destination, and
    /// giving the write the retirement means those arms are one expression
    /// with no trailing `Ok(Step::Retired)` to forget.
    /// Not force-inlined, for the reason [`Self::read`] gives.
    fn write(
        &mut self,
        instruction: &Instruction,
        operand: u32,
        width: Width,
        value: u64,
    ) -> Result<Step, Trap> {
        match instruction.op_kind(operand) {
            OpKind::Register => {
                let slice = Self::slice(instruction, instruction.op_register(operand))?;
                self.tcb.write_register(slice, value);
                Ok(Step::Retired)
            }
            OpKind::Memory => {
                let at = self.address(instruction)?;
                self.space.store(at, width, value)?;
                Ok(Step::Retired)
            }
            _ => Err(Trap::Unsupported(Unsupported::at(
                instruction,
                Some("an operand kind with no write path"),
            ))),
        }
    }

    // ---- the stack -------------------------------------------------------

    /// The stack push, in the form the fast path wants: inlined.
    ///
    /// Split from [`Self::push`] because the two callers want opposite
    /// things. Forcing the *shared* function inline put a copy inside
    /// `step` — a match over the whole of `Mnemonic` — and took `calls`
    /// from 3.41x to 3.26x, because Cranelift cannot colour a body that
    /// size. Leaving it alone left it as a wasm call at 4.4% of a Django
    /// import, which is a call per prologue, epilogue, `call` and `ret`.
    ///
    /// So the body is inlined into the fast path and reached through a
    /// plain call from the general one, and neither caller pays for the
    /// other's needs.
    #[inline(always)]
    fn push_inline(&mut self, width: Width, value: u64) -> Result<Step, Trap> {
        let at = self
            .tcb
            .stack_pointer()
            .wrapping_sub(u64::from(width.bytes()));
        self.space.store(at, width, value)?;
        self.tcb.set_stack_pointer(at);
        Ok(Step::Retired)
    }

    /// The same, for the general path, which must not grow.
    fn push(&mut self, width: Width, value: u64) -> Result<Step, Trap> {
        self.push_inline(width, value)
    }

    /// The stack pop the fast path uses; see [`Self::push_inline`].
    #[inline(always)]
    fn pop_inline(&mut self, width: Width) -> Result<u64, Trap> {
        let at = self.tcb.stack_pointer();
        let value = self.space.load(at, width)?;
        self.tcb
            .set_stack_pointer(at.wrapping_add(u64::from(width.bytes())));
        Ok(value)
    }

    /// The same, for the general path.
    fn pop(&mut self, width: Width) -> Result<u64, Trap> {
        self.pop_inline(width)
    }

    // ---- one instruction -------------------------------------------------

    /// Retires one instruction.
    ///
    /// On return `rip` names whatever runs next. That is usually the
    /// following instruction; it is the branch target for a taken branch;
    /// and it is *this instruction again* for a `rep` that has iterations
    /// left, which is the architecture's own model of an interruptible
    /// string operation and the reason a signal or a preemption can land in
    /// the middle of a `rep movsb` exactly as it does on hardware.
    pub fn step(&mut self, instruction: &Instruction) -> Result<Step, Trap> {
        self.tcb.retired += 1;
        self.tcb.rip = instruction.next_ip();
        match instruction.mnemonic() {
            // ---- moves ----
            Mnemonic::Mov => {
                let width = Self::width(instruction, 0)?;
                let value = self.read(instruction, 1, width)?;
                self.write(instruction, 0, width, value)
            }
            Mnemonic::Movzx | Mnemonic::Movsx | Mnemonic::Movsxd => {
                let destination = Self::width(instruction, 0)?;
                let source = Self::width(instruction, 1)?;
                let value = self.read(instruction, 1, source)?;
                let widened = match instruction.mnemonic() {
                    Mnemonic::Movzx => value,
                    _ => source.sign_extend(value),
                };
                self.write(instruction, 0, destination, widened)
            }
            Mnemonic::Lea => {
                let width = Self::width(instruction, 0)?;
                // Pure arithmetic: no access, no flags, and no segment base.
                // gas says as much — "segment override on `lea' is
                // ineffectual" — while still emitting the prefix byte, so
                // honouring it here would add the thread pointer to an
                // address the hardware computes without it.
                let address = self.unsegmented_address(instruction)?;
                self.write(instruction, 0, width, width.truncate(address))
            }
            Mnemonic::Xchg => {
                let width = Self::width(instruction, 0)?;
                // Both reads before either write. With two register
                // operands naming overlapping slices — `xchg %al, %ah` —
                // writing between the reads loses a byte.
                let left = self.read(instruction, 0, width)?;
                let right = self.read(instruction, 1, width)?;
                self.write(instruction, 0, width, right)?;
                self.write(instruction, 1, width, left)
            }
            Mnemonic::Bswap => {
                let width = Self::width(instruction, 0)?;
                let value = self.read(instruction, 0, width)?;
                let swapped = match width {
                    Width::Dword => u64::from((value as u32).swap_bytes()),
                    Width::Qword => value.swap_bytes(),
                    // `bswap` on a 16-bit register is architecturally
                    // undefined and no compiler emits it.
                    _ => {
                        return Err(Trap::Unsupported(Unsupported::at(
                            instruction,
                            Some("`bswap` at a width the architecture leaves undefined"),
                        )));
                    }
                };
                self.write(instruction, 0, width, swapped)
            }
            Mnemonic::Cbw | Mnemonic::Cwde | Mnemonic::Cdqe => {
                let (from, to) = match instruction.mnemonic() {
                    Mnemonic::Cbw => (Width::Byte, Width::Word),
                    Mnemonic::Cwde => (Width::Word, Width::Dword),
                    _ => (Width::Dword, Width::Qword),
                };
                let value = self.tcb.read_register(Slice {
                    number: number::RAX as u8,
                    width: from,
                    high_byte: false,
                });
                self.tcb.write_register(
                    Slice {
                        number: number::RAX as u8,
                        width: to,
                        high_byte: false,
                    },
                    from.sign_extend(value),
                );
                Ok(Step::Retired)
            }
            Mnemonic::Cwd | Mnemonic::Cdq | Mnemonic::Cqo => {
                let width = match instruction.mnemonic() {
                    Mnemonic::Cwd => Width::Word,
                    Mnemonic::Cdq => Width::Dword,
                    _ => Width::Qword,
                };
                let value = self.tcb.read_register(Slice {
                    number: number::RAX as u8,
                    width,
                    high_byte: false,
                });
                let high = match value & width.sign_bit() != 0 {
                    true => width.mask(),
                    false => 0,
                };
                self.tcb.write_register(
                    Slice {
                        number: number::RDX as u8,
                        width,
                        high_byte: false,
                    },
                    high,
                );
                Ok(Step::Retired)
            }

            // ---- arithmetic and logic ----
            Mnemonic::Add => self.arithmetic(instruction, Rule::Add, true),
            Mnemonic::Sub => self.arithmetic(instruction, Rule::Sub, true),
            Mnemonic::Cmp => self.arithmetic(instruction, Rule::Sub, false),
            Mnemonic::And => self.arithmetic(instruction, Rule::Logic, true),
            Mnemonic::Or => self.arithmetic(instruction, Rule::Logic, true),
            Mnemonic::Xor => self.arithmetic(instruction, Rule::Logic, true),
            Mnemonic::Test => self.arithmetic(instruction, Rule::Logic, false),
            Mnemonic::Adc => self.carrying(instruction, Rule::AddCarry),
            Mnemonic::Sbb => self.carrying(instruction, Rule::SubBorrow),
            Mnemonic::Inc | Mnemonic::Dec => {
                let width = Self::width(instruction, 0)?;
                let value = self.read(instruction, 0, width)?;
                let (rule, result) = match instruction.mnemonic() {
                    Mnemonic::Inc => (Rule::Increment, value.wrapping_add(1)),
                    _ => (Rule::Decrement, value.wrapping_sub(1)),
                };
                let result = width.truncate(result);
                self.tcb.flags.record(rule, width, value, 1, result);
                self.write(instruction, 0, width, result)
            }
            Mnemonic::Neg => {
                let width = Self::width(instruction, 0)?;
                let value = self.read(instruction, 0, width)?;
                let result = width.truncate(0u64.wrapping_sub(value));
                // `neg` is `sub` from zero in every flag, carry included:
                // the borrow out of zero is exactly "the operand was not
                // zero", which the subtraction rule already says.
                self.tcb.flags.record(Rule::Sub, width, 0, value, result);
                self.write(instruction, 0, width, result)
            }
            Mnemonic::Not => {
                let width = Self::width(instruction, 0)?;
                let value = self.read(instruction, 0, width)?;
                self.write(instruction, 0, width, width.truncate(!value))
            }
            Mnemonic::Mul | Mnemonic::Imul => self.multiply(instruction),
            Mnemonic::Div | Mnemonic::Idiv => self.divide(instruction),

            // ---- shifts and rotates ----
            Mnemonic::Shl | Mnemonic::Sal | Mnemonic::Shr | Mnemonic::Sar => {
                self.shift(instruction)
            }
            Mnemonic::Rol | Mnemonic::Ror | Mnemonic::Rcl | Mnemonic::Rcr => {
                self.rotate(instruction)
            }
            Mnemonic::Shld | Mnemonic::Shrd => self.double_shift(instruction),

            // ---- bits ----
            Mnemonic::Bt | Mnemonic::Bts | Mnemonic::Btr | Mnemonic::Btc => {
                self.bit_test(instruction)
            }
            Mnemonic::Bsf | Mnemonic::Bsr => {
                let width = Self::width(instruction, 0)?;
                let value = self.read(instruction, 1, width)?;
                // Only the zero flag is defined; the other five are
                // architecturally undefined and left as they were.
                let status = self.tcb.flags.status() & !bit::ZERO;
                if value == 0 {
                    // With a zero source the destination is undefined too,
                    // and hardware leaves it alone.
                    self.tcb.flags.set_all(status | bit::ZERO);
                    return Ok(Step::Retired);
                }
                self.tcb.flags.set_all(status);
                let index = match instruction.mnemonic() {
                    Mnemonic::Bsf => value.trailing_zeros(),
                    _ => width.bits() - 1 - (value << (64 - width.bits())).leading_zeros(),
                };
                self.write(instruction, 0, width, u64::from(index))
            }
            Mnemonic::Tzcnt | Mnemonic::Lzcnt => {
                let width = Self::width(instruction, 0)?;
                let value = self.read(instruction, 1, width)?;
                let count = match instruction.mnemonic() {
                    Mnemonic::Tzcnt => match value {
                        0 => width.bits(),
                        _ => value.trailing_zeros(),
                    },
                    _ => match value {
                        0 => width.bits(),
                        _ => (value << (64 - width.bits())).leading_zeros(),
                    },
                };
                let mut status = 0;
                if value == 0 {
                    status |= bit::CARRY;
                }
                if count == 0 {
                    status |= bit::ZERO;
                }
                self.tcb.flags.set_all(status);
                self.write(instruction, 0, width, u64::from(count))
            }
            Mnemonic::Popcnt => {
                let width = Self::width(instruction, 0)?;
                let value = self.read(instruction, 1, width)?;
                let count = u64::from(value.count_ones());
                self.tcb
                    .flags
                    .set_all(if value == 0 { bit::ZERO } else { 0 });
                self.write(instruction, 0, width, count)
            }

            // ---- conditionals ----
            mnemonic if conditional_of(mnemonic).is_some() => {
                let (kind, condition) =
                    conditional_of(mnemonic).expect("guarded on being a conditional");
                self.conditional(instruction, kind, condition)
            }

            // ---- control transfer ----
            Mnemonic::Jmp => {
                let target = self.branch_target(instruction)?;
                self.tcb.rip = target;
                Ok(Step::Retired)
            }
            Mnemonic::Call => {
                let target = self.branch_target(instruction)?;
                let ret = instruction.next_ip();
                self.push(Width::Qword, ret)?;
                self.tcb.rip = target;
                Ok(Step::Retired)
            }
            Mnemonic::Ret => {
                let target = self.pop(Width::Qword)?;
                if instruction.op_count() == 1 {
                    let extra = instruction.immediate(0);
                    self.tcb
                        .set_stack_pointer(self.tcb.stack_pointer().wrapping_add(extra));
                }
                self.tcb.rip = target;
                Ok(Step::Retired)
            }
            Mnemonic::Leave => {
                // `mov %rbp, %rsp` then `pop %rbp`, which is how an
                // unoptimised frame is torn down.
                let frame = self.tcb.registers[number::RBP];
                self.tcb.set_stack_pointer(frame);
                let saved = self.pop(Width::Qword)?;
                self.tcb.registers[number::RBP] = saved;
                Ok(Step::Retired)
            }
            Mnemonic::Loop | Mnemonic::Loope | Mnemonic::Loopne => {
                let count = self.tcb.registers[number::RCX].wrapping_sub(1);
                self.tcb.registers[number::RCX] = count;
                let zero = self.tcb.flags.zero();
                let take = count != 0
                    && match instruction.mnemonic() {
                        Mnemonic::Loope => zero,
                        Mnemonic::Loopne => !zero,
                        _ => true,
                    };
                if take {
                    self.tcb.rip = instruction.near_branch64();
                }
                Ok(Step::Retired)
            }
            Mnemonic::Jrcxz | Mnemonic::Jecxz => {
                let width = match instruction.mnemonic() {
                    Mnemonic::Jrcxz => Width::Qword,
                    _ => Width::Dword,
                };
                if self.tcb.read_register(Slice {
                    number: number::RCX as u8,
                    width,
                    high_byte: false,
                }) == 0
                {
                    self.tcb.rip = instruction.near_branch64();
                }
                Ok(Step::Retired)
            }

            // ---- the stack ----
            Mnemonic::Push => {
                let width = match instruction.op0_kind() {
                    OpKind::Register => Self::width(instruction, 0)?,
                    OpKind::Memory => Self::width(instruction, 0)?,
                    // A pushed immediate is sign-extended to the stack's
                    // width, which in 64-bit mode is eight bytes.
                    _ => Width::Qword,
                };
                let value = self.read(instruction, 0, width)?;
                self.push(width, value)
            }
            Mnemonic::Pop => {
                let width = Self::width(instruction, 0)?;
                let value = self.pop(width)?;
                self.write(instruction, 0, width, value)
            }
            Mnemonic::Pushfq | Mnemonic::Pushf => {
                let width = match instruction.mnemonic() {
                    Mnemonic::Pushfq => Width::Qword,
                    _ => Width::Word,
                };
                let word = self.tcb.flags.materialized();
                self.push(width, word)
            }
            Mnemonic::Popfq | Mnemonic::Popf => {
                match instruction.mnemonic() {
                    Mnemonic::Popfq => {
                        let word = self.pop(Width::Qword)?;
                        self.tcb.flags.load(word);
                    }
                    // The 16-bit form replaces only the low half of the
                    // word and leaves the rest — which in userspace is only
                    // the overflow flag — alone.
                    _ => {
                        let low = self.pop(Width::Word)?;
                        let kept = self.tcb.flags.materialized() & !0xffff;
                        self.tcb.flags.load(kept | low);
                    }
                }
                Ok(Step::Retired)
            }

            // ---- flags ----
            Mnemonic::Clc => {
                self.tcb.flags.set_carry(false);
                Ok(Step::Retired)
            }
            Mnemonic::Stc => {
                self.tcb.flags.set_carry(true);
                Ok(Step::Retired)
            }
            Mnemonic::Cmc => {
                let carry = self.tcb.flags.carry();
                self.tcb.flags.set_carry(!carry);
                Ok(Step::Retired)
            }
            Mnemonic::Cld => {
                self.tcb.flags.set_direction(false);
                Ok(Step::Retired)
            }
            Mnemonic::Std => {
                self.tcb.flags.set_direction(true);
                Ok(Step::Retired)
            }
            Mnemonic::Lahf => {
                // The low byte of the flags word, reserved bit included.
                let low = self.tcb.flags.materialized() & 0xff;
                self.tcb.write_register(
                    Slice {
                        number: number::RAX as u8,
                        width: Width::Byte,
                        high_byte: true,
                    },
                    low,
                );
                Ok(Step::Retired)
            }
            Mnemonic::Sahf => {
                let ah = self.tcb.read_register(Slice {
                    number: number::RAX as u8,
                    width: Width::Byte,
                    high_byte: true,
                });
                let keep = self.tcb.flags.materialized() & !0xff;
                self.tcb.flags.load(keep | (ah & 0xd5) | bit::RESERVED_ONE);
                Ok(Step::Retired)
            }

            // ---- exchange-class read-modify-writes ----
            Mnemonic::Cmpxchg => {
                let width = Self::width(instruction, 0)?;
                let destination = self.read(instruction, 0, width)?;
                let source = self.read(instruction, 1, width)?;
                let accumulator = self.tcb.read_register(Slice {
                    number: number::RAX as u8,
                    width,
                    high_byte: false,
                });
                let result = width.truncate(accumulator.wrapping_sub(destination));
                self.tcb
                    .flags
                    .record(Rule::Sub, width, accumulator, destination, result);
                match accumulator == destination {
                    true => self.write(instruction, 0, width, source),
                    false => {
                        self.tcb.write_register(
                            Slice {
                                number: number::RAX as u8,
                                width,
                                high_byte: false,
                            },
                            destination,
                        );
                        // The destination is written back unchanged on
                        // hardware, which matters only for a `lock`
                        // prefix's memory ordering, not for the value.
                        Ok(Step::Retired)
                    }
                }
            }
            Mnemonic::Xadd => {
                let width = Self::width(instruction, 0)?;
                let destination = self.read(instruction, 0, width)?;
                let source = self.read(instruction, 1, width)?;
                let result = width.truncate(destination.wrapping_add(source));
                self.tcb
                    .flags
                    .record(Rule::Add, width, destination, source, result);
                self.write(instruction, 1, width, destination)?;
                self.write(instruction, 0, width, result)
            }

            // ---- string operations ----
            Mnemonic::Movsb | Mnemonic::Movsw | Mnemonic::Movsd | Mnemonic::Movsq
                if is_string(instruction) =>
            {
                self.string(instruction, StringOperation::Move)
            }
            Mnemonic::Stosb | Mnemonic::Stosw | Mnemonic::Stosd | Mnemonic::Stosq => {
                self.string(instruction, StringOperation::Store)
            }
            Mnemonic::Lodsb | Mnemonic::Lodsw | Mnemonic::Lodsd | Mnemonic::Lodsq => {
                self.string(instruction, StringOperation::Load)
            }
            Mnemonic::Scasb | Mnemonic::Scasw | Mnemonic::Scasd | Mnemonic::Scasq => {
                self.string(instruction, StringOperation::Scan)
            }
            Mnemonic::Cmpsb | Mnemonic::Cmpsw | Mnemonic::Cmpsd | Mnemonic::Cmpsq
                if is_string(instruction) =>
            {
                self.string(instruction, StringOperation::Compare)
            }

            // ---- the machine ----
            Mnemonic::Syscall => {
                // Faithful, where the transpiler's seam had to invent
                // zeros: the hardware puts the return address in `%rcx` and
                // the flags word in `%r11`, and so does this. It costs
                // nothing and it removes a documented divergence from the
                // syscall-trace diff, so it must not be "simplified" back.
                self.tcb.registers[number::RCX] = instruction.next_ip();
                self.tcb.registers[number::R11] = self.tcb.flags.materialized();
                Ok(Step::Syscall)
            }
            Mnemonic::Cpuid => {
                self.cpuid();
                Ok(Step::Retired)
            }
            Mnemonic::Rdtsc | Mnemonic::Rdtscp => {
                let stamp = self.timestamp();
                self.tcb.registers[number::RAX] = stamp & 0xffff_ffff;
                self.tcb.registers[number::RDX] = stamp >> 32;
                if instruction.mnemonic() == Mnemonic::Rdtscp {
                    // The processor identifier. One machine, so one value.
                    self.tcb.registers[number::RCX] = 0;
                }
                Ok(Step::Retired)
            }
            Mnemonic::Xgetbv => {
                // x87 and SSE state enabled, and nothing else — the same
                // machine `cpuid` reports.
                self.tcb.registers[number::RAX] = 0x3;
                self.tcb.registers[number::RCX] = 0;
                Ok(Step::Retired)
            }

            // ---- instructions that mean nothing here ----
            //
            // Not silently ignored: each of these genuinely has no effect on
            // an in-order interpreter with one thread of execution and no
            // cache hierarchy, and saying so once is better than a reader
            // wondering at each site.
            Mnemonic::Nop
            | Mnemonic::Pause
            | Mnemonic::Endbr64
            | Mnemonic::Endbr32
            | Mnemonic::Mfence
            | Mnemonic::Lfence
            | Mnemonic::Sfence
            | Mnemonic::Clflush
            | Mnemonic::Clflushopt
            | Mnemonic::Prefetch
            | Mnemonic::Prefetchw
            | Mnemonic::Prefetchnta
            | Mnemonic::Prefetcht0
            | Mnemonic::Prefetcht1
            | Mnemonic::Prefetcht2 => Ok(Step::Retired),

            // The control-flow-enforcement probes, which are how a libc
            // *asks* whether it has a shadow stack.
            //
            // `rdssp` reads the shadow stack pointer — and on a processor
            // where the feature is off it does nothing at all, leaving the
            // destination register untouched. That is not a convenience: it
            // is the documented encoding, chosen so that a binary built for
            // CET runs unmodified on a processor without it. glibc zeroes a
            // register, executes `rdsspq` into it, and reads a zero as "no
            // shadow stack" — so *leaving the register alone is the answer*,
            // and writing anything would be claiming a feature this machine
            // does not report through `cpuid` and does not have.
            //
            // `incssp` is the same bargain from the other side: with the
            // feature off there is no shadow stack to advance.
            Mnemonic::Rdsspd | Mnemonic::Rdsspq | Mnemonic::Incsspd | Mnemonic::Incsspq => {
                Ok(Step::Retired)
            }

            // ---- the ways a program stops ----
            Mnemonic::Ud0 | Mnemonic::Ud1 | Mnemonic::Ud2 => Err(Trap::Undefined {
                address: instruction.ip(),
            }),
            Mnemonic::Int3 => Err(Trap::Breakpoint {
                address: instruction.ip(),
            }),
            Mnemonic::Hlt => Err(Trap::Privileged {
                address: instruction.ip(),
            }),

            // Then the vector surface, then the x87, then the loud error.
            // The order is the translator's: an integer arm first, then
            // SSE, then the FPU, then a report naming the instruction —
            // never a silent approximation.
            _ => match self.vector_step(instruction)? {
                Some(step) => Ok(step),
                None => match self.x87_step(instruction)? {
                    Some(step) => Ok(step),
                    None => Err(Trap::Unsupported(Unsupported::at(instruction, None))),
                },
            },
        }
    }

    // ---- the arms ---------------------------------------------------------

    /// `destination op= source`, with flags. `writes_back` is false for
    /// `cmp` and `test`, which compute the flags and discard the result.
    fn arithmetic(
        &mut self,
        instruction: &Instruction,
        rule: Rule,
        writes_back: bool,
    ) -> Result<Step, Trap> {
        let width = Self::width(instruction, 0)?;
        let left = self.read(instruction, 0, width)?;
        let right = self.read(instruction, 1, width)?;
        let result = width.truncate(match (rule, instruction.mnemonic()) {
            (Rule::Add, _) => left.wrapping_add(right),
            (Rule::Sub, _) => left.wrapping_sub(right),
            (Rule::Logic, Mnemonic::Or) => left | right,
            (Rule::Logic, Mnemonic::Xor) => left ^ right,
            (Rule::Logic, _) => left & right,
            _ => unreachable!("arithmetic is called with three rules"),
        });
        self.tcb.flags.record(rule, width, left, right, result);
        match writes_back {
            true => self.write(instruction, 0, width, result),
            false => Ok(Step::Retired),
        }
    }

    /// `adc` and `sbb`: the same addition and subtraction with the carry
    /// folded in, and folded back out again. Compilers reach for these well
    /// outside multi-word arithmetic — `sbb %eax, %eax` is a branchless way
    /// to broadcast the carry.
    fn carrying(&mut self, instruction: &Instruction, rule: Rule) -> Result<Step, Trap> {
        let width = Self::width(instruction, 0)?;
        let left = self.read(instruction, 0, width)?;
        let right = self.read(instruction, 1, width)?;
        let carry = u64::from(self.tcb.flags.carry());
        let result = width.truncate(match rule {
            Rule::AddCarry => left.wrapping_add(right).wrapping_add(carry),
            _ => left.wrapping_sub(right).wrapping_sub(carry),
        });
        self.tcb
            .flags
            .record_with_carry(rule, width, left, right, result, carry == 1);
        self.write(instruction, 0, width, result)
    }

    fn multiply(&mut self, instruction: &Instruction) -> Result<Step, Trap> {
        let signed = instruction.mnemonic() == Mnemonic::Imul;
        let width = Self::width(instruction, 0)?;
        // The one-operand forms multiply the accumulator and write the
        // double-width product across `%rdx:%rax`; the two- and
        // three-operand `imul` forms keep only the low half.
        let wide = instruction.op_count() == 1;
        let (left, right) = match instruction.op_count() {
            1 => (
                self.tcb.read_register(Slice {
                    number: number::RAX as u8,
                    width,
                    high_byte: false,
                }),
                self.read(instruction, 0, width)?,
            ),
            2 => (
                self.read(instruction, 0, width)?,
                self.read(instruction, 1, width)?,
            ),
            _ => (
                self.read(instruction, 1, width)?,
                self.read(instruction, 2, width)?,
            ),
        };
        let product: u128 = match signed {
            true => ((width.sign_extend(left) as i64 as i128)
                * (width.sign_extend(right) as i64 as i128)) as u128,
            false => u128::from(left) * u128::from(right),
        };
        let low = width.truncate(product as u64);
        let high = match width {
            Width::Qword => (product >> 64) as u64,
            _ => width.truncate((product >> width.bits()) as u64),
        };
        // Carry and overflow both say the same thing — the product needed
        // more room than the low half — and every other status flag is
        // architecturally undefined here.
        let fits = match signed {
            true => width.sign_extend(low) as i64 as i128 == product as i128,
            false => high == 0,
        };
        let status = match fits {
            true => 0,
            false => bit::CARRY | bit::OVERFLOW,
        };
        self.tcb.flags.set_all(status);
        if wide {
            let accumulator = Slice {
                number: number::RAX as u8,
                width,
                high_byte: false,
            };
            match width {
                // The byte form is the odd one: the whole sixteen-bit
                // product lands in `%ax` rather than being split.
                Width::Byte => self.tcb.write_register(
                    Slice {
                        number: number::RAX as u8,
                        width: Width::Word,
                        high_byte: false,
                    },
                    product as u64 & 0xffff,
                ),
                _ => {
                    self.tcb.write_register(accumulator, low);
                    self.tcb.write_register(
                        Slice {
                            number: number::RDX as u8,
                            width,
                            high_byte: false,
                        },
                        high,
                    );
                }
            }
            return Ok(Step::Retired);
        }
        // Two- and three-operand `imul` both write operand zero; only the
        // source differs.
        self.write(instruction, 0, width, low)
    }

    fn divide(&mut self, instruction: &Instruction) -> Result<Step, Trap> {
        let signed = instruction.mnemonic() == Mnemonic::Idiv;
        let width = Self::width(instruction, 0)?;
        let divisor = self.read(instruction, 0, width)?;
        let error = Trap::DivideError {
            address: instruction.ip(),
        };
        if divisor == 0 {
            return Err(error);
        }
        // The byte form's dividend is `%ax` whole, and its remainder goes to
        // `%ah` rather than to the data register — the only width where the
        // shape differs.
        let (dividend, remainder_slice) = match width {
            Width::Byte => (
                u128::from(self.tcb.read_register(Slice {
                    number: number::RAX as u8,
                    width: Width::Word,
                    high_byte: false,
                })),
                Slice {
                    number: number::RAX as u8,
                    width: Width::Byte,
                    high_byte: true,
                },
            ),
            _ => {
                let low = self.tcb.read_register(Slice {
                    number: number::RAX as u8,
                    width,
                    high_byte: false,
                });
                let high = self.tcb.read_register(Slice {
                    number: number::RDX as u8,
                    width,
                    high_byte: false,
                });
                (
                    (u128::from(high) << width.bits()) | u128::from(low),
                    Slice {
                        number: number::RDX as u8,
                        width,
                        high_byte: false,
                    },
                )
            }
        };
        let dividend_bits = match width {
            Width::Byte => 16,
            other => other.bits() * 2,
        };
        let (quotient, remainder) = match signed {
            false => {
                let divisor = u128::from(divisor);
                (dividend / divisor, dividend % divisor)
            }
            true => {
                let shift = 128 - dividend_bits;
                let dividend = ((dividend << shift) as i128) >> shift;
                let divisor = width.sign_extend(divisor) as i64 as i128;
                // The one division that overflows: the most negative
                // dividend over minus one.
                let quotient = dividend.checked_div(divisor).ok_or(error.clone())?;
                (quotient as u128, (dividend % divisor) as u128)
            }
        };
        // A quotient that does not fit is `#DE`, exactly as a zero divisor
        // is — the same signal, from the same instruction, and a guest that
        // divides `i32::MIN` by `-1` expects it.
        let fits = match signed {
            true => {
                let quotient = quotient as i128;
                quotient >= -(1i128 << (width.bits() - 1)) && quotient < (1i128 << (width.bits() - 1))
            }
            false => quotient <= u128::from(width.mask()),
        };
        if !fits {
            return Err(error);
        }
        let quotient_slice = match width {
            Width::Byte => Slice {
                number: number::RAX as u8,
                width: Width::Byte,
                high_byte: false,
            },
            _ => Slice {
                number: number::RAX as u8,
                width,
                high_byte: false,
            },
        };
        self.tcb
            .write_register(quotient_slice, width.truncate(quotient as u64));
        self.tcb
            .write_register(remainder_slice, width.truncate(remainder as u64));
        Ok(Step::Retired)
    }

    fn shift(&mut self, instruction: &Instruction) -> Result<Step, Trap> {
        let width = Self::width(instruction, 0)?;
        let value = self.read(instruction, 0, width)?;
        let count = self.shift_count(instruction, width)?;
        if count == 0 {
            // A masked count of zero changes nothing at all, flags included.
            return Ok(Step::Retired);
        }
        let bits = u64::from(width.bits());
        let (result, carry) = match instruction.mnemonic() {
            Mnemonic::Shl | Mnemonic::Sal => (
                width.truncate(value.wrapping_shl(count as u32)),
                match count <= bits {
                    true => (value >> (bits - count)) & 1 == 1,
                    false => false,
                },
            ),
            Mnemonic::Shr => (
                match count < 64 {
                    true => value >> count,
                    false => 0,
                },
                match count <= bits {
                    true => (value >> (count - 1)) & 1 == 1,
                    false => false,
                },
            ),
            _ => {
                let signed = width.sign_extend(value) as i64;
                let shift = count.min(63) as u32;
                (
                    width.truncate((signed >> shift) as u64),
                    (signed >> (count - 1).min(63)) & 1 == 1,
                )
            }
        };
        // Overflow is architecturally defined only for a count of one, and
        // hardware computes it for every count anyway; matching what
        // hardware computes is what keeps a lockstep comparison quiet.
        let overflow = match instruction.mnemonic() {
            Mnemonic::Shl | Mnemonic::Sal => (result & width.sign_bit() != 0) != carry,
            Mnemonic::Shr => value & width.sign_bit() != 0,
            _ => false,
        };
        let mut status = 0;
        if carry {
            status |= bit::CARRY;
        }
        if overflow {
            status |= bit::OVERFLOW;
        }
        if result == 0 {
            status |= bit::ZERO;
        }
        if result & width.sign_bit() != 0 {
            status |= bit::SIGN;
        }
        if (result as u8).count_ones() % 2 == 0 {
            status |= bit::PARITY;
        }
        self.tcb.flags.set_all(status);
        self.write(instruction, 0, width, result)
    }

    fn rotate(&mut self, instruction: &Instruction) -> Result<Step, Trap> {
        let width = Self::width(instruction, 0)?;
        let value = self.read(instruction, 0, width)?;
        let count = self.shift_count(instruction, width)?;
        if count == 0 {
            return Ok(Step::Retired);
        }
        let bits = u64::from(width.bits());
        let sign = width.sign_bit();
        let mut carry = self.tcb.flags.carry();
        let result = match instruction.mnemonic() {
            Mnemonic::Rol => {
                let turns = count % bits;
                let rotated = width.truncate((value << turns) | (value >> ((bits - turns) % bits)));
                carry = rotated & 1 == 1;
                rotated
            }
            Mnemonic::Ror => {
                let turns = count % bits;
                let rotated = width.truncate((value >> turns) | (value << ((bits - turns) % bits)));
                carry = rotated & sign != 0;
                rotated
            }
            // Through the carry the modulus is one bit wider, and the
            // rotation is small enough that spelling it as a loop is both
            // obviously right and fast enough.
            Mnemonic::Rcl => {
                let mut value = value;
                for _ in 0..count % (bits + 1) {
                    let out = value & sign != 0;
                    value = width.truncate((value << 1) | u64::from(carry));
                    carry = out;
                }
                value
            }
            _ => {
                let mut value = value;
                for _ in 0..count % (bits + 1) {
                    let out = value & 1 == 1;
                    value = (value >> 1) | (u64::from(carry) * sign);
                    carry = out;
                }
                width.truncate(value)
            }
        };
        // Rotates touch the carry and the overflow and nothing else: sign,
        // zero, parity and adjust survive untouched, which is why this
        // cannot go through `set_all` alone.
        let mut status = self.tcb.flags.status() & !(bit::CARRY | bit::OVERFLOW);
        if carry {
            status |= bit::CARRY;
        }
        let overflow = match instruction.mnemonic() {
            Mnemonic::Rol | Mnemonic::Rcl => (result & sign != 0) != carry,
            _ => {
                let top = result & sign != 0;
                let next = result & (sign >> 1) != 0;
                top != next
            }
        };
        if overflow {
            status |= bit::OVERFLOW;
        }
        self.tcb.flags.set_all(status);
        self.write(instruction, 0, width, result)
    }

    fn double_shift(&mut self, instruction: &Instruction) -> Result<Step, Trap> {
        let width = Self::width(instruction, 0)?;
        let destination = self.read(instruction, 0, width)?;
        let source = self.read(instruction, 1, width)?;
        let count = match instruction.op_kind(2) {
            OpKind::Register => {
                self.tcb.read_register(Self::slice(
                    instruction,
                    instruction.op_register(2),
                )?) & self.count_mask(width)
            }
            _ => instruction.immediate(2) & self.count_mask(width),
        };
        if count == 0 {
            return Ok(Step::Retired);
        }
        let bits = u64::from(width.bits());
        if count > bits {
            // Architecturally undefined, and no compiler emits it.
            return Err(Trap::Unsupported(Unsupported::at(
                instruction,
                Some("a double shift by more than the operand width"),
            )));
        }
        let left = instruction.mnemonic() == Mnemonic::Shld;
        let (result, carry) = match left {
            true => (
                width.truncate((destination << count) | (source >> (bits - count))),
                (destination >> (bits - count)) & 1 == 1,
            ),
            false => (
                width.truncate((destination >> count) | (source << (bits - count))),
                (destination >> (count - 1)) & 1 == 1,
            ),
        };
        let mut status = 0;
        if carry {
            status |= bit::CARRY;
        }
        if (destination ^ result) & width.sign_bit() != 0 {
            status |= bit::OVERFLOW;
        }
        if result == 0 {
            status |= bit::ZERO;
        }
        if result & width.sign_bit() != 0 {
            status |= bit::SIGN;
        }
        if (result as u8).count_ones() % 2 == 0 {
            status |= bit::PARITY;
        }
        self.tcb.flags.set_all(status);
        self.write(instruction, 0, width, result)
    }

    fn count_mask(&self, width: Width) -> u64 {
        match width {
            Width::Qword => 0x3f,
            _ => 0x1f,
        }
    }

    fn shift_count(&mut self, instruction: &Instruction, width: Width) -> Result<u64, Trap> {
        let raw = match instruction.op_count() {
            1 => 1,
            _ => match instruction.op_kind(1) {
                OpKind::Register => self.tcb.read_register(Self::slice(
                    instruction,
                    instruction.op_register(1),
                )?),
                _ => instruction.immediate(1),
            },
        };
        Ok(raw & self.count_mask(width))
    }

    /// `bt`, `bts`, `btr` and `btc`.
    ///
    /// The bit offset is not simply masked when the destination is in
    /// memory: it is a *signed* bit index that may address outside the
    /// operand entirely, so the effective address moves by the offset
    /// divided by eight, rounded towards negative infinity. glibc uses that
    /// form, and truncating instead would read the wrong byte silently.
    fn bit_test(&mut self, instruction: &Instruction) -> Result<Step, Trap> {
        let width = Self::width(instruction, 0)?;
        let offset = self.read(instruction, 1, width)?;
        let (address, index) = match instruction.op0_kind() {
            OpKind::Memory if instruction.op1_kind() == OpKind::Register => {
                let offset = width.sign_extend(offset) as i64;
                let byte = offset.div_euclid(8);
                let bit = offset.rem_euclid(8) as u64;
                let base = self.address(instruction)?;
                (Some(base.wrapping_add(byte as u64)), bit)
            }
            OpKind::Memory => {
                let base = self.address(instruction)?;
                let index = offset % u64::from(width.bits());
                (Some(base), index)
            }
            _ => (None, offset % u64::from(width.bits())),
        };
        // The addressed form reads a single byte, which is what the
        // architecture says and what makes the offset arithmetic above mean
        // anything.
        let (value, access_width) = match address {
            Some(at) if instruction.op1_kind() == OpKind::Register => {
                (self.space.load(at, Width::Byte)?, Width::Byte)
            }
            Some(at) => (self.space.load(at, width)?, width),
            None => (self.read(instruction, 0, width)?, width),
        };
        let mask = 1u64 << index;
        let carry = value & mask != 0;
        let mut status = self.tcb.flags.status() & !bit::CARRY;
        if carry {
            status |= bit::CARRY;
        }
        self.tcb.flags.set_all(status);
        let updated = match instruction.mnemonic() {
            Mnemonic::Bt => return Ok(Step::Retired),
            Mnemonic::Bts => value | mask,
            Mnemonic::Btr => value & !mask,
            _ => value ^ mask,
        };
        match address {
            Some(at) => {
                self.space.store(at, access_width, updated)?;
                Ok(Step::Retired)
            }
            None => self.write(instruction, 0, width, updated),
        }
    }

    /// `jcc`, `setcc` and `cmovcc`, which share their sixteen conditions.
    fn conditional(
        &mut self,
        instruction: &Instruction,
        kind: Conditional,
        condition: Condition,
    ) -> Result<Step, Trap> {
        let holds = condition.holds(&self.tcb.flags);
        match kind {
            Conditional::Jump => {
                if holds {
                    self.tcb.rip = instruction.near_branch64();
                }
                Ok(Step::Retired)
            }
            Conditional::Set => self.write(instruction, 0, Width::Byte, u64::from(holds)),
            Conditional::Move => {
                let width = Self::width(instruction, 0)?;
                // The destination is written either way. A `cmovcc` with a
                // 32-bit destination clears the register's upper half
                // whether or not the move happens, because it is a 32-bit
                // write either way, and code has been seen to depend on it.
                let value = match holds {
                    true => self.read(instruction, 1, width)?,
                    false => self.read(instruction, 0, width)?,
                };
                self.write(instruction, 0, width, value)
            }
        }
    }

    /// Where a `jmp` or `call` goes.
    fn branch_target(&mut self, instruction: &Instruction) -> Result<u64, Trap> {
        match instruction.op0_kind() {
            OpKind::NearBranch64 => Ok(instruction.near_branch64()),
            OpKind::Register | OpKind::Memory => self.read(instruction, 0, Width::Qword),
            _ => Err(Trap::Unsupported(Unsupported::at(
                instruction,
                Some("a far branch"),
            ))),
        }
    }

    /// One iteration of a string operation.
    ///
    /// Repeated instructions retire one iteration at a time and leave `rip`
    /// where it is until the count runs out. That is not a shortcut — it is
    /// the architecture's model: a `rep movsb` is interruptible between
    /// iterations, and executing the whole run atomically would make a
    /// signal or a preemption impossible to deliver inside a `memcpy` of a
    /// gigabyte.
    fn string(&mut self, instruction: &Instruction, operation: StringOperation) -> Result<Step, Trap> {
        let width = Width::from_bytes(instruction.memory_size().size()).ok_or_else(|| {
            Trap::Unsupported(Unsupported::at(instruction, Some("a string element width")))
        })?;
        let repeated = instruction.has_rep_prefix()
            || instruction.has_repe_prefix()
            || instruction.has_repne_prefix();
        if repeated && self.tcb.registers[number::RCX] == 0 {
            return Ok(Step::Retired);
        }
        let step = match self.tcb.flags.direction() {
            true => 0u64.wrapping_sub(u64::from(width.bytes())),
            false => u64::from(width.bytes()),
        };
        let source = self.tcb.registers[number::RSI];
        let destination = self.tcb.registers[number::RDI];
        match operation {
            StringOperation::Move => {
                let value = self.space.load(source, width)?;
                self.space.store(destination, width, value)?;
                self.tcb.registers[number::RSI] = source.wrapping_add(step);
                self.tcb.registers[number::RDI] = destination.wrapping_add(step);
            }
            StringOperation::Store => {
                let value = self.tcb.read_register(Slice {
                    number: number::RAX as u8,
                    width,
                    high_byte: false,
                });
                self.space.store(destination, width, value)?;
                self.tcb.registers[number::RDI] = destination.wrapping_add(step);
            }
            StringOperation::Load => {
                let value = self.space.load(source, width)?;
                self.tcb.write_register(
                    Slice {
                        number: number::RAX as u8,
                        width,
                        high_byte: false,
                    },
                    value,
                );
                self.tcb.registers[number::RSI] = source.wrapping_add(step);
            }
            StringOperation::Scan => {
                let value = self.space.load(destination, width)?;
                let accumulator = self.tcb.read_register(Slice {
                    number: number::RAX as u8,
                    width,
                    high_byte: false,
                });
                let result = width.truncate(accumulator.wrapping_sub(value));
                self.tcb
                    .flags
                    .record(Rule::Sub, width, accumulator, value, result);
                self.tcb.registers[number::RDI] = destination.wrapping_add(step);
            }
            StringOperation::Compare => {
                let left = self.space.load(source, width)?;
                let right = self.space.load(destination, width)?;
                let result = width.truncate(left.wrapping_sub(right));
                self.tcb.flags.record(Rule::Sub, width, left, right, result);
                self.tcb.registers[number::RSI] = source.wrapping_add(step);
                self.tcb.registers[number::RDI] = destination.wrapping_add(step);
            }
        }
        if !repeated {
            return Ok(Step::Retired);
        }
        let remaining = self.tcb.registers[number::RCX].wrapping_sub(1);
        self.tcb.registers[number::RCX] = remaining;
        // `repe`/`repne` also stop on the comparison's answer, and only the
        // two comparing operations have one.
        let stop = match operation {
            StringOperation::Scan | StringOperation::Compare => {
                let zero = self.tcb.flags.zero();
                (instruction.has_repe_prefix() && !zero)
                    || (instruction.has_repne_prefix() && zero)
            }
            _ => false,
        };
        if remaining != 0 && !stop {
            // Not finished: `rip` stays on this instruction, so the block
            // executor runs it again and a preemption or a signal can land
            // between iterations.
            self.tcb.rip = instruction.ip();
        }
        Ok(Step::Retired)
    }

    /// The processor this machine reports itself to be.
    ///
    /// One machine, the same on every host, and the same one the transpiler
    /// reports: a baseline x86-64 with SSE2 and nothing later. Passing the
    /// host's answer through would make a container's behaviour depend on
    /// which machine baked or ran it, and would let a libc select a code
    /// path — an AVX2 `memcpy`, say — that nothing here implements.
    fn cpuid(&mut self) {
        let leaf = self.tcb.registers[0] as u32;
        let answer = CPUID
            .iter()
            .find(|(candidate, _)| *candidate == leaf)
            .map(|(_, answer)| *answer)
            // An unknown leaf answers zeros, which is what a processor does
            // for a leaf above the highest it supports.
            .unwrap_or([0; 4]);
        for (number, value) in [0u8, 3, 1, 2].into_iter().zip(answer) {
            self.tcb.write_register(
                Slice {
                    number,
                    width: Width::Dword,
                    high_byte: false,
                },
                u64::from(value),
            );
        }
    }

    /// What `rdtsc` answers; see [`crate::state::Tcb::timestamp`].
    fn timestamp(&self) -> u64 {
        self.tcb.timestamp()
    }
}

/// See [`crate::state::Tcb::timestamp`].
pub const TIMESTAMP_STEP: u64 = 1_000_000_007;

/// Which string operation an instruction is. The mnemonics are shared with
/// unrelated SSE instructions (`movsd` is both a string move and a scalar
/// double move), so the *operation* is decided once and passed in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StringOperation {
    Move,
    Store,
    Load,
    Scan,
    Compare,
}

/// Whether a `movsd`/`cmpsd` is the string instruction rather than the SSE
/// one that shares its mnemonic. The string form's operands are the implicit
/// `%rsi` and `%rdi`, which no SSE form uses.
fn is_string(instruction: &Instruction) -> bool {
    matches!(
        instruction.op0_kind(),
        OpKind::MemorySegRSI
            | OpKind::MemorySegESI
            | OpKind::MemorySegSI
            | OpKind::MemoryESRDI
            | OpKind::MemoryESEDI
            | OpKind::MemoryESDI
    ) || matches!(
        instruction.op1_kind(),
        OpKind::MemorySegRSI
            | OpKind::MemorySegESI
            | OpKind::MemorySegSI
            | OpKind::MemoryESRDI
            | OpKind::MemoryESEDI
            | OpKind::MemoryESDI
    )
}

/// Which of the three condition-carrying families an instruction is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conditional {
    /// `jcc`: branch if it holds.
    Jump,
    /// `setcc`: write the answer as a byte.
    Set,
    /// `cmovcc`: write the source if it holds, and the destination if it
    /// does not — a write either way.
    Move,
}

/// The family and condition an instruction names, if it names one.
///
/// One table for all forty-eight mnemonics, because the sixteen conditions
/// are the same sixteen in each family and writing them out three times is
/// three places for `Jae` and `Cmovae` to drift apart.
pub fn conditional_of(mnemonic: Mnemonic) -> Option<(Conditional, Condition)> {
    use Conditional::{Jump, Move, Set};
    use Mnemonic as M;
    Some(match mnemonic {
        M::Jo => (Jump, Condition::Overflow),
        M::Jno => (Jump, Condition::NoOverflow),
        M::Jb => (Jump, Condition::Below),
        M::Jae => (Jump, Condition::AboveOrEqual),
        M::Je => (Jump, Condition::Equal),
        M::Jne => (Jump, Condition::NotEqual),
        M::Jbe => (Jump, Condition::BelowOrEqual),
        M::Ja => (Jump, Condition::Above),
        M::Js => (Jump, Condition::Sign),
        M::Jns => (Jump, Condition::NoSign),
        M::Jp => (Jump, Condition::Parity),
        M::Jnp => (Jump, Condition::NoParity),
        M::Jl => (Jump, Condition::Less),
        M::Jge => (Jump, Condition::GreaterOrEqual),
        M::Jle => (Jump, Condition::LessOrEqual),
        M::Jg => (Jump, Condition::Greater),
        M::Seto => (Set, Condition::Overflow),
        M::Setno => (Set, Condition::NoOverflow),
        M::Setb => (Set, Condition::Below),
        M::Setae => (Set, Condition::AboveOrEqual),
        M::Sete => (Set, Condition::Equal),
        M::Setne => (Set, Condition::NotEqual),
        M::Setbe => (Set, Condition::BelowOrEqual),
        M::Seta => (Set, Condition::Above),
        M::Sets => (Set, Condition::Sign),
        M::Setns => (Set, Condition::NoSign),
        M::Setp => (Set, Condition::Parity),
        M::Setnp => (Set, Condition::NoParity),
        M::Setl => (Set, Condition::Less),
        M::Setge => (Set, Condition::GreaterOrEqual),
        M::Setle => (Set, Condition::LessOrEqual),
        M::Setg => (Set, Condition::Greater),
        M::Cmovo => (Move, Condition::Overflow),
        M::Cmovno => (Move, Condition::NoOverflow),
        M::Cmovb => (Move, Condition::Below),
        M::Cmovae => (Move, Condition::AboveOrEqual),
        M::Cmove => (Move, Condition::Equal),
        M::Cmovne => (Move, Condition::NotEqual),
        M::Cmovbe => (Move, Condition::BelowOrEqual),
        M::Cmova => (Move, Condition::Above),
        M::Cmovs => (Move, Condition::Sign),
        M::Cmovns => (Move, Condition::NoSign),
        M::Cmovp => (Move, Condition::Parity),
        M::Cmovnp => (Move, Condition::NoParity),
        M::Cmovl => (Move, Condition::Less),
        M::Cmovge => (Move, Condition::GreaterOrEqual),
        M::Cmovle => (Move, Condition::LessOrEqual),
        M::Cmovg => (Move, Condition::Greater),
        _ => return None,
    })
}

/// Leaf, then `eax`, `ebx`, `ecx`, `edx`. See [`Cpu::cpuid`].
const CPUID: &[(u32, [u32; 4])] = &[
    // Leaf 0: the highest leaf understood, and the vendor string in
    // `ebx:edx:ecx`. Stopping at 1 is what keeps a libc from asking about
    // AVX2 at all — leaf 7 is where that lives, and a processor that does
    // not reach it does not have it.
    (
        0,
        [
            1,
            u32::from_le_bytes(*b"Genu"),
            u32::from_le_bytes(*b"ntel"),
            u32::from_le_bytes(*b"ineI"),
        ],
    ),
    // Leaf 1: family 6, model 15, stepping 11 — a Core 2, which is the
    // oldest processor that runs everything this targets and the newest
    // that has nothing after SSE3. Deliberately absent from `ecx` are
    // SSSE3, SSE4, POPCNT, XSAVE, OSXSAVE and AVX.
    (
        1,
        [
            0x0000_06fb,
            0x0000_0800,
            0x0000_0001,
            (1 << 0)      // FPU
                | (1 << 4)  // TSC
                | (1 << 8)  // CMPXCHG8B
                | (1 << 15) // CMOV
                | (1 << 19) // CLFSH
                | (1 << 23) // MMX
                | (1 << 24) // FXSR
                | (1 << 25) // SSE
                | (1 << 26), // SSE2
        ],
    ),
    (0x8000_0000, [0x8000_0001, 0, 0, 0]),
    (0x8000_0001, [0, 0, 0, (1 << 20) | (1 << 29)]),
];

