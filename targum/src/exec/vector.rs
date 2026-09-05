//! SSE: the vector register file, and what the packed and scalar
//! instructions do to it.
//!
//! A vector here is a number: an XMM register is a `u128`, lanes are
//! shifts, and the whole packed family is one loop taking a closure.
//!
//! The machine is a baseline x86-64 with SSE2 and a little of SSE3/SSE4
//! where a libc reaches for it — the same machine `cpuid` reports,
//! deliberately, so that a libc cannot select a path nothing here
//! implements.

use iced_x86::{Instruction, Mnemonic, OpKind, Register};

use crate::flags::bit;
use crate::state::Width;

use super::{Cpu, Step, Trap, Unsupported};

/// How wide the lanes of a packed operation are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lane {
    Byte,
    Word,
    Dword,
    Qword,
}

impl Lane {
    const fn bits(self) -> u32 {
        match self {
            Lane::Byte => 8,
            Lane::Word => 16,
            Lane::Dword => 32,
            Lane::Qword => 64,
        }
    }

    const fn count(self) -> u32 {
        128 / self.bits()
    }

    const fn mask(self) -> u64 {
        match self {
            Lane::Qword => u64::MAX,
            lane => (1u64 << lane.bits()) - 1,
        }
    }

    const fn sign_bit(self) -> u64 {
        1u64 << (self.bits() - 1)
    }

    const fn signed(self, value: u64) -> i64 {
        let shift = 64 - self.bits();
        ((value << shift) as i64) >> shift
    }
}

/// Applies `operation` to matching lanes of two vectors.
///
/// One loop for the whole packed family. The lane count is a compile-time
/// constant of the width and the closure is inlined, so what this costs
/// against sixteen hand-written arms is nothing that has shown up in a
/// measurement — and what it buys is that `paddb` and `psubq` cannot
/// disagree about what a lane is.
fn packed(left: u128, right: u128, lane: Lane, operation: impl Fn(u64, u64) -> u64) -> u128 {
    let bits = lane.bits();
    let mask = lane.mask();
    let mut result = 0u128;
    for index in 0..lane.count() {
        let shift = index * bits;
        let a = ((left >> shift) as u64) & mask;
        let b = ((right >> shift) as u64) & mask;
        result |= u128::from(operation(a, b) & mask) << shift;
    }
    result
}

fn packed_f64(left: u128, right: u128, operation: impl Fn(f64, f64) -> f64) -> u128 {
    packed(left, right, Lane::Qword, |a, b| {
        operation(f64::from_bits(a), f64::from_bits(b)).to_bits()
    })
}

fn packed_f32(left: u128, right: u128, operation: impl Fn(f32, f32) -> f32) -> u128 {
    packed(left, right, Lane::Dword, |a, b| {
        u64::from(operation(f32::from_bits(a as u32), f32::from_bits(b as u32)).to_bits())
    })
}

/// The NaN an invalid operation produces: the *real indefinite*.
///
/// Its sign bit is set, and Rust's own arithmetic produces the positive one
/// — `0.0 / 0.0` in Rust is `0x7fc00000`, and on this machine `divss` of the
/// same operands answers `0xffc00000`. A guest that prints a NaN, or hashes
/// one, or compares its bits, sees the difference. Measured 2026-08-30 for
/// `div`, `sub`, `mul` and `sqrt` alike.
const INDEFINITE_SINGLE: f32 = f32::from_bits(0xffc0_0000);
const INDEFINITE_DOUBLE: f64 = f64::from_bits(0xfff8_0000_0000_0000);

/// Sets a NaN's quiet bit, keeping its payload.
fn quiet_single(value: f32) -> f32 {
    f32::from_bits(value.to_bits() | 0x0040_0000)
}

fn quiet_double(value: f64) -> f64 {
    f64::from_bits(value.to_bits() | 0x0008_0000_0000_0000)
}

/// The arithmetic family's NaN rule, which is x86's and not the one Rust's
/// `+` happens to implement.
///
/// **A NaN operand is propagated by the destination first**: `addss` with
/// NaNs in both operands answers the *destination's*, quieted, payload and
/// all — and the source's only when the destination is not a NaN. Leaving
/// this to Rust looks like it works, because `a + b` compiles to `addss`
/// with the operands in that order; it is not sound, because LLVM's `fadd`
/// leaves NaN payloads unspecified and is free to commute its operands. The
/// lockstep oracle found it on the first argument pair that put a different
/// NaN in each.
fn arithmetic_single(left: f32, right: f32, operation: impl Fn(f32, f32) -> f32) -> f32 {
    if left.is_nan() {
        return quiet_single(left);
    }
    if right.is_nan() {
        return quiet_single(right);
    }
    let result = operation(left, right);
    match result.is_nan() {
        true => INDEFINITE_SINGLE,
        false => result,
    }
}

fn arithmetic_double(left: f64, right: f64, operation: impl Fn(f64, f64) -> f64) -> f64 {
    if left.is_nan() {
        return quiet_double(left);
    }
    if right.is_nan() {
        return quiet_double(right);
    }
    let result = operation(left, right);
    match result.is_nan() {
        true => INDEFINITE_DOUBLE,
        false => result,
    }
}

/// The same for the one-operand family, where there is no destination to
/// prefer: the source's NaN is quieted, and an invalid operation — the
/// square root of a negative — is the indefinite.
fn unary_single(value: f32, operation: impl Fn(f32) -> f32) -> f32 {
    if value.is_nan() {
        return quiet_single(value);
    }
    let result = operation(value);
    match result.is_nan() {
        true => INDEFINITE_SINGLE,
        false => result,
    }
}

fn unary_double(value: f64, operation: impl Fn(f64) -> f64) -> f64 {
    if value.is_nan() {
        return quiet_double(value);
    }
    let result = operation(value);
    match result.is_nan() {
        true => INDEFINITE_DOUBLE,
        false => result,
    }
}

/// x86's minimum and maximum, which are not `f64::min` and `f64::max`.
///
/// The architecture defines them as "if the comparison holds take the first
/// operand, otherwise take the second", which decides the two cases the
/// IEEE-flavoured functions decide differently: with a NaN anywhere the
/// answer is the *second* operand, and with two zeros of opposite sign the
/// answer is the second operand too. Compilers rely on both — the NaN rule
/// is how `fmin` is open-coded — so getting it merely reasonable is a
/// divergence a real program finds.
fn minimum(left: f64, right: f64) -> f64 {
    match left < right {
        true => left,
        false => right,
    }
}

fn maximum(left: f64, right: f64) -> f64 {
    match left > right {
        true => left,
        false => right,
    }
}

fn single_minimum(left: f32, right: f32) -> f32 {
    match left < right {
        true => left,
        false => right,
    }
}

fn single_maximum(left: f32, right: f32) -> f32 {
    match left > right {
        true => left,
        false => right,
    }
}

/// The value an out-of-range or NaN conversion produces: "integer
/// indefinite", which is the most negative value at the destination width.
fn indefinite(width: Width) -> u64 {
    width.sign_bit()
}

