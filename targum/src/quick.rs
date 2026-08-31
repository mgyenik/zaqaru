//! Pre-decoded instructions: everything [`crate::exec::Cpu::step`] would
//! otherwise re-derive on every execution, derived once when the block is
//! built.
//!
//! The block cache already decodes *bytes* once. What it did not do is stop
//! re-reading the decode. Executing `add %rax, %rbx` asks
//! [`iced_x86::Instruction`] for its operand kinds four times, its registers
//! three times, and maps each of those through [`Slice::of`] with an `Option`
//! and an error closure — then dispatches all of it through a match over a
//! seventeen-hundred-variant enum. Every one of those is a pure function of
//! the instruction, and a hot loop recomputes the lot on every iteration.
//!
//! So a block carries one of these beside each instruction, and the
//! interpreter reads fields instead of re-deriving them.
//!
//! **What cannot be lowered is not lowered.** [`Op::General`] means "ask the
//! old path", and the old path is unchanged. That is the whole safety
//! argument: this module never has to decide that something is an error,
//! because anything it does not fully understand it declines, and the
//! general path produces exactly the diagnosis it always did. A bug here
//! can make the engine slower. It cannot make it wrong about an instruction
//! this module refused to lower.
//!
//! The dispatch being *dense* matters as much as the fields being ready.
//! `step` is one enormous function whose match is sparse across the whole of
//! `Mnemonic`; this one is a handful of arms over a `u8`, which is a jump
//! table a compiler can do something with — and, under wasmtime, a function
//! small enough for Cranelift to allocate registers for, which the big one
//! is not.

use iced_x86::{Instruction, Mnemonic, OpKind, Register};

use crate::flags::Rule;
use crate::state::{Slice, Width};

/// What a lowered instruction does. Dense and small on purpose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// Not lowered. The general path owns it.
    General,
    Mov,
    /// The address itself, never a load — which is why it is not `Mov`.
    Lea,
    Add,
    Sub,
    /// `Sub` without the write-back.
    Cmp,
    And,
    Or,
    Xor,
    /// `And` without the write-back.
    Test,
    /// One operand each, and the stack pointer moves. In every prologue and
    /// every epilogue there is, which is why they are worth the second
    /// operand shape.
    Push,
    Pop,
}

/// Where a lowered operand's value comes from.
///
/// Only one memory operand is possible in an x86 instruction, so the
/// address form lives once on the [`Quick`] rather than in both operands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Register(Slice),
    /// Already truncated to the operand width, which is what the general
    /// path does at every use.
    Immediate(u64),
    Memory,
}

/// A memory operand's address, with everything constant already folded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Address {
    /// `%rip`-relative, or a bare displacement: known when the block was
    /// built and constant for as long as the block lives. iced has already
    /// folded the instruction's own length in.
    Fixed(u64),
    Computed {
        displacement: u64,
        base: Option<Slice>,
        index: Option<Slice>,
        scale: u64,
        /// A thirty-two bit address-size prefix: the *sum* wraps, which is
        /// why this is a property of the address and not of each term.
        narrow: bool,
    },
}

/// Stands in the two operand fields of [`Quick::GENERAL`], which nothing
/// reads: the op is checked first, and `General` never reaches the fast
/// path at all.
const PLACEHOLDER: Slice = Slice { number: 0, width: Width::Qword, high_byte: false };

/// One instruction, ready to run.
#[derive(Clone, Copy, Debug)]
pub struct Quick {
    pub op: Op,
    pub width: Width,
    pub destination: Source,
    pub source: Source,
    pub address: Address,
    /// `%fs`-relative. `%gs` is never lowered — the general path refuses it,
    /// loudly, and should go on being the only place that knows that.
    pub segmented: bool,
}

impl Quick {
    /// The "ask the old path" answer, which is also what every failed
    /// lowering below returns.
    pub const GENERAL: Quick = Quick {
        op: Op::General,
        width: Width::Qword,
        // Never read: `Op::General` is checked before any field is.
        destination: Source::Register(PLACEHOLDER),
        source: Source::Register(PLACEHOLDER),
        address: Address::Fixed(0),
        segmented: false,
    };

    /// Lowers an instruction, or declines.
    pub fn lower(instruction: &Instruction) -> Quick {
        let op = match instruction.mnemonic() {
            Mnemonic::Mov => Op::Mov,
            Mnemonic::Lea => Op::Lea,
            Mnemonic::Add => Op::Add,
            Mnemonic::Sub => Op::Sub,
            Mnemonic::Cmp => Op::Cmp,
            Mnemonic::And => Op::And,
            Mnemonic::Or => Op::Or,
            Mnemonic::Xor => Op::Xor,
            Mnemonic::Test => Op::Test,
            Mnemonic::Push => return lower_push(instruction),
            Mnemonic::Pop => return lower_pop(instruction),
            _ => return Quick::GENERAL,
        };
        // Exactly two operands, because every op above has two and a
        // lowering that guessed at a third would be lowering something else.
        if instruction.op_count() != 2 {
            return Quick::GENERAL;
        }
        // A lock prefix means the general path, which is where the one
        // address space's answer to atomicity is written down.
        if instruction.has_lock_prefix() || instruction.has_rep_prefix() {
            return Quick::GENERAL;
        }
        let Some(width) = width_of(instruction) else {
            return Quick::GENERAL;
        };
        let Some(destination) = source_of(instruction, 0, width) else {
            return Quick::GENERAL;
        };
        let Some(source) = source_of(instruction, 1, width) else {
            return Quick::GENERAL;
        };
        // `lea` computes an address and stores it; a register destination is
        // the only shape that means, and iced calls its source a memory
        // operand that is never loaded.
        if op == Op::Lea && !matches!((destination, source), (Source::Register(_), Source::Memory))
        {
            return Quick::GENERAL;
        }
        let wants_memory =
            destination == Source::Memory || source == Source::Memory;
        let mut address = Address::Fixed(0);
        let mut segmented = false;
        if wants_memory {
            let Some((form, fs)) = address_of(instruction) else {
                return Quick::GENERAL;
            };
            address = form;
            segmented = fs;
        }
        Quick { op, width, destination, source, address, segmented }
    }

