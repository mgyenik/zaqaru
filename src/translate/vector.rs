//! SSE: the XMM register file, and everything that moves through it —
//! the move family, the bitwise and lane-rearranging families, scalar
//! arithmetic and compares and conversions, and packed operations.
//!
//! The representation is the one the pre-plan experiments forced: each XMM
//! register is a **pair of `i64` globals**, because `wasm-ld` cannot link an
//! object that defines a `v128` global. The pair fits SSE's own grain —
//! scalar operations write the low 64 bits and preserve the high 64, which
//! here is "touch only the low global" — and a `v128` appears only inside a
//! function body, where LLD copies code opaquely and SIMD is unrestricted.
//!
//! Three things here are silently wrong if translated naively, and each is
//! written out rather than mapped:
//!
//! * the move family's **merge/zero asymmetry** — a scalar move from memory
//!   zeroes everything above the value it loads, while the register form of
//!   the same mnemonic merges and leaves the rest alone;
//! * `min`/`max`, which keep the *second* operand on ties and on NaN where
//!   wasm's instructions of the same name do neither;
//! * truncating conversions, which produce x86's integer indefinite where
//!   wasm traps or saturates.
//!
//! The corpus carries cases for all three whose results differ if the rule is
//! wrong, and each was confirmed by deliberately breaking it.

use anyhow::{Result, bail};
use iced_x86::{Instruction, Mnemonic};

use crate::emitter::ValueType;
use crate::emitter::code::FunctionBodyBuilder;
use crate::lifter::LiftedInstruction;
use crate::machine::{Flag, OperandWidth, VectorHalf, vector_register_number};

use super::{FunctionTranslator, is_immediate, render};

use FloatWidth::{Double, Single};
use PackedOperation::{
    Add, ConvertFromSignedLanes, Equal, FloatAdd, FloatDivide, FloatMultiply, FloatSquareRoot,
    FloatSubtract, GreaterSigned, MaximumSigned, MaximumUnsigned, MinimumSigned, MinimumUnsigned,
    Multiply, Subtract,
};

/// Whether the vector translation recognised an instruction, so that one that
/// is neither an integer nor a vector instruction still gets the single
/// "not implemented" error the design promises.
pub(super) enum VectorOutcome {
    Translated,
    NotAVectorInstruction,
}

/// What an XMM write does with the bits it carries no value for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VectorMerge {
    /// Everything above the written lane becomes zero. This is what a move
    /// from memory or from a general-purpose register does.
    Zero,
    /// Everything above the written lane keeps its previous value. This is
    /// what the register-to-register scalar moves — and all scalar
    /// arithmetic — do.
    Preserve,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BitwiseOperation {
    And,
    AndComplemented,
    Or,
    Xor,
}

/// SSE's two scalar floating-point widths. Both map onto wasm arithmetic
/// directly: wasm and SSE are IEEE-754 round-to-nearest-even, which is the
/// mode `MXCSR` starts in and which nothing in scope changes, so results are
/// bit-exact rather than merely close.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FloatWidth {
    Single,
    Double,
}

impl FloatWidth {
    /// The lane a scalar operation of this width occupies.
    fn lane(self) -> OperandWidth {
        match self {
            FloatWidth::Single => OperandWidth::DoubleWord,
            FloatWidth::Double => OperandWidth::QuadWord,
        }
    }

