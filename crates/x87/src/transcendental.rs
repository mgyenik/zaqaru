//! The constants and the genuinely transcendental operations.
//!
//! The constants are bit-exact: their internal values are wider than 64
//! bits and round per RC — verified against the host FPU in all four
//! modes (measured 2026-08-28).
//!
//! `f2xm1`/`fyl2x`/`fyl2xp1`/`fpatan` are the f64-backed operations: exact
//! special cases per the architecture's tables, cores computed in double
//! via `libm`, with the divergence from hardware measured in ulps by the
//! oracle rather than assumed. Bit-matching is not the target here —
//! Intel and AMD themselves disagree in these ops' low bits.

use crate::arith::{self, Outcome};
use crate::f80::{BIAS, Class, EXPONENT_MAX, F80};
use crate::flags::{DENORMAL, INVALID, PRECISION, ZERO_DIVIDE};
use crate::{Precision, Rounding};

/// The top 128 bits of π's significand. The oracle checks every rounding
/// of it against the hardware, both directly (`fldpi`) and through the
/// π-multiples `fpatan`'s special cases produce.
const PI_SIGNIFICAND: u128 = 0xC90F_DAA2_2168_C234_C4C6_628B_80DC_1CD1;

/// A constant as (128-bit significand, unbiased exponent). All tails are
/// irrational, so a rounding tie cannot occur and 128 bits decide every
/// mode exactly.
struct Constant {
    significand: u128,
    exponent: i32,
}

const LOG2_TEN: Constant = Constant {
    significand: 0xD49A_784B_CD1B_8AFE_492B_F6FF_4DAF_DB4C,
    exponent: 1,
};
const LOG2_E: Constant = Constant {
    significand: 0xB8AA_3B29_5C17_F0BB_BE87_FED0_691D_3E88,
    exponent: 0,
};
const PI: Constant = Constant {
    significand: PI_SIGNIFICAND,
    exponent: 1,
};
const LOG10_2: Constant = Constant {
    significand: 0x9A20_9A84_FBCF_F798_8F89_59AC_0B7C_9178,
    exponent: -2,
};
const LN_2: Constant = Constant {
    significand: 0xB172_17F7_D1CF_79AB_C9E3_B398_03F2_F6AF,
    exponent: -1,
};

fn round_constant(constant: &Constant, sign: bool, rounding: Rounding) -> F80 {
    // The load-constant instructions round per RC but are exempt from
    // precision control and raise no precision exception, so this is
    // narrower than the arithmetic path: round at 64 bits, drop the flags.
    arith::round_pack(
        sign,
        constant.exponent,
        constant.significand,
        rounding,
        Precision::Extended,
    )
    .value
}

/// The seven load-constant instructions, indexed by their opcode order:
/// `fld1`, `fldl2t`, `fldl2e`, `fldpi`, `fldlg2`, `fldln2`, `fldz`.
pub fn constant(index: u32, rounding: Rounding) -> F80 {
    match index {
        0 => F80::ONE,
        1 => round_constant(&LOG2_TEN, false, rounding),
        2 => round_constant(&LOG2_E, false, rounding),
        3 => round_constant(&PI, false, rounding),
        4 => round_constant(&LOG10_2, false, rounding),
        5 => round_constant(&LN_2, false, rounding),
        _ => F80::ZERO,
    }
}

/// π scaled by a dyadic fraction — the exact special-case results of
/// `fpatan`: numerator/4 × π, rounded per RC.
fn pi_multiple(numerator: u32, sign: bool, rounding: Rounding) -> F80 {
    if numerator == 0 {
        return F80::new(sign, 0, 0);
    }
    // numerator × π's significand, in 192 bits, renormalized to 128 with
    // the dropped bits kept sticky. A tie is impossible (π is irrational),
    // so the sticky fold loses nothing the rounding needs.
    let high = (PI_SIGNIFICAND >> 64) * numerator as u128;
    let low = (PI_SIGNIFICAND & u128::from(u64::MAX)) * numerator as u128;
    let top = high + (low >> 64);
    let bottom = low as u64;
    let width = 128 - top.leading_zeros();
    let shift = width as i32 - 64;
    debug_assert!((0..=2).contains(&shift));
    let mut significand = (top << (64 - shift)) | ((bottom >> shift) as u128);
    if shift > 0 && bottom & ((1 << shift) - 1) != 0 {
        significand |= 1;
    }
    let constant = Constant {
        significand,
        // π's own exponent, scaled by the numerator's width, divided by 4.
        exponent: PI.exponent + shift - 2,
    };
    round_constant(&constant, sign, rounding)
}