    /// Whether the flags this op leaves behind are a logical rule's.
    pub fn rule(&self) -> Rule {
        match self.op {
            Op::Add => Rule::Add,
            Op::Sub | Op::Cmp => Rule::Sub,
            _ => Rule::Logic,
        }
    }

    /// Whether the result is stored back into the destination.
    pub fn writes_back(&self) -> bool {
        !matches!(self.op, Op::Cmp | Op::Test)
    }
}

/// `push`, whose one operand is a source and whose immediate form is
/// sign-extended to the stack's width rather than to its own.
fn lower_push(instruction: &Instruction) -> Quick {
    if instruction.op_count() != 1 || instruction.has_lock_prefix() {
        return Quick::GENERAL;
    }
    let width = match instruction.op_kind(0) {
        OpKind::Register | OpKind::Memory => match width_of(instruction) {
            Some(width) => width,
            None => return Quick::GENERAL,
        },
        _ => Width::Qword,
    };
    let Some(source) = source_of(instruction, 0, width) else {
        return Quick::GENERAL;
    };
    let (address, segmented) = match source == Source::Memory {
        true => match address_of(instruction) {
            Some(found) => found,
            None => return Quick::GENERAL,
        },
        false => (Address::Fixed(0), false),
    };
    Quick { op: Op::Push, width, destination: Source::Register(PLACEHOLDER), source, address, segmented }
}

/// `pop`, whose one operand is a destination.
fn lower_pop(instruction: &Instruction) -> Quick {
    if instruction.op_count() != 1 || instruction.has_lock_prefix() {
        return Quick::GENERAL;
    }
    let Some(width) = width_of(instruction) else {
        return Quick::GENERAL;
    };
    let Some(destination) = source_of(instruction, 0, width) else {
        return Quick::GENERAL;
    };
    let (address, segmented) = match destination == Source::Memory {
        true => match address_of(instruction) {
            Some(found) => found,
            None => return Quick::GENERAL,
        },
        false => (Address::Fixed(0), false),
    };
    Quick { op: Op::Pop, width, destination, source: Source::Register(PLACEHOLDER), address, segmented }
}

/// Operand zero's width, which is the instruction's — the same rule
/// `Cpu::width` follows, including an immediate taking operand zero's.
fn width_of(instruction: &Instruction) -> Option<Width> {
    let bytes = match instruction.op_kind(0) {
        OpKind::Register => instruction.op_register(0).size(),
        OpKind::Memory => instruction.memory_size().size(),
        _ => return None,
    };
    Width::from_bytes(bytes)
}

fn source_of(instruction: &Instruction, operand: u32, width: Width) -> Option<Source> {
    match instruction.op_kind(operand) {
        OpKind::Register => Slice::of(instruction.op_register(operand)).map(Source::Register),
        OpKind::Memory => Some(Source::Memory),
        OpKind::Immediate8
        | OpKind::Immediate8_2nd
        | OpKind::Immediate16
        | OpKind::Immediate32
        | OpKind::Immediate64
        | OpKind::Immediate8to16
        | OpKind::Immediate8to32
        | OpKind::Immediate8to64
        | OpKind::Immediate32to64 => {
            Some(Source::Immediate(width.truncate(instruction.immediate(operand))))
        }
        _ => None,
    }
}

/// The address form, and whether it is `%fs`-relative.
///
/// Every refusal here mirrors one the general path makes, and mirrors it by
/// declining rather than by reproducing the diagnosis: a base that is
/// neither four nor eight bytes, or a `%gs` prefix, goes back to the code
/// that has always explained itself about them.
fn address_of(instruction: &Instruction) -> Option<(Address, bool)> {
    let segmented = match instruction.segment_prefix() {
        Register::FS => true,
        Register::None | Register::CS | Register::DS | Register::ES | Register::SS => false,
        _ => return None,
    };
    let base = instruction.memory_base();
    let index = instruction.memory_index();
    if matches!(base, Register::RIP | Register::EIP) {
        return Some((Address::Fixed(instruction.memory_displacement64()), segmented));
    }
    let narrow = base.size() == 4 || (index != Register::None && index.size() == 4);
    let lowered = |register: Register| -> Option<Option<Slice>> {
        if register == Register::None {
            return Some(None);
        }
        if register.size() != 8 && !narrow {
            return None;
        }
        Slice::of(register).map(Some)
    };
    Some((
        Address::Computed {
            displacement: instruction.memory_displacement64(),
            base: lowered(base)?,
            index: lowered(index)?,
            scale: u64::from(instruction.memory_index_scale()),
            narrow,
        },
        segmented,
    ))
}