    fn value_type(self) -> ValueType {
        match self {
            FloatWidth::Single => ValueType::F32,
            FloatWidth::Double => ValueType::F64,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FloatOperation {
    Add,
    Subtract,
    Multiply,
    Divide,
}

/// Which operand `minsd`/`maxsd` keep. The architecture returns the *second*
/// operand whenever the comparison does not strictly hold — on ties, and when
/// either operand is NaN — which is not what wasm's `min`/`max` do, so those
/// are emulated with a compare and a select and the wasm instructions are
/// never used.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FloatExtremum {
    Minimum,
    Maximum,
}

/// Alignment hint for a vector memory access, as a base-two logarithm.
///
/// Wasm loads never fault on a misaligned address, so this only tells the
/// engine what to expect: the `a` forms of the 128-bit moves require sixteen
/// byte alignment on the machine, the `u` forms promise nothing.
const ALIGNED_ACCESS: u32 = 3;
const UNALIGNED_ACCESS: u32 = 0;

/// Byte offset of an XMM register's high half within a 128-bit memory
/// operand.
const HIGH_HALF_OFFSET: u32 = 8;

impl FunctionTranslator<'_> {
    pub(super) fn translate_vector(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<VectorOutcome> {
        let instruction = &lifted.instruction;
        match instruction.mnemonic() {
            // ---- the move family -------------------------------------------
            //
            // `movq` and `movd` always zero what they do not write, whichever
            // direction they run in.
            Mnemonic::Movq => {
                self.translate_scalar_move(body, lifted, OperandWidth::QuadWord, VectorMerge::Zero)?
            }
            Mnemonic::Movd => self.translate_scalar_move(
                body,
                lifted,
                OperandWidth::DoubleWord,
                VectorMerge::Zero,
            )?,
            // `movsd`/`movss` merge when they move register to register, and
            // zero when the source is memory.
            Mnemonic::Movsd => {
                let merge = self.scalar_move_merge(instruction);
                self.translate_scalar_move(body, lifted, OperandWidth::QuadWord, merge)?
            }
            Mnemonic::Movss => {
                let merge = self.scalar_move_merge(instruction);
                self.translate_scalar_move(body, lifted, OperandWidth::DoubleWord, merge)?
            }
            Mnemonic::Movaps | Mnemonic::Movapd | Mnemonic::Movdqa => {
                self.translate_whole_move(body, lifted, ALIGNED_ACCESS)?
            }
            Mnemonic::Movups | Mnemonic::Movupd | Mnemonic::Movdqu => {
                self.translate_whole_move(body, lifted, UNALIGNED_ACCESS)?
            }
            // The non-temporal stores, which differ from an ordinary one
            // only in asking the processor not to keep the line in cache.
            // There is no cache here to ask about, and the architecture
            // makes the hint advisory — what is left is the store, at the
            // alignment each form requires. glibc's `memcpy` uses them for
            // copies large enough that keeping the source would evict
            // everything else.
            Mnemonic::Movntdq | Mnemonic::Movntps | Mnemonic::Movntpd => {
                self.translate_whole_move(body, lifted, ALIGNED_ACCESS)?
            }
            Mnemonic::Movntdqa => self.translate_whole_move(body, lifted, ALIGNED_ACCESS)?,
            Mnemonic::Movlps | Mnemonic::Movlpd => {
                self.translate_half_move(body, lifted, VectorHalf::Low)?
            }
            Mnemonic::Movhps | Mnemonic::Movhpd => {
                self.translate_half_move(body, lifted, VectorHalf::High)?
            }
            Mnemonic::Movhlps => {
                self.translate_half_transfer(body, lifted, VectorHalf::High, VectorHalf::Low)?
            }
            Mnemonic::Movlhps => {
                self.translate_half_transfer(body, lifted, VectorHalf::Low, VectorHalf::High)?
            }
            // ---- the bitwise family ----------------------------------------
            //
            // In pair state each of these is two `i64` operations, which is
            // also how `fabs`, negation and `copysign` are spelled: a mask in
            // read-only data and one bitwise instruction.
            Mnemonic::Pand | Mnemonic::Andps | Mnemonic::Andpd => {
                self.translate_bitwise(body, lifted, BitwiseOperation::And)?
            }
            Mnemonic::Pandn | Mnemonic::Andnps | Mnemonic::Andnpd => {
                self.translate_bitwise(body, lifted, BitwiseOperation::AndComplemented)?
            }
            Mnemonic::Por | Mnemonic::Orps | Mnemonic::Orpd => {
                self.translate_bitwise(body, lifted, BitwiseOperation::Or)?
            }
            // A register exclusive-ored with itself is how every compiler
            // spells "zero this register"; the general rule covers it without
            // recognising the idiom.
            Mnemonic::Pxor | Mnemonic::Xorps | Mnemonic::Xorpd => {
                self.translate_bitwise(body, lifted, BitwiseOperation::Xor)?
            }
            // ---- scalar arithmetic ------------------------------------------
            //
            // All of these merge into the destination's low lane and leave
            // everything above it alone, which in pair state means touching
            // only the low global — and, for the single-precision forms, only
            // its bottom half.
            Mnemonic::Addsd => {
                self.translate_float_arithmetic(body, lifted, Double, FloatOperation::Add)?
            }
            Mnemonic::Addss => {
                self.translate_float_arithmetic(body, lifted, Single, FloatOperation::Add)?
            }
            Mnemonic::Subsd => {
                self.translate_float_arithmetic(body, lifted, Double, FloatOperation::Subtract)?
            }
            Mnemonic::Subss => {
                self.translate_float_arithmetic(body, lifted, Single, FloatOperation::Subtract)?
            }
            Mnemonic::Mulsd => {
                self.translate_float_arithmetic(body, lifted, Double, FloatOperation::Multiply)?
            }
            Mnemonic::Mulss => {
                self.translate_float_arithmetic(body, lifted, Single, FloatOperation::Multiply)?
            }
            Mnemonic::Divsd => {
                self.translate_float_arithmetic(body, lifted, Double, FloatOperation::Divide)?
            }
            Mnemonic::Divss => {
                self.translate_float_arithmetic(body, lifted, Single, FloatOperation::Divide)?
            }
            Mnemonic::Sqrtsd => self.translate_float_square_root(body, lifted, Double)?,
            Mnemonic::Sqrtss => self.translate_float_square_root(body, lifted, Single)?,
            Mnemonic::Minsd => {
                self.translate_float_extremum(body, lifted, Double, FloatExtremum::Minimum)?
            }
            Mnemonic::Minss => {
                self.translate_float_extremum(body, lifted, Single, FloatExtremum::Minimum)?
            }
            Mnemonic::Maxsd => {
                self.translate_float_extremum(body, lifted, Double, FloatExtremum::Maximum)?
            }
            Mnemonic::Maxss => {
                self.translate_float_extremum(body, lifted, Single, FloatExtremum::Maximum)?
            }

            // ---- compares ---------------------------------------------------
            //
            // The two spellings differ only in which floating-point exception
            // a quiet NaN raises, and exceptions are not modelled, so they
            // translate identically.
            Mnemonic::Ucomisd | Mnemonic::Comisd => {
                self.translate_float_compare(body, lifted, Double)?
            }
            Mnemonic::Ucomiss | Mnemonic::Comiss => {
                self.translate_float_compare(body, lifted, Single)?
            }
            // The predicate form, which answers in bits rather than in flags.
            Mnemonic::Cmpsd => self.translate_float_compare_mask(body, lifted, Double)?,
            Mnemonic::Cmpss => self.translate_float_compare_mask(body, lifted, Single)?,

            // ---- conversions ------------------------------------------------
            Mnemonic::Cvttsd2si => self.translate_float_to_integer(body, lifted, Double)?,
            Mnemonic::Cvttss2si => self.translate_float_to_integer(body, lifted, Single)?,
            Mnemonic::Cvtsi2sd => self.translate_integer_to_float(body, lifted, Double)?,
            Mnemonic::Cvtsi2ss => self.translate_integer_to_float(body, lifted, Single)?,
            Mnemonic::Cvtsd2ss => self.translate_float_resize(body, lifted, Double)?,
            Mnemonic::Cvtss2sd => self.translate_float_resize(body, lifted, Single)?,

            // ---- packed arithmetic ------------------------------------------
            //
            // Auto-vectorised loops are where these come from: code nobody
            // wrote with vectors in mind, compiled at `-O2` or above. Each is
            // one wasm SIMD instruction over a `v128` assembled from the
            // register pair.
            Mnemonic::Paddb => self.packed_binary(body, lifted, Add(LaneWidth::Byte))?,
            Mnemonic::Paddw => self.packed_binary(body, lifted, Add(LaneWidth::Word))?,
            Mnemonic::Paddd => self.packed_binary(body, lifted, Add(LaneWidth::DoubleWord))?,
            Mnemonic::Paddq => self.packed_binary(body, lifted, Add(LaneWidth::QuadWord))?,
            Mnemonic::Psubb => self.packed_binary(body, lifted, Subtract(LaneWidth::Byte))?,
            Mnemonic::Psubw => self.packed_binary(body, lifted, Subtract(LaneWidth::Word))?,
            Mnemonic::Psubd => self.packed_binary(body, lifted, Subtract(LaneWidth::DoubleWord))?,
            Mnemonic::Psubq => self.packed_binary(body, lifted, Subtract(LaneWidth::QuadWord))?,
            Mnemonic::Pmullw => self.packed_binary(body, lifted, Multiply(LaneWidth::Word))?,
            Mnemonic::Pmulld => {
                self.packed_binary(body, lifted, Multiply(LaneWidth::DoubleWord))?
            }
            Mnemonic::Pmuludq => self.translate_unsigned_wide_multiply(body, lifted)?,
            // The extrema, which x86 offers only where it happened to need
            // them: unsigned bytes and signed words. An SSE2 `strlen`
            // reaches for `pminub` on every iteration.
            Mnemonic::Pminub => {
                self.packed_binary(body, lifted, MinimumUnsigned(LaneWidth::Byte))?
            }
            Mnemonic::Pmaxub => {
                self.packed_binary(body, lifted, MaximumUnsigned(LaneWidth::Byte))?
            }
            Mnemonic::Pminsw => self.packed_binary(body, lifted, MinimumSigned(LaneWidth::Word))?,
            Mnemonic::Pmaxsw => self.packed_binary(body, lifted, MaximumSigned(LaneWidth::Word))?,
            Mnemonic::Pcmpeqb => self.packed_binary(body, lifted, Equal(LaneWidth::Byte))?,
            Mnemonic::Pcmpeqw => self.packed_binary(body, lifted, Equal(LaneWidth::Word))?,
            Mnemonic::Pcmpeqd => self.packed_binary(body, lifted, Equal(LaneWidth::DoubleWord))?,
            Mnemonic::Pcmpeqq => self.packed_binary(body, lifted, Equal(LaneWidth::QuadWord))?,
            Mnemonic::Pcmpgtb => {
                self.packed_binary(body, lifted, GreaterSigned(LaneWidth::Byte))?
            }
            Mnemonic::Pcmpgtw => {
                self.packed_binary(body, lifted, GreaterSigned(LaneWidth::Word))?
            }
            Mnemonic::Pcmpgtd => {
                self.packed_binary(body, lifted, GreaterSigned(LaneWidth::DoubleWord))?
            }
            Mnemonic::Pcmpgtq => {
                self.packed_binary(body, lifted, GreaterSigned(LaneWidth::QuadWord))?
            }
            Mnemonic::Addpd => self.packed_binary(body, lifted, FloatAdd(Double))?,
            Mnemonic::Addps => self.packed_binary(body, lifted, FloatAdd(Single))?,
            Mnemonic::Subpd => self.packed_binary(body, lifted, FloatSubtract(Double))?,
            Mnemonic::Subps => self.packed_binary(body, lifted, FloatSubtract(Single))?,
            Mnemonic::Mulpd => self.packed_binary(body, lifted, FloatMultiply(Double))?,
            Mnemonic::Mulps => self.packed_binary(body, lifted, FloatMultiply(Single))?,
            Mnemonic::Divpd => self.packed_binary(body, lifted, FloatDivide(Double))?,
            Mnemonic::Divps => self.packed_binary(body, lifted, FloatDivide(Single))?,
            Mnemonic::Sqrtpd => {
                self.translate_packed_unary(body, lifted, FloatSquareRoot(Double))?
            }
            Mnemonic::Sqrtps => {
                self.translate_packed_unary(body, lifted, FloatSquareRoot(Single))?
            }
            Mnemonic::Cvtdq2pd => {
                self.translate_packed_unary(body, lifted, ConvertFromSignedLanes(Double))?
            }
            Mnemonic::Cvtdq2ps => {
                self.translate_packed_unary(body, lifted, ConvertFromSignedLanes(Single))?
            }
            Mnemonic::Cmppd => self.translate_packed_compare_mask(body, lifted, Double)?,
            Mnemonic::Cmpps => self.translate_packed_compare_mask(body, lifted, Single)?,

            // The sign bits gathered into an integer register, which is how a
            // vectorised test collapses to a scalar one — and how `signbit`
            // compiles even with no vector in sight.
            Mnemonic::Movmskpd => self.translate_sign_mask(body, lifted, LaneWidth::QuadWord)?,
            Mnemonic::Movmskps => self.translate_sign_mask(body, lifted, LaneWidth::DoubleWord)?,
            // The same gathering at byte grain, and by far the most common
            // of the three: it is how every SSE2 string function turns
            // sixteen lanes of comparison into an integer to scan.
            Mnemonic::Pmovmskb => self.translate_sign_mask(body, lifted, LaneWidth::Byte)?,

            // ---- packed shifts ----------------------------------------------
            Mnemonic::Psllw => {
                self.translate_packed_shift(body, lifted, LaneWidth::Word, PackedShift::Left)?
            }
            Mnemonic::Pslld => {
                self.translate_packed_shift(body, lifted, LaneWidth::DoubleWord, PackedShift::Left)?
            }
            Mnemonic::Psllq => {
                self.translate_packed_shift(body, lifted, LaneWidth::QuadWord, PackedShift::Left)?
            }
            Mnemonic::Psrlw => self.translate_packed_shift(
                body,
                lifted,
                LaneWidth::Word,
                PackedShift::RightUnsigned,
            )?,
            Mnemonic::Psrld => self.translate_packed_shift(
                body,
                lifted,
                LaneWidth::DoubleWord,
                PackedShift::RightUnsigned,
            )?,
            Mnemonic::Psrlq => self.translate_packed_shift(
                body,
                lifted,
                LaneWidth::QuadWord,
                PackedShift::RightUnsigned,
            )?,
            Mnemonic::Psraw => self.translate_packed_shift(
                body,
                lifted,
                LaneWidth::Word,
                PackedShift::RightSigned,
            )?,
            Mnemonic::Psrad => self.translate_packed_shift(
                body,
                lifted,
                LaneWidth::DoubleWord,
                PackedShift::RightSigned,
            )?,
            // These two shift the register by whole bytes rather than shifting
            // each lane, so they are pair arithmetic rather than a lane
            // operation.
            Mnemonic::Psrldq => {
                self.translate_byte_shift(body, lifted, PackedShift::RightUnsigned)?
            }
            Mnemonic::Pslldq => self.translate_byte_shift(body, lifted, PackedShift::Left)?,

            // ---- the lane-rearranging family --------------------------------
            //
            // Every one of these is a fixed table saying where each of the
            // result's four doubleword lanes comes from, so they share one
            // translation.
            Mnemonic::Pshufd => {
                // Each pair of selector bits names one of the source's four
                // doubleword lanes.
                let selector = instruction.immediate8();
                let lanes = std::array::from_fn(|lane| LaneSource {
                    operand: SOURCE,
                    lane: usize::from(selector >> (2 * lane)) & 3,
                });
                self.translate_shuffle(body, lifted, lanes)?
            }
            // Interleaving at byte and word grain, which the doubleword
            // shuffle below cannot express. `punpcklbw` against a register
            // holding one byte is how an SSE2 `memset` broadcasts its fill
            // value across a whole vector.
            Mnemonic::Punpcklbw => {
                self.translate_interleave(body, lifted, LaneWidth::Byte, false)?
            }
            Mnemonic::Punpckhbw => {
                self.translate_interleave(body, lifted, LaneWidth::Byte, true)?
            }
            Mnemonic::Punpcklwd => {
                self.translate_interleave(body, lifted, LaneWidth::Word, false)?
            }
            Mnemonic::Punpckhwd => {
                self.translate_interleave(body, lifted, LaneWidth::Word, true)?
            }
            Mnemonic::Punpckldq | Mnemonic::Unpcklps => self.translate_shuffle(
                body,
                lifted,
                lane_table([(DESTINATION, 0), (SOURCE, 0), (DESTINATION, 1), (SOURCE, 1)]),
            )?,
            Mnemonic::Punpckhdq | Mnemonic::Unpckhps => self.translate_shuffle(
                body,
                lifted,
                lane_table([(DESTINATION, 2), (SOURCE, 2), (DESTINATION, 3), (SOURCE, 3)]),
            )?,
            Mnemonic::Punpcklqdq | Mnemonic::Unpcklpd => self.translate_shuffle(
                body,
                lifted,
                lane_table([(DESTINATION, 0), (DESTINATION, 1), (SOURCE, 0), (SOURCE, 1)]),
            )?,
            Mnemonic::Punpckhqdq | Mnemonic::Unpckhpd => self.translate_shuffle(
                body,
                lifted,
                lane_table([(DESTINATION, 2), (DESTINATION, 3), (SOURCE, 2), (SOURCE, 3)]),
            )?,
            Mnemonic::Shufps => {
                // The low two lanes come from the destination, the high two
                // from the source, two selector bits each.
                let selector = usize::from(instruction.immediate8());
                self.translate_shuffle(
                    body,
                    lifted,
                    lane_table([
                        (DESTINATION, selector & 3),
                        (DESTINATION, (selector >> 2) & 3),
                        (SOURCE, (selector >> 4) & 3),
                        (SOURCE, (selector >> 6) & 3),
                    ]),
                )?
            }
            Mnemonic::Shufpd => {
                // One selector bit per quadword lane, so each contributes a
                // pair of adjacent doubleword lanes.
                let selector = usize::from(instruction.immediate8());
                let low = 2 * (selector & 1);
                let high = 2 * ((selector >> 1) & 1);
                self.translate_shuffle(
                    body,
                    lifted,
                    lane_table([
                        (DESTINATION, low),
                        (DESTINATION, low + 1),
                        (SOURCE, high),
                        (SOURCE, high + 1),
                    ]),
                )?
            }
            _ => return Ok(VectorOutcome::NotAVectorInstruction),
        }
        Ok(VectorOutcome::Translated)
    }

    // ---- operand plumbing ------------------------------------------------

    /// The XMM register an operand names, if it names one.
    fn vector_operand(&self, instruction: &Instruction, index: u32) -> Option<usize> {
        vector_register_number(instruction.op_register(index))
    }

    fn expect_vector_operand(&self, instruction: &Instruction, index: u32) -> Result<usize> {
        self.vector_operand(instruction, index).ok_or_else(|| {
            anyhow::anyhow!(
                "operand {index} of `{}` is not an XMM register; the MMX and \
                 x87 register files are not modelled",
                render(instruction)
            )
        })
    }

    /// Confirms that an operand really is a memory reference before its
    /// effective address is computed, since a register operand has none and
    /// would silently compute zero.
    fn expect_memory_operand(&self, instruction: &Instruction, index: u32) -> Result<()> {
        if instruction.op_kind(index) != iced_x86::OpKind::Memory {
            bail!(
                "operand {index} of `{}` is not a memory reference",
                render(instruction)
            );
        }
        Ok(())
    }

    /// Whether a scalar move merges into its destination or zeroes around it:
    /// the register-to-register form merges, every other form zeroes.
    fn scalar_move_merge(&self, instruction: &Instruction) -> VectorMerge {
        if self.vector_operand(instruction, 1).is_some() {
            VectorMerge::Preserve
        } else {
            VectorMerge::Zero
        }
    }

    /// Pushes the low `width` bits of an operand, in the carrier that width
    /// travels in.
    fn read_vector_low(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        index: u32,
        width: OperandWidth,
    ) -> Result<()> {
        let Some(number) = self.vector_operand(&lifted.instruction, index) else {
            // A general-purpose register or a memory location: the integer
            // operand path already reads both.
            return self.read_operand(body, lifted, index, width);
        };
        self.state.read_vector(body, number, VectorHalf::Low);
        match width {
            OperandWidth::QuadWord => {}
            OperandWidth::DoubleWord => body.i32_wrap_i64(),
            other => bail!("an XMM register cannot be read {other:?} at a time"),
        }
        Ok(())
    }

    /// Pushes one half of a 128-bit operand as an `i64`.
    fn read_vector_half(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        index: u32,
        half: VectorHalf,
        alignment_log2: u32,
    ) -> Result<()> {
        match self.vector_operand(&lifted.instruction, index) {
            Some(number) => self.state.read_vector(body, number, half),
            None => {
                self.expect_memory_operand(&lifted.instruction, index)?;
                self.emit_effective_address(body, lifted)?;
                body.i64_load(alignment_log2, half_offset(half));
            }
        }
        Ok(())
    }

    /// Stores the value on top of the stack into an XMM register's low lane,
    /// applying the instruction form's write mask.
    fn write_vector_low(
        &mut self,
        body: &mut FunctionBodyBuilder,
        number: usize,
        width: OperandWidth,
        merge: VectorMerge,
    ) -> Result<()> {
        match width {
            OperandWidth::QuadWord => self.state.write_vector(body, number, VectorHalf::Low),
            OperandWidth::DoubleWord => {
                body.i64_extend_i32_unsigned();
                if merge == VectorMerge::Preserve {
                    // Bits 32..64 belong to the other half of the low lane
                    // pair and must survive.
                    self.state.read_vector(body, number, VectorHalf::Low);
                    body.i64_const(!0xffff_ffffu64 as i64);
                    body.i64_and();
                    body.i64_or();
                }
                self.state.write_vector(body, number, VectorHalf::Low);
            }
            other => bail!("an XMM register cannot be written {other:?} at a time"),
        }
        if merge == VectorMerge::Zero {
            body.i64_const(0);
            self.state.write_vector(body, number, VectorHalf::High);
        }
        Ok(())
    }

    // ---- the move family -------------------------------------------------

    /// A scalar move in either direction: into an XMM register's low lane, or
    /// out of it into a general-purpose register or memory.
    fn translate_scalar_move(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        width: OperandWidth,
        merge: VectorMerge,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        match self.vector_operand(instruction, 0) {
            Some(destination) => {
                self.read_vector_low(body, lifted, 1, width)?;
                self.write_vector_low(body, destination, width, merge)
            }
            None => {
                // Out of an XMM register: the value has to be parked in a
                // local because a store takes its address first.
                self.read_vector_low(body, lifted, 1, width)?;
                let value = self.temporaries.take(body, width.value_type());
                body.local_set(value);
                self.write_operand(body, lifted, 0, width, value)
            }
        }
    }

    /// A whole-register move: `movaps` and its relatives, which copy all 128
    /// bits and are what a struct copy compiles to.
    fn translate_whole_move(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        alignment_log2: u32,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        match (
            self.vector_operand(instruction, 0),
            self.vector_operand(instruction, 1),
        ) {
            (Some(destination), _) => {
                for half in [VectorHalf::Low, VectorHalf::High] {
                    self.read_vector_half(body, lifted, 1, half, alignment_log2)?;
                    self.state.write_vector(body, destination, half);
                }
                Ok(())
            }
            (None, Some(source)) => {
                self.expect_memory_operand(instruction, 0)?;
                for half in [VectorHalf::Low, VectorHalf::High] {
                    self.emit_effective_address(body, lifted)?;
                    self.state.read_vector(body, source, half);
                    body.i64_store(alignment_log2, half_offset(half));
                }
                Ok(())
            }
            (None, None) => bail!(
                "`{}` moves nothing to or from an XMM register",
                render(instruction)
            ),
        }
    }

    /// `movlpd`/`movhpd` and their `ps` spellings: one half of a register
    /// moved to or from memory, with the other half left alone.
    fn translate_half_move(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        half: VectorHalf,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        match self.vector_operand(instruction, 0) {
            Some(destination) => {
                self.expect_memory_operand(instruction, 1)?;
                self.emit_effective_address(body, lifted)?;
                body.i64_load(ALIGNED_ACCESS, 0);
                self.state.write_vector(body, destination, half);
                Ok(())
            }
            None => {
                let source = self.expect_vector_operand(instruction, 1)?;
                self.expect_memory_operand(instruction, 0)?;
                self.emit_effective_address(body, lifted)?;
                self.state.read_vector(body, source, half);
                body.i64_store(ALIGNED_ACCESS, 0);
                Ok(())
            }
        }
    }

    /// `movhlps`/`movlhps`: one half of a register copied into the other half
    /// of another, with the destination's remaining half preserved.
    fn translate_half_transfer(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        from: VectorHalf,
        to: VectorHalf,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let destination = self.expect_vector_operand(instruction, 0)?;
        let source = self.expect_vector_operand(instruction, 1)?;
        self.state.read_vector(body, source, from);
        self.state.write_vector(body, destination, to);
        Ok(())
    }

    // ---- scalar floating point -------------------------------------------

    /// Pushes an operand's low lane as a wasm float, reinterpreting the bits
    /// the XMM pair holds.
    fn read_float_operand(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        index: u32,
        width: FloatWidth,
    ) -> Result<()> {
        self.read_vector_low(body, lifted, index, width.lane())?;
        reinterpret_as_float(body, width);
        Ok(())
    }

    /// Parks an operand's low lane in a float local, for the translations
    /// that need to read it more than once.
    fn park_float_operand(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        index: u32,
        width: FloatWidth,
    ) -> Result<u32> {
        self.read_float_operand(body, lifted, index, width)?;
        let local = self.temporaries.take(body, width.value_type());
        body.local_set(local);
        Ok(local)
    }

    /// Stores the float on top of the stack into the destination's low lane,
    /// leaving everything above it as it was — which is what every scalar SSE
    /// operation does.
    fn write_float_low(
        &mut self,
        body: &mut FunctionBodyBuilder,
        destination: usize,
        width: FloatWidth,
    ) -> Result<()> {
        reinterpret_as_integer(body, width);
        self.write_vector_low(body, destination, width.lane(), VectorMerge::Preserve)
    }

    fn translate_float_arithmetic(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        width: FloatWidth,
        operation: FloatOperation,
    ) -> Result<()> {
        let destination = self.expect_vector_operand(&lifted.instruction, 0)?;
        self.read_float_operand(body, lifted, 0, width)?;
        self.read_float_operand(body, lifted, 1, width)?;
        match (width, operation) {
            (Single, FloatOperation::Add) => body.f32_add(),
            (Single, FloatOperation::Subtract) => body.f32_sub(),
            (Single, FloatOperation::Multiply) => body.f32_mul(),
            (Single, FloatOperation::Divide) => body.f32_div(),
            (Double, FloatOperation::Add) => body.f64_add(),
            (Double, FloatOperation::Subtract) => body.f64_sub(),
            (Double, FloatOperation::Multiply) => body.f64_mul(),
            (Double, FloatOperation::Divide) => body.f64_div(),
        }
        self.write_float_low(body, destination, width)
    }

    /// `sqrtsd`/`sqrtss`, which take their input from the *source* operand
    /// rather than from the destination they merge into.
    fn translate_float_square_root(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        width: FloatWidth,
    ) -> Result<()> {
        let destination = self.expect_vector_operand(&lifted.instruction, 0)?;
        self.read_float_operand(body, lifted, 1, width)?;
        match width {
            Single => body.f32_sqrt(),
            Double => body.f64_sqrt(),
        }
        self.write_float_low(body, destination, width)
    }

    /// `minsd`/`maxsd` and their single-precision forms.
    ///
    /// The architecture's rule is exactly "keep the first operand when the
    /// strict comparison holds, otherwise the second" — which is why ties and
    /// NaNs both yield the second operand, and why the wasm `min`/`max`
    /// instructions, which propagate NaN and order the zeroes, cannot be used
    /// here.
    fn translate_float_extremum(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        width: FloatWidth,
        extremum: FloatExtremum,
    ) -> Result<()> {
        let destination = self.expect_vector_operand(&lifted.instruction, 0)?;
        let first = self.park_float_operand(body, lifted, 0, width)?;
        let second = self.park_float_operand(body, lifted, 1, width)?;

        body.local_get(first);
        body.local_get(second);
        body.local_get(first);
        body.local_get(second);
        match (width, extremum) {
            (Single, FloatExtremum::Minimum) => body.f32_lt(),
            (Single, FloatExtremum::Maximum) => body.f32_gt(),
            (Double, FloatExtremum::Minimum) => body.f64_lt(),
            (Double, FloatExtremum::Maximum) => body.f64_gt(),
        }
        body.select();
        self.write_float_low(body, destination, width)
    }

    /// `ucomisd`/`comisd` and their single-precision forms, which report
    /// through the zero, parity and carry flags and clear sign and overflow:
    ///
    /// | relation | ZF | PF | CF |
    /// |---|---|---|---|
    /// | unordered | 1 | 1 | 1 |
    /// | less | 0 | 0 | 1 |
    /// | equal | 1 | 0 | 0 |
    /// | greater | 0 | 0 | 0 |
    ///
    /// After which `jb`/`je`/`ja` read exactly what they read after an
    /// unsigned integer compare, and `jp` catches the NaN case.
    fn translate_float_compare(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        width: FloatWidth,
    ) -> Result<()> {
        let first = self.park_float_operand(body, lifted, 0, width)?;
        let second = self.park_float_operand(body, lifted, 1, width)?;
        let unordered = self.temporaries.take(body, ValueType::I32);

        let compare = |body: &mut FunctionBodyBuilder, relation: FloatRelation| {
            body.local_get(first);
            body.local_get(second);
            emit_float_relation(body, width, relation);
        };

        emit_ordered(body, width, first, second);
        body.i32_eqz();
        body.local_set(unordered);

        compare(body, FloatRelation::Equal);
        body.local_get(unordered);
        body.i32_or();
        self.state.write_flag(body, Flag::Zero);

        body.local_get(unordered);
        self.state.write_flag(body, Flag::Parity);

        compare(body, FloatRelation::Less);
        body.local_get(unordered);
        body.i32_or();
        self.state.write_flag(body, Flag::Carry);

        for cleared in [Flag::Sign, Flag::Overflow] {
            body.i32_const(0);
            self.state.write_flag(body, cleared);
        }
        Ok(())
    }

    /// `cmpsd`/`cmpss`: a predicate over the two low lanes, answering with a
    /// lane of all ones or all zeroes rather than with flags.
    ///
    /// This is how branchless floating-point selection is spelled — the mask
    /// goes on to `andpd`/`andnpd`/`orpd`, or to `movmskpd` — and clang
    /// reaches for it where gcc branches. Only the low lane is written; the
    /// rest of the destination is preserved as in every scalar operation.
    fn translate_float_compare_mask(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        width: FloatWidth,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let destination = self.expect_vector_operand(instruction, 0)?;
        let predicate = FloatPredicate::decode(instruction.immediate8()).ok_or_else(|| {
            anyhow::anyhow!(
                "`{}` names predicate {} , which the legacy encoding has no \
                 form for",
                render(instruction),
                instruction.immediate8()
            )
        })?;
        let first = self.park_float_operand(body, lifted, 0, width)?;
        let second = self.park_float_operand(body, lifted, 1, width)?;

        let compare = |body: &mut FunctionBodyBuilder, relation: FloatRelation| {
            body.local_get(first);
            body.local_get(second);
            emit_float_relation(body, width, relation);
        };
        // Turning the answer into a mask is `0 - it`, so the zero goes on
        // first and the predicate's boolean follows.
        body.i32_const(0);
        match predicate {
            FloatPredicate::Equal => compare(body, FloatRelation::Equal),
            FloatPredicate::Less => compare(body, FloatRelation::Less),
            FloatPredicate::LessOrEqual => compare(body, FloatRelation::LessOrEqual),
            // `NotEqual`, `NotLess` and `NotLessOrEqual` are true for an
            // unordered pair, which falls out of complementing a comparison
            // that is false there.
            FloatPredicate::NotEqual => {
                compare(body, FloatRelation::Equal);
                body.i32_eqz();
            }
            FloatPredicate::NotLess => {
                compare(body, FloatRelation::Less);
                body.i32_eqz();
            }
            FloatPredicate::NotLessOrEqual => {
                compare(body, FloatRelation::LessOrEqual);
                body.i32_eqz();
            }
            FloatPredicate::Unordered | FloatPredicate::Ordered => {
                emit_ordered(body, width, first, second);
                if predicate == FloatPredicate::Unordered {
                    body.i32_eqz();
                }
            }
        }
        body.i32_sub();
        if width == Double {
            body.i64_extend_i32_signed();
        }
        self.write_vector_low(body, destination, width.lane(), VectorMerge::Preserve)
    }

    /// `cvttsd2si`/`cvttss2si`: truncate towards zero into an integer
    /// register.
    ///
    /// Where the truncated value does not fit, x86 produces the *integer
    /// indefinite* — the most negative value of the destination width — while
    /// wasm's conversion traps and its saturating conversion clamps. So the
    /// value is truncated first, then bounds-checked against limits that are
    /// exactly representable as floats, and the conversion runs only where it
    /// cannot trap. NaN fails both comparisons and takes the same path as an
    /// overflow, which is what the architecture asks for.
    fn translate_float_to_integer(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        width: FloatWidth,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let destination_width = self.operand_width(instruction, 0)?;
        if !matches!(
            destination_width,
            OperandWidth::DoubleWord | OperandWidth::QuadWord
        ) {
            bail!(
                "`{}` converts into a {destination_width:?} destination, which \
                 the instruction has no form for",
                render(instruction)
            );
        }

        self.read_float_operand(body, lifted, 1, width)?;
        match width {
            Single => body.f32_truncate(),
            Double => body.f64_truncate(),
        }
        let truncated = self.temporaries.take(body, width.value_type());
        body.local_set(truncated);

        let result = self.temporaries.take(body, destination_width.value_type());
        emit_integer_indefinite(body, destination_width);
        body.local_set(result);

        // The bounds are the destination's own limits as floats: both powers
        // of two, so both are exact in either float width and the test is not
        // an approximation of the architecture's rule but the rule itself.
        body.local_get(truncated);
        emit_float_bound(body, width, destination_width, Bound::Lowest);
        match width {
            Single => body.f32_ge(),
            Double => body.f64_ge(),
        }
        body.local_get(truncated);
        emit_float_bound(body, width, destination_width, Bound::PastHighest);
        match width {
            Single => body.f32_lt(),
            Double => body.f64_lt(),
        }
        body.i32_and();
        body.if_();
        body.local_get(truncated);
        match (width, destination_width) {
            (Single, OperandWidth::DoubleWord) => body.i32_truncate_f32_signed(),
            (Single, _) => body.i64_truncate_f32_signed(),
            (Double, OperandWidth::DoubleWord) => body.i32_truncate_f64_signed(),
            (Double, _) => body.i64_truncate_f64_signed(),
        }
        body.local_set(result);
        body.end();

        self.write_operand(body, lifted, 0, destination_width, result)
    }

    /// `cvtsi2sd`/`cvtsi2ss`: an integer register or memory operand into the
    /// destination's low lane, merging as scalar arithmetic does.
    fn translate_integer_to_float(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        width: FloatWidth,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let destination = self.expect_vector_operand(instruction, 0)?;
        let source_width = self.operand_width(instruction, 1)?;
        if !matches!(
            source_width,
            OperandWidth::DoubleWord | OperandWidth::QuadWord
        ) {
            bail!(
                "`{}` converts from a {source_width:?} source, which the \
                 instruction has no form for",
                render(instruction)
            );
        }

        self.read_operand(body, lifted, 1, source_width)?;
        match (width, source_width) {
            (Single, OperandWidth::DoubleWord) => body.f32_convert_i32_signed(),
            (Single, _) => body.f32_convert_i64_signed(),
            (Double, OperandWidth::DoubleWord) => body.f64_convert_i32_signed(),
            (Double, _) => body.f64_convert_i64_signed(),
        }
        self.write_float_low(body, destination, width)
    }

    /// `cvtsd2ss` and `cvtss2sd`: the source's width names which way round
    /// the conversion runs, and the result lands in the destination's low
    /// lane of the *other* width.
    fn translate_float_resize(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        source: FloatWidth,
    ) -> Result<()> {
        let destination = self.expect_vector_operand(&lifted.instruction, 0)?;
        self.read_float_operand(body, lifted, 1, source)?;
        let result = match source {
            Double => {
                body.f32_demote_f64();
                Single
            }
            Single => {
                body.f64_promote_f32();
                Double
            }
        };
        self.write_float_low(body, destination, result)
    }

    // ---- packed operations -----------------------------------------------
    //
    // These are the only place a `v128` appears. XMM *state* cannot be a
    // `v128` global — LLD's object reader cannot parse a `v128.const`
    // initializer — but `v128` locals and SIMD instructions inside a function
    // body are unrestricted, because LLD copies code opaquely. So a packed
    // operation assembles a vector from the pair, works on it, and takes it
    // apart again.

    /// Pushes all 128 bits of an operand as a `v128`.
    fn read_operand_vector(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        index: u32,
    ) -> Result<()> {
        match self.vector_operand(&lifted.instruction, index) {
            Some(number) => {
                self.state.read_vector(body, number, VectorHalf::Low);
                body.i64x2_splat();
                self.state.read_vector(body, number, VectorHalf::High);
                body.i64x2_replace_lane(1);
            }
            None => {
                self.expect_memory_operand(&lifted.instruction, index)?;
                self.emit_effective_address(body, lifted)?;
                body.v128_load(UNALIGNED_ACCESS, 0);
            }
        }
        Ok(())
    }

    /// Splits the `v128` on top of the stack back into a register's pair.
    fn write_register_vector(&mut self, body: &mut FunctionBodyBuilder, destination: usize) {
        let held = self.temporaries.take(body, ValueType::V128);
        body.local_set(held);
        for (lane, half) in [VectorHalf::Low, VectorHalf::High].into_iter().enumerate() {
            body.local_get(held);
            body.i64x2_extract_lane(lane as u8);
            self.state.write_vector(body, destination, half);
        }
    }

    fn packed_binary(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        operation: PackedOperation,
    ) -> Result<()> {
        let destination = self.expect_vector_operand(&lifted.instruction, 0)?;
        self.read_operand_vector(body, lifted, 0)?;
        self.read_operand_vector(body, lifted, 1)?;
        emit_packed(body, operation, &lifted.instruction)?;
        self.write_register_vector(body, destination);
        Ok(())
    }

    /// A packed unary operation, which takes its input from the source and
    /// writes the whole destination.
    fn translate_packed_unary(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        operation: PackedOperation,
    ) -> Result<()> {
        let destination = self.expect_vector_operand(&lifted.instruction, 0)?;
        self.read_operand_vector(body, lifted, 1)?;
        emit_packed(body, operation, &lifted.instruction)?;
        self.write_register_vector(body, destination);
        Ok(())
    }

    /// `pslld`, `psrlq` and the rest: every lane shifted by the same count.
    ///
    /// Only the immediate-count forms are translated, which is what compilers
    /// emit. Where the count reaches the lane's width x86 produces zero — or,
    /// for an arithmetic right shift, every lane filled with its sign — while
    /// wasm reduces the count modulo the width; because the count is known
    /// here, the difference is settled at translation time rather than with a
    /// run-time test.
    fn translate_packed_shift(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        lanes: LaneWidth,
        direction: PackedShift,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let destination = self.expect_vector_operand(instruction, 0)?;
        if !is_immediate(instruction.op_kind(1)) {
            bail!(
                "`{}` takes its shift count from a register, which is not \
                 implemented; compilers emit the immediate form",
                render(instruction)
            );
        }
        let requested = u32::from(instruction.immediate8());
        let width = lanes.bits();

        if requested >= width && direction != PackedShift::RightSigned {
            // Everything is shifted out, so the answer is a zeroed register
            // and no shift instruction is needed at all.
            for half in [VectorHalf::Low, VectorHalf::High] {
                body.i64_const(0);
                self.state.write_vector(body, destination, half);
            }
            return Ok(());
        }

        self.read_operand_vector(body, lifted, 0)?;
        body.i32_const(requested.min(width - 1) as i32);
        match (lanes, direction) {
            (LaneWidth::Byte, PackedShift::Left) => body.i8x16_shift_left(),
            (LaneWidth::Byte, PackedShift::RightUnsigned) => body.i8x16_shift_right_unsigned(),
            (LaneWidth::Byte, PackedShift::RightSigned) => body.i8x16_shift_right_signed(),
            (LaneWidth::Word, PackedShift::Left) => body.i16x8_shift_left(),
            (LaneWidth::Word, PackedShift::RightUnsigned) => body.i16x8_shift_right_unsigned(),
            (LaneWidth::Word, PackedShift::RightSigned) => body.i16x8_shift_right_signed(),
            (LaneWidth::DoubleWord, PackedShift::Left) => body.i32x4_shift_left(),
            (LaneWidth::DoubleWord, PackedShift::RightUnsigned) => {
                body.i32x4_shift_right_unsigned()
            }
            (LaneWidth::DoubleWord, PackedShift::RightSigned) => body.i32x4_shift_right_signed(),
            (LaneWidth::QuadWord, PackedShift::Left) => body.i64x2_shift_left(),
            (LaneWidth::QuadWord, PackedShift::RightUnsigned) => body.i64x2_shift_right_unsigned(),
            (LaneWidth::QuadWord, PackedShift::RightSigned) => body.i64x2_shift_right_signed(),
        }
        self.write_register_vector(body, destination);
        Ok(())
    }

    /// `psrldq`/`pslldq`: the whole register shifted by whole *bytes*, which
    /// is a pair of `i64` shifts rather than a lane operation.
    fn translate_byte_shift(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        direction: PackedShift,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let destination = self.expect_vector_operand(instruction, 0)?;
        if !is_immediate(instruction.op_kind(1)) {
            bail!(
                "`{}` takes its shift count from a register, which the \
                 instruction has no form for",
                render(instruction)
            );
        }
        // A count of sixteen or more empties the register, which the
        // architecture states outright rather than leaving to wrap.
        let bytes = usize::from(instruction.immediate8()).min(16);
        let halves = self.park_operand_halves(body, lifted, 0)?;

        for (slot, half) in [VectorHalf::Low, VectorHalf::High].into_iter().enumerate() {
            emit_shifted_half(body, &halves, slot, bytes, direction);
            self.state.write_vector(body, destination, half);
        }
        Ok(())
    }

    /// `cmppd`/`cmpps`: the scalar compare mask done to every lane at once,
    /// which is what a vectorised conditional becomes.
    ///
    /// Wasm's own packed comparisons already answer in all-ones and all-zero
    /// lanes, exactly as SSE does, so the only work is the predicates wasm
    /// has no instruction for: the complemented ones, and the pair that ask
    /// whether the lanes are ordered at all.
    fn translate_packed_compare_mask(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        width: FloatWidth,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let destination = self.expect_vector_operand(instruction, 0)?;
        let predicate = FloatPredicate::decode(instruction.immediate8()).ok_or_else(|| {
            anyhow::anyhow!(
                "`{}` names predicate {}, which the legacy encoding has no \
                 form for",
                render(instruction),
                instruction.immediate8()
            )
        })?;

        // Both operands are parked, because the ordered predicates compare
        // each with itself as well as with the other.
        self.read_operand_vector(body, lifted, 0)?;
        let first = self.temporaries.take(body, ValueType::V128);
        body.local_set(first);
        self.read_operand_vector(body, lifted, 1)?;
        let second = self.temporaries.take(body, ValueType::V128);
        body.local_set(second);

        let compare = |body: &mut FunctionBodyBuilder, left, right, relation| {
            body.local_get(left);
            body.local_get(right);
            emit_packed_relation(body, width, relation);
        };
        match predicate {
            FloatPredicate::Equal => compare(body, first, second, FloatRelation::Equal),
            FloatPredicate::Less => compare(body, first, second, FloatRelation::Less),
            FloatPredicate::LessOrEqual => compare(body, first, second, FloatRelation::LessOrEqual),
            FloatPredicate::NotEqual => {
                compare(body, first, second, FloatRelation::Equal);
                body.v128_not();
            }
            FloatPredicate::NotLess => {
                compare(body, first, second, FloatRelation::Less);
                body.v128_not();
            }
            FloatPredicate::NotLessOrEqual => {
                compare(body, first, second, FloatRelation::LessOrEqual);
                body.v128_not();
            }
            FloatPredicate::Ordered | FloatPredicate::Unordered => {
                // A lane is ordered exactly when neither of its values is
                // NaN, and a value is NaN exactly when it differs from
                // itself.
                compare(body, first, first, FloatRelation::Equal);
                compare(body, second, second, FloatRelation::Equal);
                body.v128_and();
                if predicate == FloatPredicate::Unordered {
                    body.v128_not();
                }
            }
        }
        self.write_register_vector(body, destination);
        Ok(())
    }

    /// `movmskpd`/`movmskps`: each lane's sign bit gathered into the low bits
    /// of a general-purpose register, everything above them zero.
    ///
    /// A sign bit is the top bit of its lane, and the pair holds the lanes in
    /// a known order, so this is a handful of shifts rather than anything
    /// vector-shaped.
    fn translate_sign_mask(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        lanes: LaneWidth,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let source = self.expect_vector_operand(instruction, 1)?;
        let destination_width = self.operand_width(instruction, 0)?;
        let lanes_per_half = 64 / lanes.bits();

        let mut position = 0u32;
        for half in [VectorHalf::Low, VectorHalf::High] {
            for lane in 0..lanes_per_half {
                self.state.read_vector(body, source, half);
                body.i64_const(i64::from(lanes.bits() * (lane + 1) - 1));
                body.i64_shr_unsigned();
                body.i32_wrap_i64();
                body.i32_const(1);
                body.i32_and();
                if position > 0 {
                    body.i32_const(position as i32);
                    body.i32_shl();
                    body.i32_or();
                }
                position += 1;
            }
        }

        // The result is always a small number; a 64-bit destination gets it
        // zero-extended, which is what the architecture specifies.
        if destination_width == OperandWidth::QuadWord {
            body.i64_extend_i32_unsigned();
        }
        let value = self.temporaries.take(body, destination_width.value_type());
        body.local_set(value);
        self.write_operand(body, lifted, 0, destination_width, value)
    }

    /// `pmuludq`: the even doubleword lanes multiplied into quadword results,
    /// which lands on the pair's own grain — each half is one 32-by-32 product
    /// — so no vector is assembled at all.
    fn translate_unsigned_wide_multiply(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let destination = self.expect_vector_operand(&lifted.instruction, 0)?;
        let left = self.park_operand_halves(body, lifted, 0)?;
        let right = self.park_operand_halves(body, lifted, 1)?;

        for (slot, half) in [VectorHalf::Low, VectorHalf::High].into_iter().enumerate() {
            for source in [&left, &right] {
                body.local_get(source[slot]);
                body.i64_const(0xffff_ffff);
                body.i64_and();
            }
            body.i64_mul();
            self.state.write_vector(body, destination, half);
        }
        Ok(())
    }

    /// Parks both halves of an operand in locals, so that a translation which
    /// reads them more than once — or writes its destination before it has
    /// finished reading — still sees the values the instruction started with.
    fn park_operand_halves(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        index: u32,
    ) -> Result<[u32; 2]> {
        let mut locals = [0u32; 2];
        for (slot, half) in [VectorHalf::Low, VectorHalf::High].into_iter().enumerate() {
            self.read_vector_half(body, lifted, index, half, UNALIGNED_ACCESS)?;
            let local = self.temporaries.take(body, ValueType::I64);
            body.local_set(local);
            locals[slot] = local;
        }
        Ok(locals)
    }

    /// Interleaves the lanes of two operands, taking one half of each.
    ///
    /// The output's even lanes come from the destination and its odd lanes
    /// from the source, both read out of the same half — the low one for
    /// `punpckl`, the high one for `punpckh`. Sixteen bytes in, sixteen
    /// bytes out, so exactly half of each input is consumed.
    ///
    /// The shuffle below cannot express this at byte or word grain: it works
    /// in doubleword lanes, and these move pieces smaller than one.
    fn translate_interleave(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        lanes: LaneWidth,
        high: bool,
    ) -> Result<()> {
        let destination = self.expect_vector_operand(&lifted.instruction, 0)?;
        let operands = [
            self.park_operand_halves(body, lifted, 0)?,
            self.park_operand_halves(body, lifted, 1)?,
        ];
        let bits = lanes.bits();
        let per_half = 64 / bits;
        let taken = usize::from(high);
        let mask = if bits == 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };

        for (slot, output) in [VectorHalf::Low, VectorHalf::High].into_iter().enumerate() {
            for lane in 0..per_half {
                let position = slot as u32 * per_half + lane;
                // Even from the destination, odd from the source, and each
                // step of two in the output is one step of one in the input.
                let operand = (position % 2) as usize;
                let from = position / 2;

                body.local_get(operands[operand][taken]);
                if from > 0 {
                    body.i64_const(i64::from(from * bits));
                    body.i64_shr_unsigned();
                }
                body.i64_const(mask as i64);
                body.i64_and();
                if lane > 0 {
                    body.i64_const(i64::from(lane * bits));
                    body.i64_shl();
                    body.i64_or();
                }
            }
            self.state.write_vector(body, destination, output);
        }
        Ok(())
    }

    /// Rebuilds all 128 bits of the destination out of doubleword lanes taken
    /// from the two operands — `pshufd` and the rest of the shuffle family.
    fn translate_shuffle(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        lanes: [LaneSource; LANES_PER_REGISTER],
    ) -> Result<()> {
        let destination = self.expect_vector_operand(&lifted.instruction, 0)?;
        let operands = [
            self.park_operand_halves(body, lifted, 0)?,
            self.park_operand_halves(body, lifted, 1)?,
        ];

        for (slot, half) in [VectorHalf::Low, VectorHalf::High].into_iter().enumerate() {
            let low_lane = lanes[2 * slot];
            let high_lane = lanes[2 * slot + 1];
            emit_lane(body, &operands[low_lane.operand], low_lane.lane);
            emit_lane(body, &operands[high_lane.operand], high_lane.lane);
            body.i64_const(32);
            body.i64_shl();
            body.i64_or();
            self.state.write_vector(body, destination, half);
        }
        Ok(())
    }

    /// The bitwise family, which in pair state is two `i64` operations —
    /// including the register-with-itself exclusive-or every compiler uses to
    /// zero a register.
    fn translate_bitwise(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        operation: BitwiseOperation,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let destination = self.expect_vector_operand(instruction, 0)?;
        // The two halves are independent, and each is read — from both
        // operands — before it is written, so a register operated on with
        // itself still sees its own bits.
        for half in [VectorHalf::Low, VectorHalf::High] {
            self.state.read_vector(body, destination, half);
            if operation == BitwiseOperation::AndComplemented {
                // `andn` complements its *destination*, not its source.
                body.i64_const(-1);
                body.i64_xor();
            }
            self.read_vector_half(body, lifted, 1, half, UNALIGNED_ACCESS)?;
            match operation {
                BitwiseOperation::And | BitwiseOperation::AndComplemented => body.i64_and(),
                BitwiseOperation::Or => body.i64_or(),
                BitwiseOperation::Xor => body.i64_xor(),
            }
            self.state.write_vector(body, destination, half);
        }
        Ok(())
    }
}

/// Where one doubleword lane of a shuffle's result comes from: which of the
/// instruction's operands, and which of that operand's four lanes.
///
/// Every lane-rearranging instruction in SSE is some fixed table of these, so
/// they all share one translation: park both operands' halves in locals, then
/// compose the result out of the named lanes.
#[derive(Clone, Copy)]
struct LaneSource {
    operand: usize,
    lane: usize,
}

const LANES_PER_REGISTER: usize = 4;

/// Which operand a lane comes from. The first operand of a two-operand SSE
/// instruction is both a source and the destination, which is why these are
/// named for their role rather than numbered at each use.
const DESTINATION: usize = 0;
const SOURCE: usize = 1;

fn lane_table(entries: [(usize, usize); LANES_PER_REGISTER]) -> [LaneSource; LANES_PER_REGISTER] {
    entries.map(|(operand, lane)| LaneSource { operand, lane })
}

fn half_offset(half: VectorHalf) -> u32 {
    match half {
        VectorHalf::Low => 0,
        VectorHalf::High => HIGH_HALF_OFFSET,
    }
}

/// How wide a lane is in a packed operation. The mnemonic's last letter says
/// which, and it decides the family of wasm SIMD instructions that expresses
/// it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LaneWidth {
    Byte,
    Word,
    DoubleWord,
    QuadWord,
}