fn is_invalid_operand(class: Class) -> bool {
    matches!(
        class,
        Class::QuietNan | Class::SignallingNan | Class::Unsupported
    )
}

fn denormal_flag(class: Class) -> u16 {
    if class == Class::Subnormal { DENORMAL } else { 0 }
}

fn to_double(a: F80) -> f64 {
    let (bits, _) = crate::convert::to_f64(a, Rounding::Nearest);
    f64::from_bits(bits)
}

fn from_double(value: f64, flags: u16) -> Outcome {
    let out = crate::convert::from_f64(value.to_bits());
    Outcome {
        value: out.value,
        flags: flags | out.flags | PRECISION,
    }
}

/// `f2xm1`: 2^ST0 − 1, architecturally defined on [−1, 1].
pub fn f2xm1(a: F80) -> Outcome {
    let class = a.classify();
    if is_invalid_operand(class) {
        return arith::propagate_one(a);
    }
    match class {
        Class::Zero => return Outcome::exact(a),
        Class::Infinity => {
            // 2^+∞ − 1 = +∞; 2^−∞ − 1 = −1 exactly.
            return if a.sign() {
                Outcome::exact(F80::ONE.negate())
            } else {
                Outcome::exact(a)
            };
        }
        _ => {}
    }
    let u = a.unpack();
    if u.exp > 0 || (u.exp == 0 && u.sig > 1 << 63) {
        // Outside the architectural domain [−1, 1] the result is
        // undefined; the hardware returns the operand unchanged with a
        // precision flag (probed 2026-08-28), so this does too.
        return Outcome {
            value: a,
            flags: PRECISION,
        };
    }
    let flags = denormal_flag(class);
    let x = to_double(a);
    from_double(libm::expm1(x * core::f64::consts::LN_2), flags)
}

/// `fyl2x`: ST1 × log2(ST0). The caller writes the result over ST1 and
/// pops.
pub fn fyl2x(x: F80, y: F80, rounding: Rounding, precision: Precision) -> Outcome {
    let cx = x.classify();
    let cy = y.classify();
    if is_invalid_operand(cx) || is_invalid_operand(cy) {
        return arith::propagate(x, y);
    }
    let flags = denormal_flag(cx) | denormal_flag(cy);
    if cx == Class::Zero {
        return match cy {
            Class::Zero => Outcome {
                value: F80::INDEFINITE,
                flags: INVALID,
            },
            // y × log2(0) = y × −∞, poled away from y's sign; a finite y
            // makes it a genuine division by the zero operand.
            Class::Infinity => Outcome {
                value: F80::new(!y.sign(), EXPONENT_MAX, 1 << 63),
                flags,
            },
            _ => Outcome {
                value: F80::new(!y.sign(), EXPONENT_MAX, 1 << 63),
                flags: ZERO_DIVIDE,
            },
        };
    }
    if x.sign() {
        return Outcome {
            value: F80::INDEFINITE,
            flags: INVALID,
        };
    }
    if cx == Class::Infinity {
        return match cy {
            Class::Zero => Outcome {
                value: F80::INDEFINITE,
                flags: INVALID,
            },
            _ => Outcome {
                value: F80::new(y.sign(), EXPONENT_MAX, 1 << 63),
                flags,
            },
        };
    }
    if x == F80::ONE {
        return match cy {
            Class::Infinity => Outcome {
                value: F80::INDEFINITE,
                flags: INVALID,
            },
            _ => Outcome {
                value: F80::new(y.sign(), 0, 0),
                flags,
            },
        };
    }
    let below_one = x.unpack().exp < 0;
    match cy {
        Class::Zero => {
            return Outcome {
                value: F80::new(y.sign() != below_one, 0, 0),
                flags,
            };
        }
        Class::Infinity => {
            return Outcome {
                value: F80::new(y.sign() != below_one, EXPONENT_MAX, 1 << 63),
                flags,
            };
        }
        _ => {}
    }

    let u = x.unpack();
    if (-1..=0).contains(&u.exp) {
        // x within [1/2, 2): log2 through log1p of the exact extended
        // difference, so a significand's worth of cancellation is kept.
        let delta = arith::sub(x, F80::ONE, Rounding::Nearest, Precision::Extended).value;
        let log2 = libm::log1p(to_double(delta)) * core::f64::consts::LOG2_E;
        let product = arith::mul(y, from_double(log2, 0).value, rounding, precision);
        return Outcome {
            value: product.value,
            flags: flags | product.flags | PRECISION,
        };
    }
    // Elsewhere the integer exponent dominates and no cancellation is
    // possible: y×e exactly, plus y×log2(m) in double.
    let mantissa = F80::new(false, BIAS as u16, u.sig);
    let log2m = libm::log2(to_double(mantissa));
    let integer_part = arith::mul(
        y,
        crate::convert::from_i64(u.exp as i64),
        rounding,
        precision,
    );
    let fraction_part = arith::mul(y, from_double(log2m, 0).value, rounding, precision);
    let sum = arith::add(
        integer_part.value,
        fraction_part.value,
        rounding,
        precision,
    );
    Outcome {
        value: sum.value,
        flags: flags | integer_part.flags | fraction_part.flags | sum.flags | PRECISION,
    }
}

