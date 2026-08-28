//! Conversions between the extended format and everything the load/store
//! instructions speak: f32, f64, and the three integer widths.
//!
//! Widening is exact by construction (both float formats and all integer
//! widths embed in a 64-bit significand); narrowing rounds by RC — never by
//! PC, which does not apply to stores.

use crate::Rounding;
use crate::f80::{BIAS, Class, F80, Unpacked};
use crate::flags::{C1, DENORMAL, INVALID, OVERFLOW, PRECISION, UNDERFLOW};

use crate::arith::Outcome;

/// `fld m64`.
pub fn from_f64(bits: u64) -> Outcome {
    let sign = bits >> 63 != 0;
    let exponent = ((bits >> 52) & 0x7FF) as i32;
    let fraction = bits & ((1 << 52) - 1);
    match exponent {
        0x7FF => {
            let significand = (1 << 63) | (fraction << 11);
            if fraction == 0 {
                Outcome::exact(F80::new(sign, crate::f80::EXPONENT_MAX, significand))
            } else if fraction >> 51 == 0 {
                // Signalling: quieted on the way in, invalid raised.
                Outcome {
                    value: F80::new(sign, crate::f80::EXPONENT_MAX, significand | (1 << 62)),
                    flags: INVALID,
                }
            } else {
                Outcome::exact(F80::new(sign, crate::f80::EXPONENT_MAX, significand))
            }
        }
        0 => {
            if fraction == 0 {
                Outcome::exact(F80::new(sign, 0, 0))
            } else {
                let shift = fraction.leading_zeros();
                Outcome {
                    value: F80::pack(sign, -1011 - shift as i32, fraction << shift),
                    flags: DENORMAL,
                }
            }
        }
        _ => Outcome::exact(F80::new(
            sign,
            (exponent - 1023 + BIAS) as u16,
            (1 << 63) | (fraction << 11),
        )),
    }
}

/// `fld m32`.
pub fn from_f32(bits: u32) -> Outcome {
    let sign = bits >> 31 != 0;
    let exponent = ((bits >> 23) & 0xFF) as i32;
    let fraction = (bits & ((1 << 23) - 1)) as u64;
    match exponent {
        0xFF => {
            let significand = (1 << 63) | (fraction << 40);
            if fraction == 0 {
                Outcome::exact(F80::new(sign, crate::f80::EXPONENT_MAX, significand))
            } else if fraction >> 22 == 0 {
                Outcome {
                    value: F80::new(sign, crate::f80::EXPONENT_MAX, significand | (1 << 62)),
                    flags: INVALID,
                }
            } else {
                Outcome::exact(F80::new(sign, crate::f80::EXPONENT_MAX, significand))
            }
        }
        0 => {
            if fraction == 0 {
                Outcome::exact(F80::new(sign, 0, 0))
            } else {
                let shift = fraction.leading_zeros();
                Outcome {
                    value: F80::pack(sign, -86 - shift as i32, fraction << shift),
                    flags: DENORMAL,
                }
            }
        }
        _ => Outcome::exact(F80::new(
            sign,
            (exponent - 127 + BIAS) as u16,
            (1 << 63) | (fraction << 40),
        )),
    }
}

/// `fild`, any width: always exact.
pub fn from_i64(value: i64) -> F80 {
    if value == 0 {
        return F80::ZERO;
    }
    let sign = value < 0;
    let magnitude = value.unsigned_abs();
    let shift = magnitude.leading_zeros();
    F80::new(
        sign,
        (63 - shift as i32 + BIAS) as u16,
        magnitude << shift,
    )
}

/// The narrowing shape shared by both float stores: significand width,
/// exponent bias and field width, and the format's indefinite pattern.
struct Narrow {
    fraction_bits: u32,
    bias: i32,
    exponent_max: i32,
    indefinite: u64,
}

const NARROW_F64: Narrow = Narrow {
    fraction_bits: 52,
    bias: 1023,
    exponent_max: 0x7FF,
    indefinite: 0xFFF8_0000_0000_0000,
};

const NARROW_F32: Narrow = Narrow {
    fraction_bits: 23,
    bias: 127,
    exponent_max: 0xFF,
    indefinite: 0xFFC0_0000,
};

/// `fst`/`fstp m64`.
pub fn to_f64(a: F80, rounding: Rounding) -> (u64, u16) {
    narrow(a, rounding, &NARROW_F64)
}

/// `fst`/`fstp m32`.
pub fn to_f32(a: F80, rounding: Rounding) -> (u32, u16) {
    let (bits, flags) = narrow(a, rounding, &NARROW_F32);
    (bits as u32, flags)
}