impl LaneWidth {
    fn bits(self) -> u32 {
        match self {
            LaneWidth::Byte => 8,
            LaneWidth::Word => 16,
            LaneWidth::DoubleWord => 32,
            LaneWidth::QuadWord => 64,
        }
    }
}

/// A packed operation, named by what it does and the lanes it does it in.
#[derive(Clone, Copy, Debug)]
enum PackedOperation {
    Add(LaneWidth),
    Subtract(LaneWidth),
    Multiply(LaneWidth),
    Equal(LaneWidth),
    GreaterSigned(LaneWidth),
    /// Lane-wise minimum and maximum, which x86 has only at byte and word
    /// grain and only in the signedness each mnemonic names.
    MinimumUnsigned(LaneWidth),
    MinimumSigned(LaneWidth),
    MaximumUnsigned(LaneWidth),
    MaximumSigned(LaneWidth),
    FloatAdd(FloatWidth),
    FloatSubtract(FloatWidth),
    FloatMultiply(FloatWidth),
    FloatDivide(FloatWidth),
    FloatSquareRoot(FloatWidth),
    /// Signed 32-bit lanes widened into floats: `cvtdq2pd` takes the low two
    /// lanes, `cvtdq2ps` all four.
    ConvertFromSignedLanes(FloatWidth),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PackedShift {
    Left,
    RightUnsigned,
    RightSigned,
}

/// A comparison wasm has an instruction for. All of them are false when
/// either operand is NaN, which is what makes "unordered" expressible as the
/// absence of every ordered relation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FloatRelation {
    Less,
    LessOrEqual,
    Equal,
    Greater,
}

