//! The x87 stack, lowered to calls into the `x87` crate.
//!
//! Every other instruction family is translated into wasm here. This one is
//! not, and the reason is arithmetic: the x87 registers are eighty bits
//! wide, wasm has no such type, and a faithful `fmul` is an
//! extended-precision multiply over a sixty-four bit significand. That is a
//! softfloat library, and it lives in the `x87` crate where it can be unit
//! tested against the host's own FPU rather than only through a translated
//! module.
//!
//! So this module is a *lowering* and not an implementation: it decides
//! which helper an instruction means, computes its arguments with the
//! ordinary operand machinery, and calls. Three rules make that cheap, and
//! all three are load-bearing:
//!
//! 1. **A helper call is a bare call.** No flush, no reload, no
//!    return-address slot, no resume site. The helpers cannot name the
//!    register-file globals, so promoted machine state stays valid across
//!    one; they cannot block or throw, so `--resume` never has to know they
//!    happened.
//! 2. **Addressing stays here.** Memory operands are read and written with
//!    [`FunctionTranslator::read_operand`] and `write_operand` and passed by
//!    *value*. Only the ten-byte format and the environment images pass an
//!    address, because those are layouts the crate owns.
//! 3. **Registers and flags go through the state.** `fnstsw ax` writes
//!    through `write_register` at word width — the hardware leaves the rest
//!    of `%rax` alone — and `fcomi` writes flags through `write_flag`.

use anyhow::{Result, bail};
use iced_x86::{Mnemonic, OpKind, Register};

use crate::emitter::code::FunctionBodyBuilder;
use crate::emitter::{FunctionType, ValueType};
use crate::lifter::LiftedInstruction;
use crate::machine::{OperandWidth, RegisterSlice, STACK_ALIGNMENT};

use super::FunctionTranslator;

/// Whether an instruction was one of ours.
pub(super) enum X87Outcome {
    Translated,
    NotAnX87Instruction,
}

/// Which arithmetic a binary helper performs.
///
/// The numbering is `x87::ffi::binary_op`'s, mirrored rather than
/// re-derived: the two sides meet across a `u32` argument that carries no
/// type, so the only thing keeping them together is that this list says so.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Binary {
    Add = 0,
    Subtract = 1,
    SubtractReversed = 2,
    Multiply = 3,
    Divide = 4,
    DivideReversed = 5,
}

/// One helper the `x87` crate exports.
///
/// The signatures restate `x87/src/ffi.rs`. A disagreement between the two
/// is a link error rather than a mystery at run time, which is the whole
/// reason the imports are typed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum X87Helper {
    Fld32,
    Fld64,
    Fld80,
    FldSti,
    FldConst,
    Fild16,
    Fild32,
    Fild64,
    Fst32,
    Fst64,
    Fstp80,
    FstSti,
    Fist16,
    Fist32,
    Fist64,
    Fisttp16,
    Fisttp32,
    Fisttp64,
    ArithSti,
    Arith32,
    Arith64,
    ArithI16,
    ArithI32,
    Fchs,
    Fabs,
    Fsqrt,
    Frndint,
    Fprem,
    Fprem1,
    Fscale,
    Fxtract,
    F2xm1,
    Fyl2x,
    Fyl2xp1,
    Fpatan,
    Fxch,
    Ffree,
    Fincstp,
    Fdecstp,
    Fcmov,
    FcomSti,
    Fcom32,
    Fcom64,
    Ficom16,
    Ficom32,
    Ftst,
    Fxam,
    Fcomi,
    Fnstcw,
    Fldcw,
    Fnstsw,
    Fnclex,
    Finit,
    Fwait,
    Fnstenv,
    Fldenv,
    Fnsave,
    Frstor,
    Reset,
}