fn narrow(a: F80, rounding: Rounding, format: &Narrow) -> (u64, u16) {
    let class = a.classify();
    let sign_bit = (a.sign() as u64) << (format.fraction_bits + format.exponent_bits());
    match class {
        Class::Unsupported => return (format.indefinite, INVALID),
        Class::Zero => return (sign_bit, 0),
        Class::Infinity => {
            return (
                sign_bit | ((format.exponent_max as u64) << format.fraction_bits),
                0,
            );
        }
        Class::QuietNan | Class::SignallingNan => {
            let fraction = (a.significand & !(1 << 63)) >> (63 - format.fraction_bits);
            let quiet = 1 << (format.fraction_bits - 1);
            let flags = if class == Class::SignallingNan { INVALID } else { 0 };
            return (
                sign_bit
                    | ((format.exponent_max as u64) << format.fraction_bits)
                    | fraction
                    | quiet,
                flags,
            );
        }
        _ => {}
    }
    // No denormal flag for a subnormal source: the store family raises
    // #D for nothing (probed 2026-08-28) — that flag belongs to loads and
    // arithmetic.
    let mut flags = 0;
    let u = a.unpack();
    let biased = u.exp + format.bias;
    let tiny = biased <= 0;
    let drop = (63 - format.fraction_bits) as i32 + if tiny { 1 - biased } else { 0 };
    let (integer, inexact, above_half, tie) = split_tail(u.sig, drop);
    let increment = match rounding {
        Rounding::Nearest => above_half || (tie && integer & 1 != 0),
        Rounding::Down => u.sign && inexact,
        Rounding::Up => !u.sign && inexact,
        Rounding::Chop => false,
    };
    if inexact {
        flags |= PRECISION;
    }
    if increment {
        flags |= C1;
    }
    if tiny && inexact {
        flags |= UNDERFLOW;
    }
    let mut integer = integer + increment as u64;
    let mut biased = biased;
    if !tiny && integer >> (format.fraction_bits + 1) != 0 {
        // Rounding carried out of the significand.
        integer >>= 1;
        biased += 1;
    }
    if !tiny && biased >= format.exponent_max {
        flags = (flags & !C1) | OVERFLOW | PRECISION;
        let to_infinity = match rounding {
            Rounding::Nearest => true,
            Rounding::Chop => false,
            Rounding::Down => u.sign,
            Rounding::Up => !u.sign,
        };
        let bits = if to_infinity {
            flags |= C1;
            sign_bit | ((format.exponent_max as u64) << format.fraction_bits)
        } else {
            sign_bit
                | (((format.exponent_max - 1) as u64) << format.fraction_bits)
                | ((1 << format.fraction_bits) - 1)
        };
        return (bits, flags);
    }
    let bits = if tiny {
        // The denormal (or zero) path: the integer lands in the fraction
        // field directly, and a carry to one-past-the-top is exactly the
        // smallest normal.
        sign_bit | integer
    } else {
        sign_bit
            | ((biased as u64) << format.fraction_bits)
            | (integer & ((1 << format.fraction_bits) - 1))
    };
    (bits, flags)
}

impl Narrow {
    fn exponent_bits(&self) -> u32 {
        if self.fraction_bits == 52 { 11 } else { 8 }
    }
}

/// Splits a 64-bit significand `drop` bits above its bottom: the kept
/// integer part and the tail's shape (nonzero, above half, exactly half).
fn split_tail(sig: u64, drop: i32) -> (u64, bool, bool, bool) {
    if drop <= 0 {
        return (sig, false, false, false);
    }
    if drop >= 65 {
        return (0, sig != 0, false, false);
    }
    if drop == 64 {
        let half = 1u64 << 63;
        return (0, sig != 0, sig > half, sig == half);
    }
    let n = drop as u32;
    let integer = sig >> n;
    let tail = sig & ((1 << n) - 1);
    let half = 1u64 << (n - 1);
    (integer, tail != 0, tail > half, tail == half)
}

/// The `fist` family's shared core: round to integer per RC, check the
/// width, deliver the indefinite on invalid. The returned value is
/// sign-extended into i64 whatever the width.
pub fn to_int(a: F80, rounding: Rounding, width_bits: u32) -> (i64, u16) {
    let indefinite = match width_bits {
        16 => i16::MIN as i64,
        32 => i32::MIN as i64,
        _ => i64::MIN,
    };
    let class = a.classify();
    match class {
        Class::Zero => return (0, 0),
        Class::Normal | Class::Subnormal => {}
        _ => return (indefinite, INVALID),
    }
    // Like the float stores, the integer stores raise no denormal flag.
    let mut flags = 0;
    let u = a.unpack();
    if u.exp >= 63 {
        // Only −2^63 survives at the top of the widest format.
        if width_bits == 64 && u.sign && u.exp == 63 && u.sig == 1 << 63 {
            return (i64::MIN, flags);
        }
        return (indefinite, flags | INVALID);
    }
    let (integer, inexact, above_half, tie) = split_tail_unpacked(u);
    let increment = match rounding {
        Rounding::Nearest => above_half || (tie && integer & 1 != 0),
        Rounding::Down => u.sign && inexact,
        Rounding::Up => !u.sign && inexact,
        Rounding::Chop => false,
    };
    let magnitude = integer + increment as u64;
    let limit = if u.sign {
        1u64 << (width_bits - 1)
    } else {
        (1u64 << (width_bits - 1)) - 1
    };
    if magnitude > limit {
        return (indefinite, flags | INVALID);
    }
    if inexact {
        flags |= PRECISION;
    }
    if increment {
        flags |= C1;
    }
    let value = if u.sign {
        (magnitude as i64).wrapping_neg()
    } else {
        magnitude as i64
    };
    (value, flags)
}

