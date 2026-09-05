//! Soft emulation of the x87 FPU.
//!
//! The interpreter's x87 and MMX instructions are computed here, against a
//! [`state::X87State`] that lives in each thread's control block: the
//! register stack, the control and status words, and extended-precision
//! arithmetic that matches the hardware bit for bit where the table below
//! says it does.
//!
//! # Tier table
//!
//! Every operation carries its fidelity tier. The differential harness picks
//! bit-exact versus tolerance comparison from this table, and "full
//! emulation" is this table driven to done.
//!
//! | Operation | Tier |
//! | --- | --- |
//! | `fadd`/`fsub`/`fmul`/`fdiv` (all forms) | bit-exact (host-FPU oracle) |
//! | `fsqrt`, `frndint`, `fprem`, `fprem1` | bit-exact (host-FPU oracle) |
//! | `fscale`, `fxtract`, `fchs`, `fabs`, `fxch` | bit-exact |
//! | loads/stores m32/m64/m80, `fild`/`fist(p)`/`fisttp` | bit-exact |
//! | `fcom`/`fucom`/`fcomi`/`fucomi`/`ftst`/`fxam`/`fcmovcc` | bit-exact |
//! | constants (`fld1`..`fldz`), RC-sensitive | bit-exact (measured 2026-08-28) |
//! | `fnstcw`/`fldcw`/`fnstsw`/`fnclex`/`finit` | bit-exact |
//! | `fnstenv`/`fldenv`/`fnsave`/`frstor` | bit-exact (FIP/FDP as zeros) |
//! | `f2xm1`, `fyl2x`, `fyl2xp1`, `fpatan` | f64-backed; exact special cases; divergence measured in ulps by the oracle |
//! | `fsin`/`fcos`/`fsincos`/`fptan` | not yet — roadmap, transcendental target |
//! | MMX family, `emms` | not yet — roadmap, after the scalar core |
//! | `fxsave`/`fxrstor` | not yet — roadmap, on the sigframe render |
//! | `fbld`/`fbstp` | not yet — roadmap |
//! | unmasked-exception delivery | not yet — flags and ES maintained now; delivery via kisal's signal machinery |
//!
//! Exception *flags* are recorded faithfully (including ES and the stack
//! fault); exception *traps* are not delivered yet — everything behaves
//! as-if-masked, which is also why `fwait` is currently a no-op.
//!
//! # Two oracles
//!
//! The hardware oracle in `tests/oracle.rs` drives each operation from a
//! fresh state, which is what makes it able to sweep millions of operands.
//! It is structurally blind to one class of defect: what an operation
//! leaves behind for the *next* one. A condition code that should have been
//! written to zero and was left alone, a flag that should have been
//! suppressed because the other operand was a NaN, register data erased
//! where hardware only marks it unreachable — each of those is invisible
//! from a fresh state and visible in a sequence.
//!
//! The VM's lockstep oracle (`targum/tests/lockstep.rs`) supplies the sequence:
//! it compares all eighty bits of every stack register and both descriptive
//! words against a `ptrace`d process after every retired instruction. Four
//! defects of that shape were found and fixed on 2026-08-30. Neither oracle
//! replaces the other, and a change to this crate should answer to both.

pub mod arith;
pub mod compare;
pub mod convert;
pub mod f80;
pub mod ops;
pub mod state;
pub mod transcendental;

/// FSW bit assignments, used both for sticky state and as the flag word
/// operations return. Matching the hardware layout is what lets the oracle
/// compare flag words with a mask and no translation.
pub mod flags {
    pub const INVALID: u16 = 0x0001;
    pub const DENORMAL: u16 = 0x0002;
    pub const ZERO_DIVIDE: u16 = 0x0004;
    pub const OVERFLOW: u16 = 0x0008;
    pub const UNDERFLOW: u16 = 0x0010;
    pub const PRECISION: u16 = 0x0020;
    pub const STACK_FAULT: u16 = 0x0040;
    /// Summary bit: an unmasked exception is pending. Maintained by the
    /// state, never returned by an operation.
    pub const ERROR_SUMMARY: u16 = 0x0080;
    pub const C0: u16 = 0x0100;
    /// Doubles as "the result was rounded up" after arithmetic and as the
    /// fault direction on stack faults (1 = overflow).
    pub const C1: u16 = 0x0200;
    pub const C2: u16 = 0x0400;
    pub const C3: u16 = 0x4000;

    /// The six accumulating exceptions.
    pub const EXCEPTIONS: u16 =
        INVALID | DENORMAL | ZERO_DIVIDE | OVERFLOW | UNDERFLOW | PRECISION;
    pub const CONDITIONS: u16 = C0 | C1 | C2 | C3;
}

/// Rounding control, FCW bits 10–11.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rounding {
    Nearest,
    Down,
    Up,
    Chop,
}

impl Rounding {
    pub fn from_control(control: u16) -> Self {
        match (control >> 10) & 3 {
            0 => Rounding::Nearest,
            1 => Rounding::Down,
            2 => Rounding::Up,
            _ => Rounding::Chop,
        }
    }
}

/// Precision control, FCW bits 8–9. It narrows the rounding position, never
/// the exponent range — which is exactly what produces the famous double
/// rounding when an extended result later stores to memory.
///
/// The `01` encoding is architecturally reserved; it maps to `Single` here
/// and the oracle never generates it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Precision {
    Single,
    Double,
    Extended,
}

impl Precision {
    pub fn from_control(control: u16) -> Self {
        match (control >> 8) & 3 {
            2 => Precision::Double,
            3 => Precision::Extended,
            _ => Precision::Single,
        }
    }

    /// Bits of significand below the rounding position, as a mask.
    pub fn tail_mask(self) -> u64 {
        match self {
            Precision::Extended => 0,
            Precision::Double => (1 << 11) - 1,
            Precision::Single => (1 << 40) - 1,
        }
    }
}