/// `fyl2xp1`: ST1 × log2(1 + ST0), architecturally defined for
/// |ST0| < 1 − √2/2.
pub fn fyl2xp1(x: F80, y: F80, rounding: Rounding, precision: Precision) -> Outcome {
    let cx = x.classify();
    let cy = y.classify();
    if is_invalid_operand(cx) || is_invalid_operand(cy) {
        return arith::propagate(x, y);
    }
    let flags = denormal_flag(cx) | denormal_flag(cy);
    if cx == Class::Zero {
        return match cy {
            Class::Infinity => Outcome {
                value: F80::INDEFINITE,
                flags: INVALID,
            },
            _ => Outcome {
                value: F80::new(y.sign() != x.sign(), 0, 0),
                flags,
            },
        };
    }
    if cy == Class::Zero {
        return Outcome {
            value: F80::new(y.sign() != x.sign(), 0, 0),
            flags,
        };
    }
    if cy == Class::Infinity {
        return Outcome {
            value: F80::new(y.sign() != x.sign(), EXPONENT_MAX, 1 << 63),
            flags,
        };
    }
    let log2p1 = libm::log1p(to_double(x)) * core::f64::consts::LOG2_E;
    let product = arith::mul(y, from_double(log2p1, 0).value, rounding, precision);
    Outcome {
        value: product.value,
        flags: flags | product.flags | PRECISION,
    }
}