/// The predicates the legacy `cmpsd`/`cmpss` encoding can name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FloatPredicate {
    Equal,
    Less,
    LessOrEqual,
    Unordered,
    NotEqual,
    NotLess,
    NotLessOrEqual,
    Ordered,
}

impl FloatPredicate {
    /// The immediate's low three bits, in the architecture's order. Anything
    /// above them belongs to the wider AVX encoding, which is out of scope
    /// and is reported rather than silently truncated.
    fn decode(selector: u8) -> Option<Self> {
        Some(match selector {
            0 => FloatPredicate::Equal,
            1 => FloatPredicate::Less,
            2 => FloatPredicate::LessOrEqual,
            3 => FloatPredicate::Unordered,
            4 => FloatPredicate::NotEqual,
            5 => FloatPredicate::NotLess,
            6 => FloatPredicate::NotLessOrEqual,
            7 => FloatPredicate::Ordered,
            _ => return None,
        })
    }
}

/// Emits the wasm SIMD instruction a packed operation is.
///
/// Where wasm has no instruction for a lane width — there is no eight-bit or
/// sixty-four-bit packed multiply, for instance — this reports rather than
/// reaching for something close.
fn emit_packed(
    body: &mut FunctionBodyBuilder,
    operation: PackedOperation,
    instruction: &Instruction,
) -> Result<()> {
    let unsupported = || {
        anyhow::anyhow!(
            "`{}` has no single wasm instruction for its lane width, and no \
             emulation for it is implemented",
            render(instruction)
        )
    };
    match operation {
        PackedOperation::Add(LaneWidth::Byte) => body.i8x16_add(),
        PackedOperation::Add(LaneWidth::Word) => body.i16x8_add(),
        PackedOperation::Add(LaneWidth::DoubleWord) => body.i32x4_add(),
        PackedOperation::Add(LaneWidth::QuadWord) => body.i64x2_add(),
        PackedOperation::Subtract(LaneWidth::Byte) => body.i8x16_sub(),
        PackedOperation::Subtract(LaneWidth::Word) => body.i16x8_sub(),
        PackedOperation::Subtract(LaneWidth::DoubleWord) => body.i32x4_sub(),
        PackedOperation::Subtract(LaneWidth::QuadWord) => body.i64x2_sub(),
        PackedOperation::Multiply(LaneWidth::Word) => body.i16x8_mul(),
        PackedOperation::Multiply(LaneWidth::DoubleWord) => body.i32x4_mul(),
        PackedOperation::Multiply(LaneWidth::QuadWord) => body.i64x2_mul(),
        PackedOperation::Multiply(LaneWidth::Byte) => return Err(unsupported()),
        PackedOperation::Equal(LaneWidth::Byte) => body.i8x16_equal(),
        PackedOperation::Equal(LaneWidth::Word) => body.i16x8_equal(),
        PackedOperation::Equal(LaneWidth::DoubleWord) => body.i32x4_equal(),
        PackedOperation::Equal(LaneWidth::QuadWord) => body.i64x2_equal(),
        PackedOperation::MinimumUnsigned(LaneWidth::Byte) => body.i8x16_min_unsigned(),
        PackedOperation::MinimumUnsigned(LaneWidth::Word) => body.i16x8_min_unsigned(),
        PackedOperation::MinimumSigned(LaneWidth::Byte) => body.i8x16_min_signed(),
        PackedOperation::MinimumSigned(LaneWidth::Word) => body.i16x8_min_signed(),
        PackedOperation::MaximumUnsigned(LaneWidth::Byte) => body.i8x16_max_unsigned(),
        PackedOperation::MaximumUnsigned(LaneWidth::Word) => body.i16x8_max_unsigned(),
        PackedOperation::MaximumSigned(LaneWidth::Byte) => body.i8x16_max_signed(),
        PackedOperation::MaximumSigned(LaneWidth::Word) => body.i16x8_max_signed(),
        PackedOperation::GreaterSigned(LaneWidth::Byte) => body.i8x16_greater_signed(),
        PackedOperation::GreaterSigned(LaneWidth::Word) => body.i16x8_greater_signed(),
        PackedOperation::GreaterSigned(LaneWidth::DoubleWord) => body.i32x4_greater_signed(),
        PackedOperation::GreaterSigned(LaneWidth::QuadWord) => body.i64x2_greater_signed(),
        PackedOperation::FloatAdd(Single) => body.f32x4_add(),
        PackedOperation::FloatAdd(Double) => body.f64x2_add(),
        PackedOperation::FloatSubtract(Single) => body.f32x4_sub(),
        PackedOperation::FloatSubtract(Double) => body.f64x2_sub(),
        PackedOperation::FloatMultiply(Single) => body.f32x4_mul(),
        PackedOperation::FloatMultiply(Double) => body.f64x2_mul(),
        PackedOperation::FloatDivide(Single) => body.f32x4_div(),
        PackedOperation::FloatDivide(Double) => body.f64x2_div(),
        PackedOperation::FloatSquareRoot(Single) => body.f32x4_sqrt(),
        PackedOperation::FloatSquareRoot(Double) => body.f64x2_sqrt(),
        PackedOperation::ConvertFromSignedLanes(Single) => body.f32x4_convert_i32x4_signed(),
        PackedOperation::ConvertFromSignedLanes(Double) => body.f64x2_convert_low_i32x4_signed(),
        // x86 has the extrema only at byte and word grain, and only in the
        // signedness each mnemonic names — there is no `pminuw` before
        // SSE4.1 and no `pminub` for doublewords at all. Nothing should ask
        // for the rest, and if something does it says so.
        PackedOperation::MinimumUnsigned(lanes)
        | PackedOperation::MinimumSigned(lanes)
        | PackedOperation::MaximumUnsigned(lanes)
        | PackedOperation::MaximumSigned(lanes) => bail!(
            "`{}` asks for a lane-wise extremum at {} bits, which x86 has no \
             instruction for",
            crate::translate::render(instruction),
            lanes.bits()
        ),
    }
    Ok(())
}