fn to_integer(value: f64, width: Width) -> u64 {
    let truncated = value.trunc();
    let representable = match width {
        Width::Qword => truncated >= -(2f64.powi(63)) && truncated < 2f64.powi(63),
        _ => truncated >= -(2f64.powi(31)) && truncated < 2f64.powi(31),
    };
    match value.is_nan() || !representable {
        true => indefinite(width),
        false => width.truncate(truncated as i64 as u64),
    }
}

/// What a mask-producing compare answers per lane: all ones or all zeros.
fn mask_of(holds: bool, lane: Lane) -> u64 {
    match holds {
        true => lane.mask(),
        false => 0,
    }
}

/// The eight predicates `cmpps`/`cmpsd` and friends select with their
/// immediate.
fn float_predicate(code: u8, left: f64, right: f64) -> bool {
    match code & 7 {
        0 => left == right,
        1 => left < right,
        2 => left <= right,
        3 => left.is_nan() || right.is_nan(),
        4 => !(left == right),
        5 => !(left < right),
        6 => !(left <= right),
        _ => !(left.is_nan() || right.is_nan()),
    }
}

impl Cpu<'_> {
    fn vector_register(instruction: &Instruction, operand: u32) -> Result<usize, Trap> {
        let register = instruction.op_register(operand);
        match register.is_xmm() {
            true => Ok(register.number()),
            false => Err(Trap::Unsupported(Unsupported::at(
                instruction,
                Some("a vector operand naming a register outside XMM0-15"),
            ))),
        }
    }

    pub(super) fn vector(&self, number: usize) -> u128 {
        let halves = self.tcb.vectors[number];
        u128::from(halves[0]) | (u128::from(halves[1]) << 64)
    }

    pub(super) fn set_vector(&mut self, number: usize, value: u128) {
        self.tcb.vectors[number] = [value as u64, (value >> 64) as u64];
    }

    /// Reads `bytes` bytes of a vector operand, zero-extended into a `u128`.
    ///
    /// A register source is read from the register file; a memory source is
    /// read through [`crate::space::Space`] like every other access, so the
    /// permission check and the code-page test apply to vector loads exactly
    /// as they do to integer ones.
    fn read_vector(
        &mut self,
        instruction: &Instruction,
        operand: u32,
        bytes: usize,
        aligned: bool,
    ) -> Result<u128, Trap> {
        match instruction.op_kind(operand) {
            OpKind::Register => {
                let register = instruction.op_register(operand);
                match register.is_xmm() {
                    true => Ok(self.vector(register.number())),
                    // `movd`/`movq` and the conversions take a
                    // general-purpose source.
                    false => {
                        let slice = Self::slice(instruction, register)?;
                        Ok(u128::from(self.tcb.read_register(slice)))
                    }
                }
            }
            OpKind::Memory => {
                let at = self.address(instruction)?;
                self.check_alignment(instruction, at, aligned)?;
                let mut buffer = [0u8; 16];
                self.space.read(at, &mut buffer[..bytes])?;
                Ok(u128::from_le_bytes(buffer))
            }
            _ => Err(Trap::Unsupported(Unsupported::at(
                instruction,
                Some("a vector operand kind with no read path"),
            ))),
        }
    }

    fn write_vector_operand(
        &mut self,
        instruction: &Instruction,
        operand: u32,
        bytes: usize,
        aligned: bool,
        value: u128,
    ) -> Result<Step, Trap> {
        match instruction.op_kind(operand) {
            OpKind::Register => {
                let register = instruction.op_register(operand);
                match register.is_xmm() {
                    true => {
                        self.set_vector(register.number(), value);
                        Ok(Step::Retired)
                    }
                    false => {
                        let slice = Self::slice(instruction, register)?;
                        self.tcb.write_register(slice, value as u64);
                        Ok(Step::Retired)
                    }
                }
            }
            OpKind::Memory => {
                let at = self.address(instruction)?;
                self.check_alignment(instruction, at, aligned)?;
                let buffer = value.to_le_bytes();
                self.space.write(at, &buffer[..bytes])?;
                Ok(Step::Retired)
            }
            _ => Err(Trap::Unsupported(Unsupported::at(
                instruction,
                Some("a vector operand kind with no write path"),
            ))),
        }
    }

    /// The aligned moves fault on an address that is not sixteen-byte
    /// aligned, and it is a real fault the guest can catch — `movaps` on a
    /// misaligned pointer is a general-protection exception, which reaches
    /// userspace as `SIGSEGV`. Ignoring the requirement would make a class
    /// of guest bug invisible here and fatal on real hardware.
    fn check_alignment(
        &self,
        instruction: &Instruction,
        address: u64,
        aligned: bool,
    ) -> Result<(), Trap> {
        match aligned && address % 16 != 0 {
            true => Err(Trap::Misaligned {
                address: instruction.ip(),
            }),
            false => Ok(()),
        }
    }

    /// Executes a vector instruction, or reports that it is not one.
    ///
    /// The `Option` rather than a bool-and-out-parameter because the caller
    /// chains: integer arms, then this, then the x87, then the loud error.
    pub(super) fn vector_step(
        &mut self,
        instruction: &Instruction,
    ) -> Result<Option<Step>, Trap> {
        const UNALIGNED: bool = false;
        const ALIGNED: bool = true;
        let step = match instruction.mnemonic() {
            // ---- moves ----
            Mnemonic::Movd | Mnemonic::Movq => {
                let bytes = match instruction.mnemonic() {
                    Mnemonic::Movd => 4,
                    _ => 8,
                };
                match instruction.op0_kind() == OpKind::Register
                    && instruction.op_register(0).is_xmm()
                {
                    // Into a vector register, which zeroes everything above
                    // what was moved — including for the register-to-register
                    // form, which is the whole reason `movq %xmm1, %xmm0` is
                    // a way to zero the high half.
                    true => {
                        let value = self.read_vector(instruction, 1, bytes, UNALIGNED)?;
                        let mask = match bytes {
                            4 => u128::from(u32::MAX),
                            _ => u128::from(u64::MAX),
                        };
                        let number = Self::vector_register(instruction, 0)?;
                        self.set_vector(number, value & mask);
                        Step::Retired
                    }
                    false => {
                        let value = self.read_vector(instruction, 1, bytes, UNALIGNED)?;
                        self.write_vector_operand(instruction, 0, bytes, UNALIGNED, value)?
                    }
                }
            }
            Mnemonic::Movss | Mnemonic::Movsd => {
                let bytes = match instruction.mnemonic() {
                    Mnemonic::Movss => 4,
                    _ => 8,
                };
                let mask = match bytes {
                    4 => u128::from(u32::MAX),
                    _ => u128::from(u64::MAX),
                };
                let value = self.read_vector(instruction, 1, bytes, UNALIGNED)?;
                match instruction.op0_kind() {
                    OpKind::Register if instruction.op_register(0).is_xmm() => {
                        let number = Self::vector_register(instruction, 0)?;
                        // Register to register keeps the rest of the
                        // destination; loading from memory zeroes it. The
                        // asymmetry is the architecture's, and code depends
                        // on both halves of it.
                        let kept = match instruction.op1_kind() {
                            OpKind::Register => self.vector(number) & !mask,
                            _ => 0,
                        };
                        self.set_vector(number, kept | (value & mask));
                        Step::Retired
                    }
                    _ => self.write_vector_operand(instruction, 0, bytes, UNALIGNED, value)?,
                }
            }
            Mnemonic::Movaps | Mnemonic::Movapd | Mnemonic::Movdqa | Mnemonic::Movntdq
            | Mnemonic::Movntps | Mnemonic::Movntpd | Mnemonic::Movntdqa => {
                let value = self.read_vector(instruction, 1, 16, ALIGNED)?;
                self.write_vector_operand(instruction, 0, 16, ALIGNED, value)?
            }
            Mnemonic::Movups | Mnemonic::Movupd | Mnemonic::Movdqu => {
                let value = self.read_vector(instruction, 1, 16, UNALIGNED)?;
                self.write_vector_operand(instruction, 0, 16, UNALIGNED, value)?
            }
            Mnemonic::Movlps | Mnemonic::Movlpd => match instruction.op0_kind() {
                OpKind::Register => {
                    let number = Self::vector_register(instruction, 0)?;
                    let value = self.read_vector(instruction, 1, 8, UNALIGNED)?;
                    let kept = self.vector(number) & !u128::from(u64::MAX);
                    self.set_vector(number, kept | (value & u128::from(u64::MAX)));
                    Step::Retired
                }
                _ => {
                    let value = self.read_vector(instruction, 1, 8, UNALIGNED)?;
                    self.write_vector_operand(instruction, 0, 8, UNALIGNED, value)?
                }
            },
            Mnemonic::Movhps | Mnemonic::Movhpd => match instruction.op0_kind() {
                OpKind::Register => {
                    let number = Self::vector_register(instruction, 0)?;
                    let value = self.read_vector(instruction, 1, 8, UNALIGNED)?;
                    let kept = self.vector(number) & u128::from(u64::MAX);
                    self.set_vector(number, kept | ((value & u128::from(u64::MAX)) << 64));
                    Step::Retired
                }
                _ => {
                    let source = Self::vector_register(instruction, 1)?;
                    let value = self.vector(source) >> 64;
                    self.write_vector_operand(instruction, 0, 8, UNALIGNED, value)?
                }
            },
            // SSE3's movers, which duplicate a lane rather than moving it.
            // What reaches them is vectorised numerics: a complex multiply
            // needs the real part in both halves, and `movddup` is how it
            // gets there.
            Mnemonic::Movddup => {
                let source = self.read_vector(instruction, 1, 8, UNALIGNED)?;
                let low = source & u128::from(u64::MAX);
                let number = Self::vector_register(instruction, 0)?;
                self.set_vector(number, low | (low << 64));
                Step::Retired
            }
            Mnemonic::Movshdup | Mnemonic::Movsldup => {
                let source = self.read_vector(instruction, 1, 16, UNALIGNED)?;
                // The odd lanes duplicated downwards, or the even lanes
                // upwards.
                let odd = instruction.mnemonic() == Mnemonic::Movshdup;
                let mut result = 0u128;
                for index in 0..4u32 {
                    let from = match odd {
                        true => index | 1,
                        false => index & !1,
                    };
                    let lane = (source >> (from * 32)) & u128::from(u32::MAX);
                    result |= lane << (index * 32);
                }
                let number = Self::vector_register(instruction, 0)?;
                self.set_vector(number, result);
                Step::Retired
            }
            // `lddqu` is `movdqu` with a hint about how to fetch it, and the
            // hint is about a store-forwarding stall on hardware that has a
            // store buffer. Nothing here does.
            Mnemonic::Lddqu => {
                let value = self.read_vector(instruction, 1, 16, UNALIGNED)?;
                self.write_vector_operand(instruction, 0, 16, UNALIGNED, value)?
            }
            // The horizontal family: pairs *within* each operand, which is
            // how a dot product ends.
            Mnemonic::Haddpd | Mnemonic::Hsubpd => {
                let subtract = instruction.mnemonic() == Mnemonic::Hsubpd;
                let (number, left, right) = self.vector_operands(instruction)?;
                let fold = |value: u128| {
                    let low = f64::from_bits(value as u64);
                    let high = f64::from_bits((value >> 64) as u64);
                    arithmetic_double(low, high, |a, b| if subtract { a - b } else { a + b })
                        .to_bits()
                };
                self.set_vector(
                    number,
                    u128::from(fold(left)) | (u128::from(fold(right)) << 64),
                );
                Step::Retired
            }
            Mnemonic::Haddps | Mnemonic::Hsubps => {
                let subtract = instruction.mnemonic() == Mnemonic::Hsubps;
                let (number, left, right) = self.vector_operands(instruction)?;
                let lane = |value: u128, index: u32| f32::from_bits((value >> (index * 32)) as u32);
                let fold = |value: u128, index: u32| {
                    arithmetic_single(lane(value, index), lane(value, index + 1), |a, b| {
                        if subtract { a - b } else { a + b }
                    })
                    .to_bits()
                };
                let result = u128::from(fold(left, 0))
                    | (u128::from(fold(left, 2)) << 32)
                    | (u128::from(fold(right, 0)) << 64)
                    | (u128::from(fold(right, 2)) << 96);
                self.set_vector(number, result);
                Step::Retired
            }
            // Subtract the even lanes, add the odd ones — a complex
            // multiply in one instruction.
            Mnemonic::Addsubpd => {
                let (number, left, right) = self.vector_operands(instruction)?;
                let low = arithmetic_double(
                    f64::from_bits(left as u64),
                    f64::from_bits(right as u64),
                    |a, b| a - b,
                );
                let high = arithmetic_double(
                    f64::from_bits((left >> 64) as u64),
                    f64::from_bits((right >> 64) as u64),
                    |a, b| a + b,
                );
                self.set_vector(
                    number,
                    u128::from(low.to_bits()) | (u128::from(high.to_bits()) << 64),
                );
                Step::Retired
            }
            Mnemonic::Addsubps => {
                let (number, left, right) = self.vector_operands(instruction)?;
                let mut result = 0u128;
                for index in 0..4u32 {
                    let a = f32::from_bits((left >> (index * 32)) as u32);
                    let b = f32::from_bits((right >> (index * 32)) as u32);
                    let lane = match index % 2 == 0 {
                        true => arithmetic_single(a, b, |a, b| a - b),
                        false => arithmetic_single(a, b, |a, b| a + b),
                    };
                    result |= u128::from(lane.to_bits()) << (index * 32);
                }
                self.set_vector(number, result);
                Step::Retired
            }

            Mnemonic::Movhlps => {
                let destination = Self::vector_register(instruction, 0)?;
                let source = Self::vector_register(instruction, 1)?;
                let kept = self.vector(destination) & !u128::from(u64::MAX);
                let moved = self.vector(source) >> 64;
                self.set_vector(destination, kept | moved);
                Step::Retired
            }
            Mnemonic::Movlhps => {
                let destination = Self::vector_register(instruction, 0)?;
                let source = Self::vector_register(instruction, 1)?;
                let kept = self.vector(destination) & u128::from(u64::MAX);
                let moved = (self.vector(source) & u128::from(u64::MAX)) << 64;
                self.set_vector(destination, kept | moved);
                Step::Retired
            }

            // ---- bitwise, which have no lanes at all ----
            Mnemonic::Pand | Mnemonic::Andps | Mnemonic::Andpd => {
                self.vector_binary(instruction, |left, right| left & right)?
            }
            Mnemonic::Pandn | Mnemonic::Andnps | Mnemonic::Andnpd => {
                self.vector_binary(instruction, |left, right| !left & right)?
            }
            Mnemonic::Por | Mnemonic::Orps | Mnemonic::Orpd => {
                self.vector_binary(instruction, |left, right| left | right)?
            }
            Mnemonic::Pxor | Mnemonic::Xorps | Mnemonic::Xorpd => {
                self.vector_binary(instruction, |left, right| left ^ right)?
            }

            // ---- scalar floating point ----
            Mnemonic::Addsd => {
                self.scalar_f64(instruction, |a, b| arithmetic_double(a, b, |a, b| a + b))?
            }
            Mnemonic::Subsd => {
                self.scalar_f64(instruction, |a, b| arithmetic_double(a, b, |a, b| a - b))?
            }
            Mnemonic::Mulsd => {
                self.scalar_f64(instruction, |a, b| arithmetic_double(a, b, |a, b| a * b))?
            }
            Mnemonic::Divsd => {
                self.scalar_f64(instruction, |a, b| arithmetic_double(a, b, |a, b| a / b))?
            }
            // Minimum and maximum are *not* in the arithmetic family: with
            // an unordered pair they answer the second operand unchanged,
            // signalling NaN and all, which is why they take the raw rule.
            Mnemonic::Minsd => self.scalar_f64(instruction, minimum)?,
            Mnemonic::Maxsd => self.scalar_f64(instruction, maximum)?,
            Mnemonic::Sqrtsd => {
                self.scalar_f64(instruction, |_, b| unary_double(b, f64::sqrt))?
            }
            Mnemonic::Addss => {
                self.scalar_f32(instruction, |a, b| arithmetic_single(a, b, |a, b| a + b))?
            }
            Mnemonic::Subss => {
                self.scalar_f32(instruction, |a, b| arithmetic_single(a, b, |a, b| a - b))?
            }
            Mnemonic::Mulss => {
                self.scalar_f32(instruction, |a, b| arithmetic_single(a, b, |a, b| a * b))?
            }
            Mnemonic::Divss => {
                self.scalar_f32(instruction, |a, b| arithmetic_single(a, b, |a, b| a / b))?
            }
            Mnemonic::Minss => self.scalar_f32(instruction, single_minimum)?,
            Mnemonic::Maxss => self.scalar_f32(instruction, single_maximum)?,
            Mnemonic::Sqrtss => {
                self.scalar_f32(instruction, |_, b| unary_single(b, f32::sqrt))?
            }

            // ---- the compares that write the integer flags ----
            //
            // The only floating-point instructions that do. Parity is the
            // unordered answer, which is why the flag exists in the model at
            // all: a compiler emits `jp` immediately after one of these.
            Mnemonic::Ucomisd | Mnemonic::Comisd | Mnemonic::Ucomiss | Mnemonic::Comiss => {
                let single = matches!(
                    instruction.mnemonic(),
                    Mnemonic::Ucomiss | Mnemonic::Comiss
                );
                let bytes = if single { 4 } else { 8 };
                let left = self.read_vector(instruction, 0, bytes, UNALIGNED)?;
                let right = self.read_vector(instruction, 1, bytes, UNALIGNED)?;
                let (left, right) = match single {
                    true => (
                        f64::from(f32::from_bits(left as u32)),
                        f64::from(f32::from_bits(right as u32)),
                    ),
                    false => (f64::from_bits(left as u64), f64::from_bits(right as u64)),
                };
                let status = if left.is_nan() || right.is_nan() {
                    bit::ZERO | bit::PARITY | bit::CARRY
                } else if left < right {
                    bit::CARRY
                } else if left == right {
                    bit::ZERO
                } else {
                    0
                };
                // Sign, overflow and adjust are cleared, not preserved.
                self.tcb.flags.set_all(status);
                Step::Retired
            }
            Mnemonic::Cmpsd | Mnemonic::Cmpss => {
                let single = instruction.mnemonic() == Mnemonic::Cmpss;
                let bytes = if single { 4 } else { 8 };
                let code = instruction.immediate8();
                let number = Self::vector_register(instruction, 0)?;
                let left = self.vector(number);
                let right = self.read_vector(instruction, 1, bytes, UNALIGNED)?;
                let holds = match single {
                    true => float_predicate(
                        code,
                        f64::from(f32::from_bits(left as u32)),
                        f64::from(f32::from_bits(right as u32)),
                    ),
                    false => float_predicate(
                        code,
                        f64::from_bits(left as u64),
                        f64::from_bits(right as u64),
                    ),
                };
                let (lane, mask) = match single {
                    true => (Lane::Dword, u128::from(u32::MAX)),
                    false => (Lane::Qword, u128::from(u64::MAX)),
                };
                let answer = u128::from(mask_of(holds, lane));
                self.set_vector(number, (left & !mask) | answer);
                Step::Retired
            }

            // ---- conversions ----
            Mnemonic::Cvttsd2si | Mnemonic::Cvttss2si | Mnemonic::Cvtsd2si | Mnemonic::Cvtss2si => {
                let single = matches!(
                    instruction.mnemonic(),
                    Mnemonic::Cvttss2si | Mnemonic::Cvtss2si
                );
                let truncating = matches!(
                    instruction.mnemonic(),
                    Mnemonic::Cvttsd2si | Mnemonic::Cvttss2si
                );
                let bytes = if single { 4 } else { 8 };
                let source = self.read_vector(instruction, 1, bytes, UNALIGNED)?;
                let value = match single {
                    true => f64::from(f32::from_bits(source as u32)),
                    false => f64::from_bits(source as u64),
                };
                // The non-truncating forms round to nearest, which is the
                // only rounding mode this machine is ever in: `ldmxcsr` is
                // accepted and remembered, and nothing in the corpus changes
                // it. A guest that does gets the loud error below rather
                // than a quiet wrong answer.
                let value = match truncating {
                    true => value,
                    false => {
                        if self.tcb.mxcsr & 0x6000 != 0 {
                            return Err(Trap::Unsupported(Unsupported::at(
                                instruction,
                                Some("a rounding mode other than nearest"),
                            )));
                        }
                        round_to_nearest_even(value)
                    }
                };
                let width = Self::width(instruction, 0)?;
                let converted = to_integer(value, width);
                Some(self.write(instruction, 0, width, converted)?)
                    .expect("a write reports retirement")
            }
            Mnemonic::Cvtsi2sd | Mnemonic::Cvtsi2ss => {
                let width = Self::width(instruction, 1)?;
                let source = self.read(instruction, 1, width)?;
                let value = width.sign_extend(source) as i64 as f64;
                let number = Self::vector_register(instruction, 0)?;
                let kept = self.vector(number);
                match instruction.mnemonic() {
                    Mnemonic::Cvtsi2sd => {
                        let bits = u128::from(value.to_bits());
                        self.set_vector(number, (kept & !u128::from(u64::MAX)) | bits);
                    }
                    _ => {
                        let bits = u128::from((value as f32).to_bits());
                        self.set_vector(number, (kept & !u128::from(u32::MAX)) | bits);
                    }
                }
                Step::Retired
            }
            Mnemonic::Cvtsd2ss => {
                let source = self.read_vector(instruction, 1, 8, UNALIGNED)?;
                let value = f64::from_bits(source as u64) as f32;
                let number = Self::vector_register(instruction, 0)?;
                let kept = self.vector(number) & !u128::from(u32::MAX);
                self.set_vector(number, kept | u128::from(value.to_bits()));
                Step::Retired
            }
            Mnemonic::Cvtss2sd => {
                let source = self.read_vector(instruction, 1, 4, UNALIGNED)?;
                let value = f64::from(f32::from_bits(source as u32));
                let number = Self::vector_register(instruction, 0)?;
                let kept = self.vector(number) & !u128::from(u64::MAX);
                self.set_vector(number, kept | u128::from(value.to_bits()));
                Step::Retired
            }
            Mnemonic::Cvtdq2ps => {
                let source = self.read_vector(instruction, 1, 16, UNALIGNED)?;
                let value = packed(source, 0, Lane::Dword, |lane, _| {
                    u64::from(((lane as u32) as i32 as f32).to_bits())
                });
                let number = Self::vector_register(instruction, 0)?;
                self.set_vector(number, value);
                Step::Retired
            }
            Mnemonic::Cvtdq2pd => {
                let source = self.read_vector(instruction, 1, 8, UNALIGNED)?;
                let low = f64::from((source as u32) as i32);
                let high = f64::from(((source >> 32) as u32) as i32);
                let number = Self::vector_register(instruction, 0)?;
                self.set_vector(
                    number,
                    u128::from(low.to_bits()) | (u128::from(high.to_bits()) << 64),
                );
                Step::Retired
            }
            Mnemonic::Cvttps2dq => {
                let source = self.read_vector(instruction, 1, 16, UNALIGNED)?;
                let value = packed(source, 0, Lane::Dword, |lane, _| {
                    to_integer(f64::from(f32::from_bits(lane as u32)), Width::Dword)
                });
                let number = Self::vector_register(instruction, 0)?;
                self.set_vector(number, value);
                Step::Retired
            }
            Mnemonic::Cvtps2pd => {
                let source = self.read_vector(instruction, 1, 8, UNALIGNED)?;
                let low = f64::from(f32::from_bits(source as u32));
                let high = f64::from(f32::from_bits((source >> 32) as u32));
                let number = Self::vector_register(instruction, 0)?;
                self.set_vector(
                    number,
                    u128::from(low.to_bits()) | (u128::from(high.to_bits()) << 64),
                );
                Step::Retired
            }
            Mnemonic::Cvtpd2ps => {
                let source = self.read_vector(instruction, 1, 16, UNALIGNED)?;
                let low = (f64::from_bits(source as u64) as f32).to_bits();
                let high = (f64::from_bits((source >> 64) as u64) as f32).to_bits();
                let number = Self::vector_register(instruction, 0)?;
                self.set_vector(number, u128::from(low) | (u128::from(high) << 32));
                Step::Retired
            }

            // ---- packed integer arithmetic ----
            Mnemonic::Paddb => self.packed_binary(instruction, Lane::Byte, |a, b| a.wrapping_add(b))?,
            Mnemonic::Paddw => self.packed_binary(instruction, Lane::Word, |a, b| a.wrapping_add(b))?,
            Mnemonic::Paddd => self.packed_binary(instruction, Lane::Dword, |a, b| a.wrapping_add(b))?,
            Mnemonic::Paddq => self.packed_binary(instruction, Lane::Qword, |a, b| a.wrapping_add(b))?,
            Mnemonic::Psubb => self.packed_binary(instruction, Lane::Byte, |a, b| a.wrapping_sub(b))?,
            Mnemonic::Psubw => self.packed_binary(instruction, Lane::Word, |a, b| a.wrapping_sub(b))?,
            Mnemonic::Psubd => self.packed_binary(instruction, Lane::Dword, |a, b| a.wrapping_sub(b))?,
            Mnemonic::Psubq => self.packed_binary(instruction, Lane::Qword, |a, b| a.wrapping_sub(b))?,
            Mnemonic::Paddsb => self.packed_binary(instruction, Lane::Byte, saturating_add_signed(Lane::Byte))?,
            Mnemonic::Paddsw => self.packed_binary(instruction, Lane::Word, saturating_add_signed(Lane::Word))?,
            Mnemonic::Psubsb => self.packed_binary(instruction, Lane::Byte, saturating_sub_signed(Lane::Byte))?,
            Mnemonic::Psubsw => self.packed_binary(instruction, Lane::Word, saturating_sub_signed(Lane::Word))?,
            Mnemonic::Paddusb => self.packed_binary(instruction, Lane::Byte, saturating_add_unsigned(Lane::Byte))?,
            Mnemonic::Paddusw => self.packed_binary(instruction, Lane::Word, saturating_add_unsigned(Lane::Word))?,
            Mnemonic::Psubusb => self.packed_binary(instruction, Lane::Byte, saturating_sub_unsigned())?,
            Mnemonic::Psubusw => self.packed_binary(instruction, Lane::Word, saturating_sub_unsigned())?,
            Mnemonic::Pmullw => self.packed_binary(instruction, Lane::Word, |a, b| a.wrapping_mul(b))?,
            Mnemonic::Pmulld => self.packed_binary(instruction, Lane::Dword, |a, b| a.wrapping_mul(b))?,
            Mnemonic::Pmulhw => self.packed_binary(instruction, Lane::Word, |a, b| {
                ((Lane::Word.signed(a) * Lane::Word.signed(b)) >> 16) as u64
            })?,
            Mnemonic::Pmulhuw => {
                self.packed_binary(instruction, Lane::Word, |a, b| (a * b) >> 16)?
            }
            Mnemonic::Pmuludq => {
                // Only the even double-word lanes participate, and the
                // product is twice as wide, so this is not a lane-for-lane
                // operation and does not go through `packed`.
                let (number, left, right) = self.vector_operands(instruction)?;
                let mut result = 0u128;
                for half in 0..2u32 {
                    let shift = half * 64;
                    let a = u64::from((left >> shift) as u32);
                    let b = u64::from((right >> shift) as u32);
                    result |= u128::from(a * b) << shift;
                }
                self.set_vector(number, result);
                Step::Retired
            }
            Mnemonic::Pavgb => self.packed_binary(instruction, Lane::Byte, |a, b| (a + b + 1) >> 1)?,
            Mnemonic::Pavgw => self.packed_binary(instruction, Lane::Word, |a, b| (a + b + 1) >> 1)?,
            Mnemonic::Pminub => self.packed_binary(instruction, Lane::Byte, |a, b| a.min(b))?,
            Mnemonic::Pmaxub => self.packed_binary(instruction, Lane::Byte, |a, b| a.max(b))?,
            Mnemonic::Pminud => self.packed_binary(instruction, Lane::Dword, |a, b| a.min(b))?,
            Mnemonic::Pmaxud => self.packed_binary(instruction, Lane::Dword, |a, b| a.max(b))?,
            Mnemonic::Pminsw => self.packed_binary(instruction, Lane::Word, signed_minimum(Lane::Word))?,
            Mnemonic::Pmaxsw => self.packed_binary(instruction, Lane::Word, signed_maximum(Lane::Word))?,
            Mnemonic::Pminsb => self.packed_binary(instruction, Lane::Byte, signed_minimum(Lane::Byte))?,
            Mnemonic::Pmaxsb => self.packed_binary(instruction, Lane::Byte, signed_maximum(Lane::Byte))?,
            Mnemonic::Pminsd => self.packed_binary(instruction, Lane::Dword, signed_minimum(Lane::Dword))?,
            Mnemonic::Pmaxsd => self.packed_binary(instruction, Lane::Dword, signed_maximum(Lane::Dword))?,
            Mnemonic::Pcmpeqb => self.packed_binary(instruction, Lane::Byte, equal(Lane::Byte))?,
            Mnemonic::Pcmpeqw => self.packed_binary(instruction, Lane::Word, equal(Lane::Word))?,
            Mnemonic::Pcmpeqd => self.packed_binary(instruction, Lane::Dword, equal(Lane::Dword))?,
            Mnemonic::Pcmpeqq => self.packed_binary(instruction, Lane::Qword, equal(Lane::Qword))?,
            Mnemonic::Pcmpgtb => self.packed_binary(instruction, Lane::Byte, greater(Lane::Byte))?,
            Mnemonic::Pcmpgtw => self.packed_binary(instruction, Lane::Word, greater(Lane::Word))?,
            Mnemonic::Pcmpgtd => self.packed_binary(instruction, Lane::Dword, greater(Lane::Dword))?,
            Mnemonic::Pcmpgtq => self.packed_binary(instruction, Lane::Qword, greater(Lane::Qword))?,

            // ---- packed floating point ----
            Mnemonic::Addpd => {
                self.packed_double(instruction, |a, b| arithmetic_double(a, b, |a, b| a + b))?
            }
            Mnemonic::Subpd => {
                self.packed_double(instruction, |a, b| arithmetic_double(a, b, |a, b| a - b))?
            }
            Mnemonic::Mulpd => {
                self.packed_double(instruction, |a, b| arithmetic_double(a, b, |a, b| a * b))?
            }
            Mnemonic::Divpd => {
                self.packed_double(instruction, |a, b| arithmetic_double(a, b, |a, b| a / b))?
            }
            Mnemonic::Minpd => self.packed_double(instruction, minimum)?,
            Mnemonic::Maxpd => self.packed_double(instruction, maximum)?,
            Mnemonic::Sqrtpd => {
                self.packed_double(instruction, |_, b| unary_double(b, f64::sqrt))?
            }
            Mnemonic::Addps => {
                self.packed_single(instruction, |a, b| arithmetic_single(a, b, |a, b| a + b))?
            }
            Mnemonic::Subps => {
                self.packed_single(instruction, |a, b| arithmetic_single(a, b, |a, b| a - b))?
            }
            Mnemonic::Mulps => {
                self.packed_single(instruction, |a, b| arithmetic_single(a, b, |a, b| a * b))?
            }
            Mnemonic::Divps => {
                self.packed_single(instruction, |a, b| arithmetic_single(a, b, |a, b| a / b))?
            }
            Mnemonic::Minps => self.packed_single(instruction, single_minimum)?,
            Mnemonic::Maxps => self.packed_single(instruction, single_maximum)?,
            Mnemonic::Sqrtps => {
                self.packed_single(instruction, |_, b| unary_single(b, f32::sqrt))?
            }
            Mnemonic::Cmppd | Mnemonic::Cmpps => {
                let single = instruction.mnemonic() == Mnemonic::Cmpps;
                let code = instruction.immediate8();
                let (number, left, right) = self.vector_operands(instruction)?;
                let value = match single {
                    true => packed(left, right, Lane::Dword, |a, b| {
                        let holds = float_predicate(
                            code,
                            f64::from(f32::from_bits(a as u32)),
                            f64::from(f32::from_bits(b as u32)),
                        );
                        mask_of(holds, Lane::Dword)
                    }),
                    false => packed(left, right, Lane::Qword, |a, b| {
                        let holds =
                            float_predicate(code, f64::from_bits(a), f64::from_bits(b));
                        mask_of(holds, Lane::Qword)
                    }),
                };
                self.set_vector(number, value);
                Step::Retired
            }

            // ---- lane shuffling and extraction ----
            Mnemonic::Movmskps | Mnemonic::Movmskpd | Mnemonic::Pmovmskb => {
                let lane = match instruction.mnemonic() {
                    Mnemonic::Movmskps => Lane::Dword,
                    Mnemonic::Movmskpd => Lane::Qword,
                    _ => Lane::Byte,
                };
                let source = Self::vector_register(instruction, 1)?;
                let value = self.vector(source);
                let mut mask = 0u64;
                for index in 0..lane.count() {
                    let shift = index * lane.bits();
                    if ((value >> shift) as u64) & lane.sign_bit() != 0 {
                        mask |= 1 << index;
                    }
                }
                // A 32-bit write, which clears the register's upper half.
                let slice = Self::slice(instruction, instruction.op_register(0))?;
                self.tcb.write_register(slice, mask);
                Step::Retired
            }
            Mnemonic::Psllw | Mnemonic::Pslld | Mnemonic::Psllq | Mnemonic::Psrlw
            | Mnemonic::Psrld | Mnemonic::Psrlq | Mnemonic::Psraw | Mnemonic::Psrad => {
                let (lane, direction) = match instruction.mnemonic() {
                    Mnemonic::Psllw => (Lane::Word, Shift::Left),
                    Mnemonic::Pslld => (Lane::Dword, Shift::Left),
                    Mnemonic::Psllq => (Lane::Qword, Shift::Left),
                    Mnemonic::Psrlw => (Lane::Word, Shift::Logical),
                    Mnemonic::Psrld => (Lane::Dword, Shift::Logical),
                    Mnemonic::Psrlq => (Lane::Qword, Shift::Logical),
                    Mnemonic::Psraw => (Lane::Word, Shift::Arithmetic),
                    _ => (Lane::Dword, Shift::Arithmetic),
                };
                let number = Self::vector_register(instruction, 0)?;
                let value = self.vector(number);
                // The count is the whole 64-bit source, not one lane of it,
                // and a count at or past the lane width clears every lane
                // (or fills them with the sign, for the arithmetic form)
                // rather than being masked. Masking is the mistake here.
                let count = match instruction.op1_kind() {
                    OpKind::Immediate8 => u64::from(instruction.immediate8()),
                    _ => self.read_vector(instruction, 1, 8, UNALIGNED)? as u64,
                };
                let bits = u64::from(lane.bits());
                let shifted = packed(value, 0, lane, |a, _| match direction {
                    Shift::Left => match count < bits {
                        true => a << count,
                        false => 0,
                    },
                    Shift::Logical => match count < bits {
                        true => a >> count,
                        false => 0,
                    },
                    Shift::Arithmetic => {
                        let signed = lane.signed(a);
                        (signed >> count.min(bits - 1)) as u64
                    }
                });
                self.set_vector(number, shifted);
                Step::Retired
            }
            Mnemonic::Pslldq | Mnemonic::Psrldq => {
                // Whole-register byte shifts, which have no lanes: they are
                // how a vector is aligned after an unaligned load.
                let number = Self::vector_register(instruction, 0)?;
                let value = self.vector(number);
                let bytes = u32::from(instruction.immediate8()).min(16);
                let shifted = match instruction.mnemonic() {
                    Mnemonic::Pslldq => value.checked_shl(bytes * 8).unwrap_or(0),
                    _ => value.checked_shr(bytes * 8).unwrap_or(0),
                };
                self.set_vector(number, shifted);
                Step::Retired
            }
            Mnemonic::Pshufd => {
                let source = self.read_vector(instruction, 1, 16, UNALIGNED)?;
                let control = instruction.immediate8();
                let mut result = 0u128;
                for index in 0..4u32 {
                    let selector = u32::from((control >> (index * 2)) & 3);
                    let lane = (source >> (selector * 32)) & u128::from(u32::MAX);
                    result |= lane << (index * 32);
                }
                let number = Self::vector_register(instruction, 0)?;
                self.set_vector(number, result);
                Step::Retired
            }
            Mnemonic::Pshuflw | Mnemonic::Pshufhw => {
                let source = self.read_vector(instruction, 1, 16, UNALIGNED)?;
                let control = instruction.immediate8();
                let high = instruction.mnemonic() == Mnemonic::Pshufhw;
                let base = if high { 64 } else { 0 };
                let mut result = match high {
                    true => source & u128::from(u64::MAX),
                    false => source & !u128::from(u64::MAX),
                };
                for index in 0..4u32 {
                    let selector = u32::from((control >> (index * 2)) & 3);
                    let lane = (source >> (base + selector * 16)) & u128::from(u16::MAX);
                    result |= lane << (base + index * 16);
                }
                let number = Self::vector_register(instruction, 0)?;
                self.set_vector(number, result);
                Step::Retired
            }
            Mnemonic::Shufps | Mnemonic::Shufpd => {
                let number = Self::vector_register(instruction, 0)?;
                let left = self.vector(number);
                let right = self.read_vector(instruction, 1, 16, UNALIGNED)?;
                let control = instruction.immediate8();
                let result = match instruction.mnemonic() {
                    Mnemonic::Shufps => {
                        let mut result = 0u128;
                        for index in 0..4u32 {
                            let selector = u32::from((control >> (index * 2)) & 3);
                            let from = if index < 2 { left } else { right };
                            let lane = (from >> (selector * 32)) & u128::from(u32::MAX);
                            result |= lane << (index * 32);
                        }
                        result
                    }
                    _ => {
                        let low = (left >> (u32::from(control & 1) * 64)) & u128::from(u64::MAX);
                        let high =
                            (right >> (u32::from((control >> 1) & 1) * 64)) & u128::from(u64::MAX);
                        low | (high << 64)
                    }
                };
                self.set_vector(number, result);
                Step::Retired
            }
            Mnemonic::Punpcklbw | Mnemonic::Punpckhbw | Mnemonic::Punpcklwd
            | Mnemonic::Punpckhwd | Mnemonic::Punpckldq | Mnemonic::Unpcklps
            | Mnemonic::Punpckhdq | Mnemonic::Unpckhps | Mnemonic::Punpcklqdq
            | Mnemonic::Unpcklpd | Mnemonic::Punpckhqdq | Mnemonic::Unpckhpd => {
                let (lane, high) = match instruction.mnemonic() {
                    Mnemonic::Punpcklbw => (Lane::Byte, false),
                    Mnemonic::Punpckhbw => (Lane::Byte, true),
                    Mnemonic::Punpcklwd => (Lane::Word, false),
                    Mnemonic::Punpckhwd => (Lane::Word, true),
                    Mnemonic::Punpckldq | Mnemonic::Unpcklps => (Lane::Dword, false),
                    Mnemonic::Punpckhdq | Mnemonic::Unpckhps => (Lane::Dword, true),
                    Mnemonic::Punpcklqdq | Mnemonic::Unpcklpd => (Lane::Qword, false),
                    _ => (Lane::Qword, true),
                };
                let (number, left, right) = self.vector_operands(instruction)?;
                let bits = lane.bits();
                let half = lane.count() / 2;
                let start = if high { half } else { 0 };
                let mut result = 0u128;
                for index in 0..half {
                    let source = (start + index) * bits;
                    let a = (left >> source) & u128::from(lane.mask());
                    let b = (right >> source) & u128::from(lane.mask());
                    result |= a << (index * 2 * bits);
                    result |= b << ((index * 2 + 1) * bits);
                }
                self.set_vector(number, result);
                Step::Retired
            }
            Mnemonic::Packsswb | Mnemonic::Packssdw | Mnemonic::Packuswb
            | Mnemonic::Packusdw => {
                let (from, signed) = match instruction.mnemonic() {
                    Mnemonic::Packsswb => (Lane::Word, true),
                    Mnemonic::Packuswb => (Lane::Word, false),
                    Mnemonic::Packssdw => (Lane::Dword, true),
                    _ => (Lane::Dword, false),
                };
                let to = match from {
                    Lane::Word => Lane::Byte,
                    _ => Lane::Word,
                };
                let (number, left, right) = self.vector_operands(instruction)?;
                let half = from.count();
                let mut result = 0u128;
                for index in 0..half * 2 {
                    let source = match index < half {
                        true => left >> (index * from.bits()),
                        false => right >> ((index - half) * from.bits()),
                    };
                    let value = from.signed((source as u64) & from.mask());
                    let saturated = match signed {
                        true => value.clamp(
                            -(1i64 << (to.bits() - 1)),
                            (1i64 << (to.bits() - 1)) - 1,
                        ) as u64,
                        false => value.clamp(0, to.mask() as i64) as u64,
                    };
                    result |= u128::from(saturated & to.mask()) << (index * to.bits());
                }
                self.set_vector(number, result);
                Step::Retired
            }
            Mnemonic::Pinsrw => {
                let number = Self::vector_register(instruction, 0)?;
                let value = self.read(instruction, 1, Width::Word)?;
                let index = u32::from(instruction.immediate8() & 7);
                let kept = self.vector(number) & !(u128::from(u16::MAX) << (index * 16));
                self.set_vector(number, kept | (u128::from(value & 0xffff) << (index * 16)));
                Step::Retired
            }
            Mnemonic::Pextrw => {
                let source = self.read_vector(instruction, 1, 16, UNALIGNED)?;
                let index = u32::from(instruction.immediate8() & 7);
                let value = ((source >> (index * 16)) as u64) & 0xffff;
                self.write(instruction, 0, Width::Dword, value)?
            }
            Mnemonic::Ldmxcsr => {
                let width = Width::Dword;
                let at = self.address(instruction)?;
                self.tcb.mxcsr = self.space.load(at, width)? as u32;
                Step::Retired
            }
            Mnemonic::Stmxcsr => {
                let at = self.address(instruction)?;
                self.space.store(at, Width::Dword, u64::from(self.tcb.mxcsr))?;
                Step::Retired
            }

            _ => return Ok(None),
        };
        Ok(Some(step))
    }

    /// The three things nearly every packed instruction needs: the
    /// destination register number and the two source vectors.
    fn vector_operands(&mut self, instruction: &Instruction) -> Result<(usize, u128, u128), Trap> {
        let number = Self::vector_register(instruction, 0)?;
        let left = self.vector(number);
        let right = self.read_vector(instruction, 1, 16, false)?;
        Ok((number, left, right))
    }

    fn vector_binary(
        &mut self,
        instruction: &Instruction,
        operation: impl Fn(u128, u128) -> u128,
    ) -> Result<Step, Trap> {
        let (number, left, right) = self.vector_operands(instruction)?;
        self.set_vector(number, operation(left, right));
        Ok(Step::Retired)
    }

    fn packed_binary(
        &mut self,
        instruction: &Instruction,
        lane: Lane,
        operation: impl Fn(u64, u64) -> u64,
    ) -> Result<Step, Trap> {
        let (number, left, right) = self.vector_operands(instruction)?;
        self.set_vector(number, packed(left, right, lane, operation));
        Ok(Step::Retired)
    }

    fn packed_double(
        &mut self,
        instruction: &Instruction,
        operation: impl Fn(f64, f64) -> f64,
    ) -> Result<Step, Trap> {
        let (number, left, right) = self.vector_operands(instruction)?;
        self.set_vector(number, packed_f64(left, right, operation));
        Ok(Step::Retired)
    }

    /// The single-precision packed family, computed *in* single precision.
    ///
    /// Not by widening to double, operating, and narrowing back: that is
    /// double rounding, and it differs from the hardware in the last bit for
    /// a large fraction of operands. The temptation is real because the
    /// double-precision closure is already written; the answer is no.
    fn packed_single(
        &mut self,
        instruction: &Instruction,
        operation: impl Fn(f32, f32) -> f32,
    ) -> Result<Step, Trap> {
        let (number, left, right) = self.vector_operands(instruction)?;
        self.set_vector(number, packed_f32(left, right, operation));
        Ok(Step::Retired)
    }

    /// A scalar double operation: the low lane is computed, and everything
    /// above it is the destination's, untouched. That preservation is the
    /// grain of the whole scalar family.
    fn scalar_f64(
        &mut self,
        instruction: &Instruction,
        operation: impl Fn(f64, f64) -> f64,
    ) -> Result<Step, Trap> {
        let number = Self::vector_register(instruction, 0)?;
        let left = self.vector(number);
        let right = self.read_vector(instruction, 1, 8, false)?;
        let value = operation(f64::from_bits(left as u64), f64::from_bits(right as u64));
        self.set_vector(
            number,
            (left & !u128::from(u64::MAX)) | u128::from(value.to_bits()),
        );
        Ok(Step::Retired)
    }

    fn scalar_f32(
        &mut self,
        instruction: &Instruction,
        operation: impl Fn(f32, f32) -> f32,
    ) -> Result<Step, Trap> {
        let number = Self::vector_register(instruction, 0)?;
        let left = self.vector(number);
        let right = self.read_vector(instruction, 1, 4, false)?;
        let value = operation(
            f32::from_bits(left as u32),
            f32::from_bits(right as u32),
        );
        self.set_vector(
            number,
            (left & !u128::from(u32::MAX)) | u128::from(value.to_bits()),
        );
        Ok(Step::Retired)
    }
}