impl X87Helper {
    /// Every helper, so the declaration can walk them without a second list
    /// to keep in step.
    pub const ALL: [X87Helper; 59] = [
        X87Helper::Fld32,
        X87Helper::Fld64,
        X87Helper::Fld80,
        X87Helper::FldSti,
        X87Helper::FldConst,
        X87Helper::Fild16,
        X87Helper::Fild32,
        X87Helper::Fild64,
        X87Helper::Fst32,
        X87Helper::Fst64,
        X87Helper::Fstp80,
        X87Helper::FstSti,
        X87Helper::Fist16,
        X87Helper::Fist32,
        X87Helper::Fist64,
        X87Helper::Fisttp16,
        X87Helper::Fisttp32,
        X87Helper::Fisttp64,
        X87Helper::ArithSti,
        X87Helper::Arith32,
        X87Helper::Arith64,
        X87Helper::ArithI16,
        X87Helper::ArithI32,
        X87Helper::Fchs,
        X87Helper::Fabs,
        X87Helper::Fsqrt,
        X87Helper::Frndint,
        X87Helper::Fprem,
        X87Helper::Fprem1,
        X87Helper::Fscale,
        X87Helper::Fxtract,
        X87Helper::F2xm1,
        X87Helper::Fyl2x,
        X87Helper::Fyl2xp1,
        X87Helper::Fpatan,
        X87Helper::Fxch,
        X87Helper::Ffree,
        X87Helper::Fincstp,
        X87Helper::Fdecstp,
        X87Helper::Fcmov,
        X87Helper::FcomSti,
        X87Helper::Fcom32,
        X87Helper::Fcom64,
        X87Helper::Ficom16,
        X87Helper::Ficom32,
        X87Helper::Ftst,
        X87Helper::Fxam,
        X87Helper::Fcomi,
        X87Helper::Fnstcw,
        X87Helper::Fldcw,
        X87Helper::Fnstsw,
        X87Helper::Fnclex,
        X87Helper::Finit,
        X87Helper::Fwait,
        X87Helper::Fnstenv,
        X87Helper::Fldenv,
        X87Helper::Fnsave,
        X87Helper::Frstor,
        X87Helper::Reset,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn symbol_name(self) -> &'static str {
        match self {
            X87Helper::Fld32 => "x87_fld32",
            X87Helper::Fld64 => "x87_fld64",
            X87Helper::Fld80 => "x87_fld80",
            X87Helper::FldSti => "x87_fld_sti",
            X87Helper::FldConst => "x87_fld_const",
            X87Helper::Fild16 => "x87_fild16",
            X87Helper::Fild32 => "x87_fild32",
            X87Helper::Fild64 => "x87_fild64",
            X87Helper::Fst32 => "x87_fst32",
            X87Helper::Fst64 => "x87_fst64",
            X87Helper::Fstp80 => "x87_fstp80",
            X87Helper::FstSti => "x87_fst_sti",
            X87Helper::Fist16 => "x87_fist16",
            X87Helper::Fist32 => "x87_fist32",
            X87Helper::Fist64 => "x87_fist64",
            X87Helper::Fisttp16 => "x87_fisttp16",
            X87Helper::Fisttp32 => "x87_fisttp32",
            X87Helper::Fisttp64 => "x87_fisttp64",
            X87Helper::ArithSti => "x87_arith_sti",
            X87Helper::Arith32 => "x87_arith32",
            X87Helper::Arith64 => "x87_arith64",
            X87Helper::ArithI16 => "x87_arith_i16",
            X87Helper::ArithI32 => "x87_arith_i32",
            X87Helper::Fchs => "x87_fchs",
            X87Helper::Fabs => "x87_fabs",
            X87Helper::Fsqrt => "x87_fsqrt",
            X87Helper::Frndint => "x87_frndint",
            X87Helper::Fprem => "x87_fprem",
            X87Helper::Fprem1 => "x87_fprem1",
            X87Helper::Fscale => "x87_fscale",
            X87Helper::Fxtract => "x87_fxtract",
            X87Helper::F2xm1 => "x87_f2xm1",
            X87Helper::Fyl2x => "x87_fyl2x",
            X87Helper::Fyl2xp1 => "x87_fyl2xp1",
            X87Helper::Fpatan => "x87_fpatan",
            X87Helper::Fxch => "x87_fxch",
            X87Helper::Ffree => "x87_ffree",
            X87Helper::Fincstp => "x87_fincstp",
            X87Helper::Fdecstp => "x87_fdecstp",
            X87Helper::Fcmov => "x87_fcmov",
            X87Helper::FcomSti => "x87_fcom_sti",
            X87Helper::Fcom32 => "x87_fcom32",
            X87Helper::Fcom64 => "x87_fcom64",
            X87Helper::Ficom16 => "x87_ficom16",
            X87Helper::Ficom32 => "x87_ficom32",
            X87Helper::Ftst => "x87_ftst",
            X87Helper::Fxam => "x87_fxam",
            X87Helper::Fcomi => "x87_fcomi",
            X87Helper::Fnstcw => "x87_fnstcw",
            X87Helper::Fldcw => "x87_fldcw",
            X87Helper::Fnstsw => "x87_fnstsw",
            X87Helper::Fnclex => "x87_fnclex",
            X87Helper::Finit => "x87_finit",
            X87Helper::Fwait => "x87_fwait",
            X87Helper::Fnstenv => "x87_fnstenv",
            X87Helper::Fldenv => "x87_fldenv",
            X87Helper::Fnsave => "x87_fnsave",
            X87Helper::Frstor => "x87_frstor",
            X87Helper::Reset => "x87_reset",
        }
    }