/// Pushes one half of the result of shifting a 128-bit value by whole bytes.
///
/// Bits crossing the halfway point move between the two globals, which is the
/// one place the pair representation costs something the single-register form
/// would not.
fn emit_shifted_half(
    body: &mut FunctionBodyBuilder,
    halves: &[u32; 2],
    slot: usize,
    bytes: usize,
    direction: PackedShift,
) {
    // Each half of the result is a 64-bit window onto the original value: a
    // right shift slides the window up, a left shift slides it down, and both
    // are the same arithmetic once the offset is allowed to be negative. A
    // window that falls off either end is zero, which is what these
    // instructions fill with.
    let distance = (bytes * 8) as i64;
    let start = 64 * slot as i64
        + match direction {
            PackedShift::Left => -distance,
            _ => distance,
        };
    let whole = start.div_euclid(64);
    let within = start.rem_euclid(64);
    let half = |index: i64| (0..2).contains(&index).then(|| halves[index as usize]);

    if within == 0 {
        match half(whole) {
            Some(local) => body.local_get(local),
            None => body.i64_const(0),
        }
        return;
    }

    let (lower, upper) = (half(whole), half(whole + 1));
    if lower.is_none() && upper.is_none() {
        body.i64_const(0);
        return;
    }
    if let Some(local) = lower {
        body.local_get(local);
        body.i64_const(within);
        body.i64_shr_unsigned();
    }
    if let Some(local) = upper {
        body.local_get(local);
        body.i64_const(64 - within);
        body.i64_shl();
        if lower.is_some() {
            body.i64_or();
        }
    }
}