#[derive(Clone, Copy)]
enum Shift {
    Left,
    Logical,
    Arithmetic,
}

fn saturating_add_signed(lane: Lane) -> impl Fn(u64, u64) -> u64 {
    let low = -(1i64 << (lane.bits() - 1));
    let high = (1i64 << (lane.bits() - 1)) - 1;
    move |a, b| (lane.signed(a) + lane.signed(b)).clamp(low, high) as u64
}

fn saturating_sub_signed(lane: Lane) -> impl Fn(u64, u64) -> u64 {
    let low = -(1i64 << (lane.bits() - 1));
    let high = (1i64 << (lane.bits() - 1)) - 1;
    move |a, b| (lane.signed(a) - lane.signed(b)).clamp(low, high) as u64
}

fn saturating_add_unsigned(lane: Lane) -> impl Fn(u64, u64) -> u64 {
    let high = lane.mask();
    move |a, b| (a + b).min(high)
}

fn saturating_sub_unsigned() -> impl Fn(u64, u64) -> u64 {
    move |a, b| a.saturating_sub(b)
}

fn signed_minimum(lane: Lane) -> impl Fn(u64, u64) -> u64 {
    move |a, b| match lane.signed(a) <= lane.signed(b) {
        true => a,
        false => b,
    }
}

fn signed_maximum(lane: Lane) -> impl Fn(u64, u64) -> u64 {
    move |a, b| match lane.signed(a) >= lane.signed(b) {
        true => a,
        false => b,
    }
}

fn equal(lane: Lane) -> impl Fn(u64, u64) -> u64 {
    move |a, b| mask_of(a == b, lane)
}

fn greater(lane: Lane) -> impl Fn(u64, u64) -> u64 {
    move |a, b| mask_of(lane.signed(a) > lane.signed(b), lane)
}

/// Round-half-to-even, which is what the default rounding mode means and
/// what `f64::round` does not do (it rounds half away from zero).
fn round_to_nearest_even(value: f64) -> f64 {
    let down = value.floor();
    let fraction = value - down;
    match fraction.partial_cmp(&0.5) {
        Some(std::cmp::Ordering::Less) => down,
        Some(std::cmp::Ordering::Greater) => down + 1.0,
        // Exactly halfway: to the even neighbour.
        _ => match (down as i64) % 2 == 0 {
            true => down,
            false => down + 1.0,
        },
    }
}

/// Whether a register is one of the eight legacy MMX registers, which alias
/// the x87 stack and are not modelled here.
pub fn is_mmx(register: Register) -> bool {
    register.is_mm()
}