/// `fpatan`: arctangent of ST1/ST0, quadrant-correct — atan2(y = ST1,
/// x = ST0). The caller writes over ST1 and pops.
pub fn fpatan(x: F80, y: F80, rounding: Rounding, precision: Precision) -> Outcome {
    let cx = x.classify();
    let cy = y.classify();
    if is_invalid_operand(cx) || is_invalid_operand(cy) {
        return arith::propagate(x, y);
    }
    let flags = denormal_flag(cx) | denormal_flag(cy);
    let sign = y.sign();
    let multiple = match (cx, cy) {
        // The architecture's special-case table, as quarters of π.
        (Class::Infinity, Class::Infinity) => Some(if x.sign() { 3 } else { 1 }),
        (Class::Infinity, _) => Some(if x.sign() { 4 } else { 0 }),
        (_, Class::Infinity) => Some(2),
        (Class::Zero, Class::Zero) => Some(if x.sign() { 4 } else { 0 }),
        (_, Class::Zero) => Some(if x.sign() { 4 } else { 0 }),
        (Class::Zero, _) => Some(2),
        _ => None,
    };
    if let Some(numerator) = multiple {
        let value = pi_multiple(numerator, sign, rounding);
        let inexact = if numerator == 0 { 0 } else { PRECISION };
        return Outcome {
            value,
            flags: flags | inexact,
        };
    }
    let angle = libm::atan2(to_double(y), to_double(x));
    let out = from_double(angle, flags);
    // The double round-tripped exactly into extended; precision control
    // still applies to the delivered result.
    let rounded = arith::add(out.value, F80::ZERO, rounding, precision);
    Outcome {
        value: rounded.value,
        flags: out.flags | rounded.flags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_constants_round_per_mode() {
        // The 2026-08-28 host measurement, all four modes.
        let cases: [(u32, u16, u64, u64); 5] = [
            (3, 0x4000, 0xC90F_DAA2_2168_C235, 0xC90F_DAA2_2168_C234),
            (1, 0x4000, 0xD49A_784B_CD1B_8AFE, 0xD49A_784B_CD1B_8AFE),
            (2, 0x3FFF, 0xB8AA_3B29_5C17_F0BC, 0xB8AA_3B29_5C17_F0BB),
            (4, 0x3FFD, 0x9A20_9A84_FBCF_F799, 0x9A20_9A84_FBCF_F798),
            (5, 0x3FFE, 0xB172_17F7_D1CF_79AC, 0xB172_17F7_D1CF_79AB),
        ];
        for (index, exponent, nearest, truncated) in cases {
            let n = constant(index, Rounding::Nearest);
            assert_eq!((n.sign_exponent, n.significand), (exponent, nearest));
            let c = constant(index, Rounding::Chop);
            assert_eq!((c.sign_exponent, c.significand), (exponent, truncated));
            let d = constant(index, Rounding::Down);
            assert_eq!(d.significand, truncated);
            let u = constant(index, Rounding::Up);
            assert_eq!(u.significand, truncated + 1);
        }
        assert_eq!(constant(0, Rounding::Chop), F80::ONE);
        assert_eq!(constant(6, Rounding::Up), F80::ZERO);
    }

    #[test]
    fn pi_multiples() {
        // π/2 and π/4 share π's significand; 3π/4 has its own.
        let half = pi_multiple(2, false, Rounding::Nearest);
        assert_eq!(half.significand, 0xC90F_DAA2_2168_C235);
        assert_eq!(half.sign_exponent, 0x3FFF);
        let quarter = pi_multiple(1, false, Rounding::Nearest);
        assert_eq!(quarter.sign_exponent, 0x3FFE);
        let three_quarters = pi_multiple(3, true, Rounding::Nearest);
        assert!(three_quarters.sign());
        let value = f64::from_bits(crate::convert::to_f64(three_quarters, Rounding::Nearest).0);
        assert!((value + 3.0 * core::f64::consts::FRAC_PI_4).abs() < 1e-15);
        let whole = pi_multiple(4, false, Rounding::Chop);
        assert_eq!(whole.significand, 0xC90F_DAA2_2168_C234);
        assert_eq!(whole.sign_exponent, 0x4000);
    }

    fn f(value: f64) -> F80 {
        crate::convert::from_f64(value.to_bits()).value
    }

    fn close(outcome: Outcome, expected: f64) {
        let got = f64::from_bits(crate::convert::to_f64(outcome.value, Rounding::Nearest).0);
        assert!(
            (got - expected).abs() <= expected.abs() * 1e-14 + 1e-300,
            "{got} vs {expected}"
        );
    }

    #[test]
    fn cores_track_double_precision() {
        close(f2xm1(f(0.5)), core::f64::consts::SQRT_2 - 1.0);
        close(f2xm1(f(-1.0)), -0.5);
        let (r, p) = (Rounding::Nearest, Precision::Extended);
        close(fyl2x(f(8.0), f(1.0), r, p), 3.0);
        close(fyl2x(f(0.25), f(3.0), r, p), -6.0);
        close(fyl2xp1(f(0.001), f(1.0), r, p), 0.001_f64.ln_1p() / 2.0_f64.ln());
        close(fpatan(f(1.0), f(1.0), r, p), core::f64::consts::FRAC_PI_4);
        close(fpatan(f(-1.0), f(1.0), r, p), 3.0 * core::f64::consts::FRAC_PI_4);
    }

    #[test]
    fn fyl2x_keeps_precision_near_one() {
        // x = 1 + 2^-60: an f64-rounded mantissa would see exactly 1 and
        // return zero; the exact-difference path keeps the value.
        let tiny = F80::new(false, (BIAS - 60) as u16, 1 << 63);
        let x = arith::add(F80::ONE, tiny, Rounding::Nearest, Precision::Extended).value;
        let out = fyl2x(x, F80::ONE, Rounding::Nearest, Precision::Extended);
        let got = f64::from_bits(crate::convert::to_f64(out.value, Rounding::Nearest).0);
        let expected = (2f64.powi(-60)) * core::f64::consts::LOG2_E;
        assert!((got - expected).abs() < expected * 1e-12, "{got} vs {expected}");
    }

    #[test]
    fn fyl2x_specials() {
        let (r, p) = (Rounding::Nearest, Precision::Extended);
        let out = fyl2x(F80::ZERO, f(2.0), r, p);
        assert!(out.flags & ZERO_DIVIDE != 0);
        assert_eq!(out.value.classify(), Class::Infinity);
        assert!(out.value.sign());
        let out = fyl2x(f(-2.0), f(1.0), r, p);
        assert!(out.flags & INVALID != 0);
        let out = fyl2x(F80::ONE, f(5.0), r, p);
        assert_eq!(out.value.classify(), Class::Zero);
        assert_eq!(out.flags & PRECISION, 0);
    }
}