fn emit_float_relation(body: &mut FunctionBodyBuilder, width: FloatWidth, relation: FloatRelation) {
    match (width, relation) {
        (Single, FloatRelation::Less) => body.f32_lt(),
        (Single, FloatRelation::LessOrEqual) => body.f32_le(),
        (Single, FloatRelation::Equal) => body.f32_eq(),
        (Single, FloatRelation::Greater) => body.f32_gt(),
        (Double, FloatRelation::Less) => body.f64_lt(),
        (Double, FloatRelation::LessOrEqual) => body.f64_le(),
        (Double, FloatRelation::Equal) => body.f64_eq(),
        (Double, FloatRelation::Greater) => body.f64_gt(),
    }
}

/// The lane-wise counterpart of [`emit_float_relation`]. Wasm's packed
/// comparisons already produce all-ones and all-zero lanes, which is the same
/// answer SSE gives.
fn emit_packed_relation(
    body: &mut FunctionBodyBuilder,
    width: FloatWidth,
    relation: FloatRelation,
) {
    match (width, relation) {
        (Single, FloatRelation::Less) => body.f32x4_less(),
        (Single, FloatRelation::LessOrEqual) => body.f32x4_less_or_equal(),
        (Single, FloatRelation::Equal) => body.f32x4_equal(),
        (Single, FloatRelation::Greater) => body.f32x4_greater(),
        (Double, FloatRelation::Less) => body.f64x2_less(),
        (Double, FloatRelation::LessOrEqual) => body.f64x2_less_or_equal(),
        (Double, FloatRelation::Equal) => body.f64x2_equal(),
        (Double, FloatRelation::Greater) => body.f64x2_greater(),
    }
}