    pub fn signature(self) -> FunctionType {
        use ValueType::{I32, I64};
        let (parameters, results): (&[ValueType], &[ValueType]) = match self {
            // Values arrive by value; only the ten-byte format and the
            // environment images pass an address, because those are layouts
            // the crate owns rather than numbers the translator can make.
            X87Helper::Fld32 | X87Helper::Fild16 | X87Helper::Fild32 => (&[I32], &[]),
            X87Helper::Fld64 | X87Helper::Fild64 => (&[I64], &[]),
            X87Helper::Fld80
            | X87Helper::Fstp80
            | X87Helper::FldSti
            | X87Helper::FldConst
            | X87Helper::Fxch
            | X87Helper::Fincstp
            | X87Helper::Fdecstp
            | X87Helper::Fldcw
            | X87Helper::Fnstenv
            | X87Helper::Fldenv
            | X87Helper::Fnsave
            | X87Helper::Frstor => match self {
                X87Helper::Fincstp | X87Helper::Fdecstp => (&[], &[]),
                _ => (&[I32], &[]),
            },
            X87Helper::Fst32 | X87Helper::Fist16 | X87Helper::Fist32 => (&[I32], &[I32]),
            X87Helper::Fst64 | X87Helper::Fist64 => (&[I32], &[I64]),
            X87Helper::Fisttp16 | X87Helper::Fisttp32 => (&[], &[I32]),
            X87Helper::Fisttp64 => (&[], &[I64]),
            X87Helper::FstSti | X87Helper::Ffree | X87Helper::Fcmov => (&[I32, I32], &[]),
            X87Helper::ArithSti => (&[I32, I32, I32, I32], &[]),
            X87Helper::Arith32 | X87Helper::ArithI16 | X87Helper::ArithI32 => (&[I32, I32], &[]),
            X87Helper::Arith64 => (&[I32, I64], &[]),
            X87Helper::FcomSti => (&[I32, I32, I32], &[]),
            X87Helper::Fcom32 | X87Helper::Ficom16 | X87Helper::Ficom32 => (&[I32, I32], &[]),
            X87Helper::Fcom64 => (&[I64, I32], &[]),
            X87Helper::Fcomi => (&[I32, I32, I32], &[I32]),
            X87Helper::Fnstcw | X87Helper::Fnstsw => (&[], &[I32]),
            X87Helper::Fchs
            | X87Helper::Fabs
            | X87Helper::Fsqrt
            | X87Helper::Frndint
            | X87Helper::Fprem
            | X87Helper::Fprem1
            | X87Helper::Fscale
            | X87Helper::Fxtract
            | X87Helper::F2xm1
            | X87Helper::Fyl2x
            | X87Helper::Fyl2xp1
            | X87Helper::Fpatan
            | X87Helper::Ftst
            | X87Helper::Fxam
            | X87Helper::Fnclex
            | X87Helper::Finit
            | X87Helper::Fwait
            | X87Helper::Reset => (&[], &[]),
        };
        FunctionType {
            parameters: parameters.to_vec(),
            results: results.to_vec(),
        }
    }
}

/// Whether an instruction belongs to this module.
///
/// Shared with the reference scan, so that what declares the imports and
/// what consumes them can never disagree about which instructions are x87.
pub fn is_x87_mnemonic(mnemonic: Mnemonic) -> bool {
    helper_shape(mnemonic).is_some()
}

/// The families this module lowers, as a coarse shape the scan can ask about
/// without knowing operand widths.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Load,
    Constant,
    IntegerLoad,
    Store,
    IntegerStore,
    TruncatingStore,
    Arithmetic(Binary),
    IntegerArithmetic(Binary),
    Nullary(X87Helper),
    Exchange,
    Free,
    ConditionalMove,
    Compare,
    IntegerCompare,
    CompareToFlags,
    StatusWord,
    ControlWord,
    LoadControlWord,
    Environment(X87Helper),
    Nothing,
}