fn split_tail_unpacked(u: Unpacked) -> (u64, bool, bool, bool) {
    split_tail(u.sig, 63 - u.exp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags;

    fn f(value: f64) -> F80 {
        from_f64(value.to_bits()).value
    }

    #[test]
    fn f64_round_trips_exactly() {
        for value in [
            0.0f64,
            -0.0,
            1.0,
            -1.5,
            0.1,
            f64::MAX,
            f64::MIN_POSITIVE,
            5e-324, // smallest denormal
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let wide = from_f64(value.to_bits()).value;
            let (bits, flags) = to_f64(wide, Rounding::Nearest);
            assert_eq!(bits, value.to_bits(), "{value}");
            assert_eq!(flags & flags::PRECISION, 0, "{value} came back inexact");
        }
    }

    #[test]
    fn f32_round_trips_exactly() {
        for value in [0.0f32, -2.5, 1e-40, f32::MAX, f32::INFINITY] {
            let wide = from_f32(value.to_bits()).value;
            let (bits, _) = to_f32(wide, Rounding::Nearest);
            assert_eq!(bits, value.to_bits(), "{value}");
        }
    }

    #[test]
    fn narrowing_rounds_and_flags() {
        // 1 + 2^-60 fits extended, not double.
        let wide = crate::arith::add(
            F80::ONE,
            F80::new(false, (BIAS - 60) as u16, 1 << 63),
            Rounding::Nearest,
            crate::Precision::Extended,
        )
        .value;
        let (bits, flags) = to_f64(wide, Rounding::Nearest);
        assert_eq!(f64::from_bits(bits), 1.0);
        assert!(flags & flags::PRECISION != 0);
        let (bits, flags) = to_f64(wide, Rounding::Up);
        assert_eq!(f64::from_bits(bits), 1.0 + f64::EPSILON);
        assert!(flags & C1 != 0);
    }

    #[test]
    fn narrowing_overflow_and_underflow() {
        let huge = F80::new(false, (BIAS + 2000) as u16, 1 << 63);
        let (bits, flags) = to_f64(huge, Rounding::Nearest);
        assert_eq!(f64::from_bits(bits), f64::INFINITY);
        assert!(flags & OVERFLOW != 0);
        let (bits, _) = to_f64(huge, Rounding::Chop);
        assert_eq!(f64::from_bits(bits), f64::MAX);
        let tiny = F80::new(false, (BIAS - 2000) as u16, 1 << 63);
        let (bits, flags) = to_f64(tiny, Rounding::Nearest);
        assert_eq!(f64::from_bits(bits), 0.0);
        assert!(flags & UNDERFLOW != 0 && flags & flags::PRECISION != 0);
        // A value that lands exactly on a double denormal is not an
        // underflow, because nothing was lost.
        let exact = f(5e-324);
        let (bits, flags) = to_f64(exact, Rounding::Nearest);
        assert_eq!(f64::from_bits(bits), 5e-324);
        assert_eq!(flags & (UNDERFLOW | flags::PRECISION), 0);
    }

    #[test]
    fn integers_round_trip() {
        // Every i64 fits the 64-bit significand exactly, extremes included.
        for value in [0i64, 1, -1, 42, i64::MAX, i64::MIN, i32::MIN as i64] {
            let wide = from_i64(value);
            let (back, flags) = to_int(wide, Rounding::Chop, 64);
            assert_eq!(back, value);
            assert_eq!(flags, 0, "{value}");
        }
    }

    #[test]
    fn integer_conversion_rounds_per_mode() {
        assert_eq!(to_int(f(2.5), Rounding::Nearest, 32).0, 2);
        assert_eq!(to_int(f(3.5), Rounding::Nearest, 32).0, 4);
        assert_eq!(to_int(f(-2.5), Rounding::Nearest, 32).0, -2);
        assert_eq!(to_int(f(2.9), Rounding::Chop, 32).0, 2);
        assert_eq!(to_int(f(-2.9), Rounding::Chop, 32).0, -2);
        assert_eq!(to_int(f(2.1), Rounding::Up, 32).0, 3);
        let (value, flags) = to_int(f(1e30), Rounding::Nearest, 32);
        assert_eq!(value, i32::MIN as i64);
        assert!(flags & INVALID != 0);
        let (value, flags) = to_int(f(40000.0), Rounding::Nearest, 16);
        assert_eq!(value, i16::MIN as i64);
        assert!(flags & INVALID != 0);
        assert_eq!(to_int(f(-32768.0), Rounding::Nearest, 16).0, -32768);
        let (value, flags) = to_int(F80::INDEFINITE, Rounding::Nearest, 64);
        assert_eq!(value, i64::MIN);
        assert!(flags & INVALID != 0);
    }
}