/// Pushes `1` when two parked values are *ordered* — that is, when neither is
/// NaN. Exactly one of less, equal and greater holds for an ordered pair, and
/// none of them holds otherwise.
fn emit_ordered(body: &mut FunctionBodyBuilder, width: FloatWidth, first: u32, second: u32) {
    for (index, relation) in [
        FloatRelation::Less,
        FloatRelation::Equal,
        FloatRelation::Greater,
    ]
    .into_iter()
    .enumerate()
    {
        body.local_get(first);
        body.local_get(second);
        emit_float_relation(body, width, relation);
        if index > 0 {
            body.i32_or();
        }
    }
}

/// Which end of a destination width's range a bounds check is against.
#[derive(Clone, Copy)]
enum Bound {
    /// The most negative value the destination can hold.
    Lowest,
    /// One past the largest, which is a power of two and so exact where the
    /// largest itself is not.
    PastHighest,
}

fn reinterpret_as_float(body: &mut FunctionBodyBuilder, width: FloatWidth) {
    match width {
        Single => body.f32_reinterpret_i32(),
        Double => body.f64_reinterpret_i64(),
    }
}

fn reinterpret_as_integer(body: &mut FunctionBodyBuilder, width: FloatWidth) {
    match width {
        Single => body.i32_reinterpret_f32(),
        Double => body.i64_reinterpret_f64(),
    }
}