fn helper_shape(mnemonic: Mnemonic) -> Option<Shape> {
    use Binary::*;
    Some(match mnemonic {
        Mnemonic::Fld => Shape::Load,
        Mnemonic::Fld1
        | Mnemonic::Fldl2t
        | Mnemonic::Fldl2e
        | Mnemonic::Fldpi
        | Mnemonic::Fldlg2
        | Mnemonic::Fldln2
        | Mnemonic::Fldz => Shape::Constant,
        Mnemonic::Fild => Shape::IntegerLoad,
        Mnemonic::Fst | Mnemonic::Fstp => Shape::Store,
        Mnemonic::Fist | Mnemonic::Fistp => Shape::IntegerStore,
        Mnemonic::Fisttp => Shape::TruncatingStore,
        Mnemonic::Fadd | Mnemonic::Faddp => Shape::Arithmetic(Add),
        Mnemonic::Fsub | Mnemonic::Fsubp => Shape::Arithmetic(Subtract),
        Mnemonic::Fsubr | Mnemonic::Fsubrp => Shape::Arithmetic(SubtractReversed),
        Mnemonic::Fmul | Mnemonic::Fmulp => Shape::Arithmetic(Multiply),
        Mnemonic::Fdiv | Mnemonic::Fdivp => Shape::Arithmetic(Divide),
        Mnemonic::Fdivr | Mnemonic::Fdivrp => Shape::Arithmetic(DivideReversed),
        Mnemonic::Fiadd => Shape::IntegerArithmetic(Add),
        Mnemonic::Fisub => Shape::IntegerArithmetic(Subtract),
        Mnemonic::Fisubr => Shape::IntegerArithmetic(SubtractReversed),
        Mnemonic::Fimul => Shape::IntegerArithmetic(Multiply),
        Mnemonic::Fidiv => Shape::IntegerArithmetic(Divide),
        Mnemonic::Fidivr => Shape::IntegerArithmetic(DivideReversed),
        Mnemonic::Fchs => Shape::Nullary(X87Helper::Fchs),
        Mnemonic::Fabs => Shape::Nullary(X87Helper::Fabs),
        Mnemonic::Fsqrt => Shape::Nullary(X87Helper::Fsqrt),
        Mnemonic::Frndint => Shape::Nullary(X87Helper::Frndint),
        Mnemonic::Fprem => Shape::Nullary(X87Helper::Fprem),
        Mnemonic::Fprem1 => Shape::Nullary(X87Helper::Fprem1),
        Mnemonic::Fscale => Shape::Nullary(X87Helper::Fscale),
        Mnemonic::Fxtract => Shape::Nullary(X87Helper::Fxtract),
        Mnemonic::F2xm1 => Shape::Nullary(X87Helper::F2xm1),
        Mnemonic::Fyl2x => Shape::Nullary(X87Helper::Fyl2x),
        Mnemonic::Fyl2xp1 => Shape::Nullary(X87Helper::Fyl2xp1),
        Mnemonic::Fpatan => Shape::Nullary(X87Helper::Fpatan),
        Mnemonic::Fincstp => Shape::Nullary(X87Helper::Fincstp),
        Mnemonic::Fdecstp => Shape::Nullary(X87Helper::Fdecstp),
        Mnemonic::Ftst => Shape::Nullary(X87Helper::Ftst),
        Mnemonic::Fxam => Shape::Nullary(X87Helper::Fxam),
        Mnemonic::Fnclex => Shape::Nullary(X87Helper::Fnclex),
        Mnemonic::Fninit => Shape::Nullary(X87Helper::Finit),
        Mnemonic::Wait => Shape::Nullary(X87Helper::Fwait),
        Mnemonic::Fxch => Shape::Exchange,
        Mnemonic::Ffree | Mnemonic::Ffreep => Shape::Free,
        Mnemonic::Fcmovb
        | Mnemonic::Fcmove
        | Mnemonic::Fcmovbe
        | Mnemonic::Fcmovu
        | Mnemonic::Fcmovnb
        | Mnemonic::Fcmovne
        | Mnemonic::Fcmovnbe
        | Mnemonic::Fcmovnu => Shape::ConditionalMove,
        Mnemonic::Fcom | Mnemonic::Fcomp | Mnemonic::Fcompp => Shape::Compare,
        Mnemonic::Fucom | Mnemonic::Fucomp | Mnemonic::Fucompp => Shape::Compare,
        Mnemonic::Ficom | Mnemonic::Ficomp => Shape::IntegerCompare,
        Mnemonic::Fcomi | Mnemonic::Fcomip | Mnemonic::Fucomi | Mnemonic::Fucomip => {
            Shape::CompareToFlags
        }
        Mnemonic::Fnstsw => Shape::StatusWord,
        Mnemonic::Fnstcw => Shape::ControlWord,
        Mnemonic::Fldcw => Shape::LoadControlWord,
        Mnemonic::Fnstenv => Shape::Environment(X87Helper::Fnstenv),
        Mnemonic::Fldenv => Shape::Environment(X87Helper::Fldenv),
        Mnemonic::Fnsave => Shape::Environment(X87Helper::Fnsave),
        Mnemonic::Frstor => Shape::Environment(X87Helper::Frstor),
        Mnemonic::Fnop => Shape::Nothing,
        _ => return None,
    })
}