/// The *integer indefinite*: what x86 produces when a truncating conversion
/// has no representable answer, which is the most negative value of the
/// destination width.
fn emit_integer_indefinite(body: &mut FunctionBodyBuilder, destination: OperandWidth) {
    match destination {
        OperandWidth::QuadWord => body.i64_const(i64::MIN),
        _ => body.i32_const(i32::MIN),
    }
}

/// Pushes one end of a destination width's range as a float of the source's
/// width. Both ends are powers of two, so both are exact in either width.
fn emit_float_bound(
    body: &mut FunctionBodyBuilder,
    width: FloatWidth,
    destination: OperandWidth,
    bound: Bound,
) {
    let magnitude = match destination {
        OperandWidth::QuadWord => 9_223_372_036_854_775_808.0f64,
        _ => 2_147_483_648.0f64,
    };
    let value = match bound {
        Bound::Lowest => -magnitude,
        Bound::PastHighest => magnitude,
    };
    match width {
        Single => body.f32_const_bits((value as f32).to_bits()),
        Double => body.f64_const_bits(value.to_bits()),
    }
}

/// Pushes one doubleword lane of a parked operand, zero-extended into an
/// `i64`. Lanes zero and one live in the low half, two and three in the high.
fn emit_lane(body: &mut FunctionBodyBuilder, halves: &[u32; 2], lane: usize) {
    body.local_get(halves[lane / 2]);
    let shift = 32 * (lane % 2) as i64;
    if shift != 0 {
        body.i64_const(shift);
        body.i64_shr_unsigned();
    }
    body.i64_const(0xffff_ffff);
    body.i64_and();
}