impl FunctionTranslator<'_> {
    /// Lowers one x87 instruction, or reports that it was not one.
    pub(super) fn translate_x87(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<X87Outcome> {
        let Some(shape) = helper_shape(lifted.instruction.mnemonic()) else {
            return Ok(X87Outcome::NotAnX87Instruction);
        };
        match shape {
            Shape::Nothing => {}
            Shape::Load => self.x87_load(body, lifted)?,
            Shape::Constant => self.x87_constant(body, lifted)?,
            Shape::IntegerLoad => self.x87_integer_load(body, lifted)?,
            Shape::Store => self.x87_store(body, lifted)?,
            Shape::IntegerStore => self.x87_integer_store(body, lifted, false)?,
            Shape::TruncatingStore => self.x87_integer_store(body, lifted, true)?,
            Shape::Arithmetic(op) => self.x87_arithmetic(body, lifted, op)?,
            Shape::IntegerArithmetic(op) => self.x87_integer_arithmetic(body, lifted, op)?,
            Shape::Nullary(helper) => self.call_x87(body, helper)?,
            Shape::Exchange => {
                // `fxch` with no operand exchanges with `st(1)`, which is
                // the form every compiler emits.
                let index = self.x87_stack_operand(lifted, 1);
                body.i32_const(index as i32);
                self.call_x87(body, X87Helper::Fxch)?;
            }
            Shape::Free => {
                let index = self.x87_stack_operand(lifted, 0);
                body.i32_const(index as i32);
                body.i32_const(i32::from(lifted.instruction.mnemonic() == Mnemonic::Ffreep));
                self.call_x87(body, X87Helper::Ffree)?;
            }
            Shape::ConditionalMove => self.x87_conditional_move(body, lifted)?,
            Shape::Compare => self.x87_compare(body, lifted)?,
            Shape::IntegerCompare => self.x87_integer_compare(body, lifted)?,
            Shape::CompareToFlags => self.x87_compare_to_flags(body, lifted)?,
            Shape::StatusWord => self.x87_read_word(body, lifted, X87Helper::Fnstsw)?,
            Shape::ControlWord => self.x87_read_word(body, lifted, X87Helper::Fnstcw)?,
            Shape::LoadControlWord => {
                self.read_operand(body, lifted, 0, OperandWidth::Word)?;
                self.call_x87(body, X87Helper::Fldcw)?;
            }
            Shape::Environment(helper) => {
                // The crate owns these layouts, so it gets an address rather
                // than a value.
                self.emit_effective_address(body, lifted)?;
                self.call_x87(body, helper)?;
            }
        }
        Ok(X87Outcome::Translated)
    }

    /// A helper call, and nothing else.
    ///
    /// Deliberately not `emit_transfer`: a helper is not guest code. It
    /// cannot name the register-file globals, so promoted state survives it
    /// untouched; it cannot block or throw, so it reserves no return-address
    /// slot and is not a resume site. Going through the call machinery would
    /// add all three for nothing.
    /// Calls one helper, on a stack that is not the guest's.
    ///
    /// The guest's stack pointer stays where it is — an x87 instruction does
    /// not move `%rsp`, and nothing downstream may see that it did. What
    /// moves is the linker's, because the helper is an ordinary wasm callee
    /// and allocates its frame from there.
    ///
    /// This is the kernel seam's rule, not the interop thunk's, and the
    /// distinction is the one SysV draws: a foreign *call* is allowed to eat
    /// the caller's red zone, so a thunk hands the callee the guest's own
    /// stack pointer. An x87 instruction is not a call. A compiler is free
    /// to keep a `long double`'s bytes in the 128 bytes below `%rsp` across
    /// one, and a helper frame allocated from the guest's pointer lands
    /// exactly there.
    ///
    /// Saved and restored around the call rather than set once per function,
    /// because `%rsp` moves within a function and the helper's frame has to
    /// stay clear of wherever it has reached.
    fn call_x87(&mut self, body: &mut FunctionBodyBuilder, helper: X87Helper) -> Result<()> {
        let reference = self.symbols.x87_helper(helper)?;
        let stack_top = self.symbols.x87_stack()?;
        let saved = self.temporaries.take(body, ValueType::I32);

        body.global_get(self.state.linker_stack_pointer());
        body.local_set(saved);

        body.i32_const_data_address(stack_top);
        body.i32_const(-STACK_ALIGNMENT);
        body.i32_and();
        body.global_set(self.state.linker_stack_pointer());

        body.call(reference);

        body.local_get(saved);
        body.global_set(self.state.linker_stack_pointer());
        Ok(())
    }

    /// The `ST(i)` the instruction's *last* operand names.
    ///
    /// Which operand carries the index is not constant, and the difference
    /// is easy to get backwards: `ffree st(2)`, `fld st(2)` and `fstp st(2)`
    /// have one operand and it is the index, while `fxch st(1)`,
    /// `fcom st(2)` and `fcmovb st, st(2)` have two of which the first is
    /// always `ST0`. The last operand is the index in every one of them.
    /// `fcompp` and `fucompp` have none at all and mean `st(1)`, which is
    /// what the caller's default supplies.
    ///
    /// The forms this does *not* serve are the arithmetic ones, where both
    /// operands matter and the first is the destination.
    fn x87_stack_operand(&self, lifted: &LiftedInstruction, default: u32) -> u32 {
        let count = lifted.instruction.op_count();
        if count == 0 {
            return default;
        }
        self.x87_register_operand(lifted, count - 1)
            .unwrap_or(default)
    }

    /// The `ST(i)` an operand names, if it names one.
    fn x87_register_operand(&self, lifted: &LiftedInstruction, index: u32) -> Option<u32> {
        let instruction = &lifted.instruction;
        if index >= instruction.op_count() || instruction.op_kind(index) != OpKind::Register {
            return None;
        }
        let register = instruction.op_register(index);
        (Register::ST0..=Register::ST7)
            .contains(&register)
            .then(|| register.number() as u32)
    }

    /// Which memory width an operand carries, for the forms that have
    /// several.
    fn x87_memory_size(&self, lifted: &LiftedInstruction) -> iced_x86::MemorySize {
        lifted.instruction.memory_size()
    }

    fn x87_load(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        if let Some(index) = self.x87_register_operand(lifted, 0) {
            body.i32_const(index as i32);
            return self.call_x87(body, X87Helper::FldSti);
        }
        match self.x87_memory_size(lifted) {
            iced_x86::MemorySize::Float32 => {
                self.read_operand(body, lifted, 0, OperandWidth::DoubleWord)?;
                self.call_x87(body, X87Helper::Fld32)
            }
            iced_x86::MemorySize::Float64 => {
                self.read_operand(body, lifted, 0, OperandWidth::QuadWord)?;
                self.call_x87(body, X87Helper::Fld64)
            }
            // Ten bytes have no wasm type, so the crate reads them itself.
            iced_x86::MemorySize::Float80 => {
                self.emit_effective_address(body, lifted)?;
                self.call_x87(body, X87Helper::Fld80)
            }
            other => bail!("`fld` of a {other:?} operand"),
        }
    }

    /// The seven constants, in the order the opcode numbers them: 1, log₂10,
    /// log₂e, π, log₁₀2, ln 2, 0.
    fn x87_constant(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let index = match lifted.instruction.mnemonic() {
            Mnemonic::Fld1 => 0,
            Mnemonic::Fldl2t => 1,
            Mnemonic::Fldl2e => 2,
            Mnemonic::Fldpi => 3,
            Mnemonic::Fldlg2 => 4,
            Mnemonic::Fldln2 => 5,
            _ => 6,
        };
        body.i32_const(index);
        self.call_x87(body, X87Helper::FldConst)
    }

    fn x87_integer_load(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        match self.x87_memory_size(lifted) {
            // Sixteen bits arrive zero-extended in an `i32`; the crate casts
            // back through `i16`, so the raw value is what it wants.
            iced_x86::MemorySize::Int16 => {
                self.read_operand(body, lifted, 0, OperandWidth::Word)?;
                self.call_x87(body, X87Helper::Fild16)
            }
            iced_x86::MemorySize::Int32 => {
                self.read_operand(body, lifted, 0, OperandWidth::DoubleWord)?;
                self.call_x87(body, X87Helper::Fild32)
            }
            iced_x86::MemorySize::Int64 => {
                self.read_operand(body, lifted, 0, OperandWidth::QuadWord)?;
                self.call_x87(body, X87Helper::Fild64)
            }
            other => bail!("`fild` of a {other:?} operand"),
        }
    }

    fn x87_store(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let pop = lifted.instruction.mnemonic() == Mnemonic::Fstp;
        if let Some(index) = self.x87_register_operand(lifted, 0) {
            body.i32_const(index as i32);
            body.i32_const(i32::from(pop));
            return self.call_x87(body, X87Helper::FstSti);
        }
        let (helper, width) = match self.x87_memory_size(lifted) {
            iced_x86::MemorySize::Float32 => (X87Helper::Fst32, OperandWidth::DoubleWord),
            iced_x86::MemorySize::Float64 => (X87Helper::Fst64, OperandWidth::QuadWord),
            iced_x86::MemorySize::Float80 => {
                self.emit_effective_address(body, lifted)?;
                return self.call_x87(body, X87Helper::Fstp80);
            }
            other => bail!("`fst` to a {other:?} operand"),
        };
        body.i32_const(i32::from(pop));
        self.call_x87(body, helper)?;
        self.x87_park_and_store(body, lifted, width)
    }

    fn x87_integer_store(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        truncating: bool,
    ) -> Result<()> {
        let pop = truncating || lifted.instruction.mnemonic() == Mnemonic::Fistp;
        let (helper, width) = match (self.x87_memory_size(lifted), truncating) {
            (iced_x86::MemorySize::Int16, false) => (X87Helper::Fist16, OperandWidth::Word),
            (iced_x86::MemorySize::Int32, false) => (X87Helper::Fist32, OperandWidth::DoubleWord),
            (iced_x86::MemorySize::Int64, false) => (X87Helper::Fist64, OperandWidth::QuadWord),
            (iced_x86::MemorySize::Int16, true) => (X87Helper::Fisttp16, OperandWidth::Word),
            (iced_x86::MemorySize::Int32, true) => (X87Helper::Fisttp32, OperandWidth::DoubleWord),
            (iced_x86::MemorySize::Int64, true) => (X87Helper::Fisttp64, OperandWidth::QuadWord),
            (other, _) => bail!("`fist` to a {other:?} operand"),
        };
        // `fisttp` always pops, so it takes no argument saying whether to.
        if !truncating {
            body.i32_const(i32::from(pop));
        }
        self.call_x87(body, helper)?;
        self.x87_park_and_store(body, lifted, width)
    }

    /// Parks a helper's result and writes it to the instruction's memory
    /// operand.
    ///
    /// Two steps rather than one because `write_operand` computes the
    /// address itself and would otherwise do so underneath a value already
    /// on the stack.
    fn x87_park_and_store(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        width: OperandWidth,
    ) -> Result<()> {
        let value = self.temporaries.take(body, width.value_type());
        body.local_set(value);
        self.write_operand(body, lifted, 0, width, value)
    }

    fn x87_arithmetic(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        op: Binary,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        // The register forms, including the popping ones. The destination is
        // the first operand and the source the second; with no operands at
        // all the instruction means `st(1), st(0)` and pops, which is what
        // the bare `faddp` spelling is.
        if instruction.op_count() == 0 || self.x87_register_operand(lifted, 0).is_some() {
            let popping = matches!(
                instruction.mnemonic(),
                Mnemonic::Faddp
                    | Mnemonic::Fsubp
                    | Mnemonic::Fsubrp
                    | Mnemonic::Fmulp
                    | Mnemonic::Fdivp
                    | Mnemonic::Fdivrp
            );
            let (destination, source) = match instruction.op_count() {
                0 => (1, 0),
                _ => (
                    self.x87_register_operand(lifted, 0).unwrap_or(0),
                    self.x87_register_operand(lifted, 1).unwrap_or(0),
                ),
            };
            body.i32_const(op as i32);
            body.i32_const(destination as i32);
            body.i32_const(source as i32);
            body.i32_const(i32::from(popping));
            return self.call_x87(body, X87Helper::ArithSti);
        }
        let (helper, width) = match self.x87_memory_size(lifted) {
            iced_x86::MemorySize::Float32 => (X87Helper::Arith32, OperandWidth::DoubleWord),
            iced_x86::MemorySize::Float64 => (X87Helper::Arith64, OperandWidth::QuadWord),
            other => bail!("x87 arithmetic against a {other:?} operand"),
        };
        body.i32_const(op as i32);
        self.read_operand(body, lifted, 0, width)?;
        self.call_x87(body, helper)
    }

    fn x87_integer_arithmetic(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        op: Binary,
    ) -> Result<()> {
        let (helper, width) = match self.x87_memory_size(lifted) {
            iced_x86::MemorySize::Int16 => (X87Helper::ArithI16, OperandWidth::Word),
            iced_x86::MemorySize::Int32 => (X87Helper::ArithI32, OperandWidth::DoubleWord),
            other => bail!("x87 integer arithmetic against a {other:?} operand"),
        };
        body.i32_const(op as i32);
        self.read_operand(body, lifted, 0, width)?;
        self.call_x87(body, helper)
    }

    /// `fcmovcc`: the predicate comes from the *promoted* flags, through the
    /// same emitter every conditional move uses.
    fn x87_conditional_move(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        use iced_x86::ConditionCode;
        let condition = match lifted.instruction.mnemonic() {
            Mnemonic::Fcmovb => ConditionCode::b,
            Mnemonic::Fcmove => ConditionCode::e,
            Mnemonic::Fcmovbe => ConditionCode::be,
            Mnemonic::Fcmovu => ConditionCode::p,
            Mnemonic::Fcmovnb => ConditionCode::ae,
            Mnemonic::Fcmovne => ConditionCode::ne,
            Mnemonic::Fcmovnbe => ConditionCode::a,
            _ => ConditionCode::np,
        };
        let index = self.x87_stack_operand(lifted, 0);
        body.i32_const(index as i32);
        self.emit_condition(body, condition)?;
        self.call_x87(body, X87Helper::Fcmov)
    }

    fn x87_compare(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let quiet = matches!(
            instruction.mnemonic(),
            Mnemonic::Fucom | Mnemonic::Fucomp | Mnemonic::Fucompp
        );
        let pops = match instruction.mnemonic() {
            Mnemonic::Fcompp | Mnemonic::Fucompp => 2,
            Mnemonic::Fcomp | Mnemonic::Fucomp => 1,
            _ => 0,
        };
        if instruction.op_count() == 0 || self.x87_register_operand(lifted, 0).is_some() {
            // With no operand the comparison is against `st(1)`.
            let index = self.x87_stack_operand(lifted, 1);
            body.i32_const(index as i32);
            body.i32_const(i32::from(quiet));
            body.i32_const(pops);
            return self.call_x87(body, X87Helper::FcomSti);
        }
        let (helper, width) = match self.x87_memory_size(lifted) {
            iced_x86::MemorySize::Float32 => (X87Helper::Fcom32, OperandWidth::DoubleWord),
            iced_x86::MemorySize::Float64 => (X87Helper::Fcom64, OperandWidth::QuadWord),
            other => bail!("`fcom` against a {other:?} operand"),
        };
        self.read_operand(body, lifted, 0, width)?;
        body.i32_const(pops);
        self.call_x87(body, helper)
    }

    fn x87_integer_compare(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let pop = i32::from(lifted.instruction.mnemonic() == Mnemonic::Ficomp);
        let (helper, width) = match self.x87_memory_size(lifted) {
            iced_x86::MemorySize::Int16 => (X87Helper::Ficom16, OperandWidth::Word),
            iced_x86::MemorySize::Int32 => (X87Helper::Ficom32, OperandWidth::DoubleWord),
            other => bail!("`ficom` against a {other:?} operand"),
        };
        self.read_operand(body, lifted, 0, width)?;
        body.i32_const(pop);
        self.call_x87(body, helper)
    }

    /// `fcomi` and friends, which report into the integer flags rather than
    /// into the status word.
    fn x87_compare_to_flags(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        use crate::machine::Flag;
        let instruction = &lifted.instruction;
        let quiet = matches!(instruction.mnemonic(), Mnemonic::Fucomi | Mnemonic::Fucomip);
        let pop = matches!(instruction.mnemonic(), Mnemonic::Fcomip | Mnemonic::Fucomip);
        let index = self.x87_stack_operand(lifted, 1);
        body.i32_const(index as i32);
        body.i32_const(i32::from(quiet));
        body.i32_const(i32::from(pop));
        self.call_x87(body, X87Helper::Fcomi)?;

        // The helper answers in the layout the hardware uses: carry at bit
        // zero, parity at bit two, zero at bit six.
        let packed = self.temporaries.take(body, ValueType::I32);
        body.local_set(packed);
        for (flag, shift) in [(Flag::Carry, 0), (Flag::Parity, 2), (Flag::Zero, 6)] {
            body.local_get(packed);
            if shift != 0 {
                body.i32_const(shift);
                body.i32_shr_unsigned();
            }
            body.i32_const(1);
            body.i32_and();
            self.state.write_flag(body, flag);
        }
        // Overflow and sign are cleared, which is what the instruction
        // defines; the adjust flag is not modelled at all.
        for flag in [Flag::Overflow, Flag::Sign] {
            body.i32_const(0);
            self.state.write_flag(body, flag);
        }
        Ok(())
    }

    /// `fnstsw`/`fnstcw`, to `%ax` or to memory.
    fn x87_read_word(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        helper: X87Helper,
    ) -> Result<()> {
        self.call_x87(body, helper)?;
        let value = self.temporaries.take(body, ValueType::I32);
        body.local_set(value);
        if lifted.instruction.op_count() > 0
            && lifted.instruction.op_kind(0) == OpKind::Register
            && lifted.instruction.op_register(0) == Register::AX
        {
            // Word width on purpose: the instruction writes `%ax` and leaves
            // the rest of `%rax` alone, which a wider write would not.
            body.local_get(value);
            self.state
                .write_register(body, RegisterSlice::of(Register::AX)?);
            return Ok(());
        }
        self.write_operand(body, lifted, 0, OperandWidth::Word, value)
    }
}
