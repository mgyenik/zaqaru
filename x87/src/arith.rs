//! The extended-precision softfloat core: rounding, add/sub/mul/div, square
//! root, the partial remainder, round-to-integer, scale and extract.
//!
//! Everything here is bit-exact against the hardware, enforced by the
//! host-FPU oracle in `tests/oracle.rs`. The working representation is
//! [`Unpacked`] — normalized 64-bit significand, unbiased exponent — with
//! intermediate results carried as a 128-bit fixed-point significand
//! (integer bit at bit 127) plus the exponent that places it.

use crate::f80::{BIAS, Class, F80, Unpacked};
use crate::flags::{
    C0, C1, C2, C3, DENORMAL, INVALID, OVERFLOW, PRECISION, UNDERFLOW, ZERO_DIVIDE,
};
use crate::{Precision, Rounding};

/// A value and the exception flags computing it raised, in FSW bit
/// positions. `C1` doubles as the rounded-up indicator.
#[derive(Clone, Copy, Debug)]
pub struct Outcome {
    pub value: F80,
    pub flags: u16,
}

impl Outcome {
    pub fn exact(value: F80) -> Self {
        Self { value, flags: 0 }
    }

    fn with(value: F80, flags: u16) -> Self {
        Self { value, flags }
    }
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

/// The x87 two-operand NaN rule: an unsupported pattern forces the real
/// indefinite; two NaNs resolve to the larger significand (ties to the
/// first operand); the survivor is quieted. Invalid is raised for
/// signalling and unsupported operands.
pub(crate) fn propagate(a: F80, b: F80) -> Outcome {
    let ca = a.classify();
    let cb = b.classify();
    let mut flags = 0;
    if matches!(ca, Class::SignallingNan | Class::Unsupported)
        || matches!(cb, Class::SignallingNan | Class::Unsupported)
    {
        flags |= INVALID;
    }
    let value = if ca == Class::Unsupported || cb == Class::Unsupported {
        F80::INDEFINITE
    } else {
        match (a.is_nan(), b.is_nan()) {
            (true, true) => {
                if b.significand > a.significand {
                    b.quieted()
                } else {
                    a.quieted()
                }
            }
            (true, false) => a.quieted(),
            (false, true) => b.quieted(),
            (false, false) => unreachable!("propagate called without a NaN"),
        }
    };
    Outcome::with(value, flags)
}

/// The single-operand form of [`propagate`].
pub(crate) fn propagate_one(a: F80) -> Outcome {
    match a.classify() {
        Class::Unsupported => Outcome::with(F80::INDEFINITE, INVALID),
        Class::SignallingNan => Outcome::with(a.quieted(), INVALID),
        Class::QuietNan => Outcome::exact(a),
        _ => unreachable!("propagate_one called on an ordinary value"),
    }
}

/// Right shift that ORs everything shifted out into the lowest bit — the
/// classic sticky shift.
fn shift_right_jam(value: u128, shift: u32) -> u128 {
    if shift == 0 {
        value
    } else if shift >= 128 {
        (value != 0) as u128
    } else {
        (value >> shift) | ((value << (128 - shift) != 0) as u128)
    }
}

/// Rounds and packs a normalized result. The value is
/// `wide × 2^(exp − 127)` with `wide`'s integer bit at bit 127; `flags`
/// come back with the rounding's contributions (precision, underflow,
/// overflow, C1) ORed in.
pub(crate) fn round_pack(
    sign: bool,
    exp: i32,
    wide: u128,
    rounding: Rounding,
    precision: Precision,
) -> Outcome {
    debug_assert!(wide >> 127 != 0);
    let mut flags = 0u16;
    let mut exp = exp;
    let mut wide = wide;

    // Extended-range tininess: denormalize before rounding, at full width,
    // with everything shifted out kept sticky.
    let minimum = 1 - BIAS;
    let tiny = exp < minimum;
    if tiny {
        wide = shift_right_jam(wide, (minimum - exp).min(255) as u32);
        exp = minimum;
    }

    let mut sig = (wide >> 64) as u64;
    let rest = wide as u64;

    // The tail below the rounding position: precision control narrows the
    // position but never the range.
    let mask = precision.tail_mask();
    let tail = (((sig & mask) as u128) << 64) | rest as u128;
    let half = ((mask as u128 + 1) << 64) >> 1;
    let increment = match rounding {
        Rounding::Nearest => {
            tail > half || (tail == half && sig & (mask.wrapping_add(1) | 1) != 0)
        }
        Rounding::Down => sign && tail != 0,
        Rounding::Up => !sign && tail != 0,
        Rounding::Chop => false,
    };
    let inexact = tail != 0;
    if inexact {
        flags |= PRECISION;
    }
    if tiny && inexact {
        flags |= UNDERFLOW;
    }

    sig &= !mask;
    if increment {
        flags |= C1;
        let (incremented, carry) = sig.overflowing_add(mask.wrapping_add(1).max(1));
        sig = incremented;
        if carry {
            sig = 1 << 63;
            exp += 1;
        }
    }

    if exp > BIAS {
        flags |= OVERFLOW | PRECISION;
        let to_infinity = match rounding {
            Rounding::Nearest => true,
            Rounding::Chop => false,
            Rounding::Down => sign,
            Rounding::Up => !sign,
        };
        let value = if to_infinity {
            flags |= C1;
            F80::new(sign, crate::f80::EXPONENT_MAX, 1 << 63)
        } else {
            flags &= !C1;
            F80::new(sign, 0x7FFE, !mask)
        };
        return Outcome::with(value, flags);
    }

    let value = if sig == 0 {
        // Underflowed all the way out.
        F80::new(sign, 0, 0)
    } else if sig & (1 << 63) == 0 {
        // A denormal: exponent field zero, same effective exponent.
        debug_assert!(exp == minimum);
        F80::new(sign, 0, sig)
    } else {
        F80::new(sign, (exp + BIAS) as u16, sig)
    };
    Outcome::with(value, flags)
}

/// A signed zero, with the sign the rounding mode gives an exact
/// cancellation.
fn cancellation_zero(rounding: Rounding) -> F80 {
    F80::new(rounding == Rounding::Down, 0, 0)
}

pub fn add(a: F80, b: F80, rounding: Rounding, precision: Precision) -> Outcome {
    add_signed(a, b, false, rounding, precision)
}

pub fn sub(a: F80, b: F80, rounding: Rounding, precision: Precision) -> Outcome {
    add_signed(a, b, true, rounding, precision)
}

fn add_signed(
    a: F80,
    b: F80,
    negate_b: bool,
    rounding: Rounding,
    precision: Precision,
) -> Outcome {
    let ca = a.classify();
    let cb = b.classify();
    if is_invalid_operand(ca) || is_invalid_operand(cb) {
        return propagate(a, b);
    }
    let b = if negate_b { b.negate() } else { b };
    let flags = denormal_flag(ca) | denormal_flag(cb);

    match (ca, cb) {
        (Class::Infinity, Class::Infinity) => {
            if a.sign() == b.sign() {
                Outcome::with(a, flags)
            } else {
                Outcome::with(F80::INDEFINITE, flags | INVALID)
            }
        }
        (Class::Infinity, _) => Outcome::with(a, flags),
        (_, Class::Infinity) => Outcome::with(b, flags),
        (Class::Zero, Class::Zero) => {
            let value = if a.sign() == b.sign() {
                F80::new(a.sign(), 0, 0)
            } else {
                cancellation_zero(rounding)
            };
            Outcome::with(value, flags)
        }
        (Class::Zero, _) => {
            let u = b.unpack();
            let out = round_pack(u.sign, u.exp, (u.sig as u128) << 64, rounding, precision);
            Outcome::with(out.value, out.flags | flags)
        }
        (_, Class::Zero) => {
            let u = a.unpack();
            let out = round_pack(u.sign, u.exp, (u.sig as u128) << 64, rounding, precision);
            Outcome::with(out.value, out.flags | flags)
        }
        _ => {
            let ua = a.unpack();
            let ub = b.unpack();
            let (big, small) = if (ua.exp, ua.sig) >= (ub.exp, ub.sig) {
                (ua, ub)
            } else {
                (ub, ua)
            };
            let big128 = (big.sig as u128) << 64;
            let small128 =
                shift_right_jam((small.sig as u128) << 64, (big.exp - small.exp) as u32);
            if ua.sign == ub.sign {
                let (sum, carry) = big128.overflowing_add(small128);
                let (wide, exp) = if carry {
                    ((sum >> 1) | (1u128 << 127) | (sum & 1), big.exp + 1)
                } else {
                    (sum, big.exp)
                };
                let out = round_pack(big.sign, exp, wide, rounding, precision);
                Outcome::with(out.value, out.flags | flags)
            } else {
                let difference = big128 - small128;
                if difference == 0 {
                    return Outcome::with(cancellation_zero(rounding), flags);
                }
                let shift = difference.leading_zeros();
                let out = round_pack(
                    big.sign,
                    big.exp - shift as i32,
                    difference << shift,
                    rounding,
                    precision,
                );
                Outcome::with(out.value, out.flags | flags)
            }
        }
    }
}

pub fn mul(a: F80, b: F80, rounding: Rounding, precision: Precision) -> Outcome {
    let ca = a.classify();
    let cb = b.classify();
    if is_invalid_operand(ca) || is_invalid_operand(cb) {
        return propagate(a, b);
    }
    let sign = a.sign() != b.sign();
    let flags = denormal_flag(ca) | denormal_flag(cb);
    match (ca, cb) {
        (Class::Infinity, Class::Zero) | (Class::Zero, Class::Infinity) => {
            Outcome::with(F80::INDEFINITE, flags | INVALID)
        }
        (Class::Infinity, _) | (_, Class::Infinity) => {
            Outcome::with(F80::new(sign, crate::f80::EXPONENT_MAX, 1 << 63), flags)
        }
        (Class::Zero, _) | (_, Class::Zero) => Outcome::with(F80::new(sign, 0, 0), flags),
        _ => {
            let ua = a.unpack();
            let ub = b.unpack();
            let product = (ua.sig as u128) * (ub.sig as u128);
            let (wide, exp) = if product >> 127 != 0 {
                (product, ua.exp + ub.exp + 1)
            } else {
                (product << 1, ua.exp + ub.exp)
            };
            let out = round_pack(sign, exp, wide, rounding, precision);
            Outcome::with(out.value, out.flags | flags)
        }
    }
}

pub fn div(a: F80, b: F80, rounding: Rounding, precision: Precision) -> Outcome {
    let ca = a.classify();
    let cb = b.classify();
    if is_invalid_operand(ca) || is_invalid_operand(cb) {
        return propagate(a, b);
    }
    let sign = a.sign() != b.sign();
    let flags = denormal_flag(ca) | denormal_flag(cb);
    match (ca, cb) {
        (Class::Infinity, Class::Infinity) | (Class::Zero, Class::Zero) => {
            Outcome::with(F80::INDEFINITE, flags | INVALID)
        }
        (Class::Infinity, _) => {
            Outcome::with(F80::new(sign, crate::f80::EXPONENT_MAX, 1 << 63), flags)
        }
        (_, Class::Infinity) => Outcome::with(F80::new(sign, 0, 0), flags),
        (Class::Zero, _) => Outcome::with(F80::new(sign, 0, 0), flags),
        // The denormal flag is dropped here, not an oversight: the
        // hardware reports only the higher-priority zero-divide (probed
        // 2026-08-28 — denormal/0 raises ZE alone), and the same
        // suppression holds wherever invalid is raised.
        (_, Class::Zero) => Outcome::with(
            F80::new(sign, crate::f80::EXPONENT_MAX, 1 << 63),
            ZERO_DIVIDE,
        ),
        _ => {
            let ua = a.unpack();
            let ub = b.unpack();
            let (dividend, exp) = if ua.sig >= ub.sig {
                ((ua.sig as u128) << 63, ua.exp - ub.exp)
            } else {
                ((ua.sig as u128) << 64, ua.exp - ub.exp - 1)
            };
            let divisor = ub.sig as u128;
            let quotient = (dividend / divisor) as u64;
            let remainder = dividend % divisor;
            let low_dividend = remainder << 64;
            let low = (low_dividend / divisor) as u64;
            let sticky = (low_dividend % divisor != 0) as u64;
            let wide = ((quotient as u128) << 64) | (low | sticky) as u128;
            let out = round_pack(sign, exp, wide, rounding, precision);
            Outcome::with(out.value, out.flags | flags)
        }
    }
}

/// Restoring square root of a 128-bit radicand: the floor root (which has
/// at most 64 bits) and the remainder.
fn isqrt128(x: u128) -> (u64, u128) {
    let mut root: u128 = 0;
    let mut remainder: u128 = 0;
    let mut i = 64;
    while i > 0 {
        i -= 1;
        remainder = (remainder << 2) | ((x >> (2 * i)) & 3);
        let trial = (root << 2) | 1;
        root <<= 1;
        if remainder >= trial {
            remainder -= trial;
            root += 1;
        }
    }
    (root as u64, remainder)
}

pub fn sqrt(a: F80, rounding: Rounding, precision: Precision) -> Outcome {
    let class = a.classify();
    if is_invalid_operand(class) {
        return propagate_one(a);
    }
    match class {
        Class::Zero => return Outcome::exact(a),
        Class::Infinity if !a.sign() => return Outcome::exact(a),
        _ => {}
    }
    if a.sign() {
        return Outcome::with(F80::INDEFINITE, INVALID);
    }
    let flags = denormal_flag(class);
    let u = a.unpack();
    // sqrt(m·2^e): fold the exponent's parity into the radicand so the
    // integer root lands normalized in [2^63, 2^64).
    let (radicand, exp) = if u.exp & 1 == 0 {
        ((u.sig as u128) << 63, u.exp / 2)
    } else {
        ((u.sig as u128) << 64, (u.exp - 1) / 2)
    };
    let (root, remainder) = isqrt128(radicand);
    // One more result bit from the remainder: the root of the radicand at
    // four times the scale is 2·root, +1 exactly when the remainder covers
    // the cost of the odd extension.
    let guard = remainder > root as u128;
    let below = if guard {
        4 * remainder - 4 * (root as u128) - 1
    } else {
        4 * remainder
    };
    let rest = ((guard as u64) << 63) | (below != 0) as u64;
    let wide = ((root as u128) << 64) | rest as u128;
    let out = round_pack(false, exp, wide, rounding, precision);
    Outcome::with(out.value, out.flags | flags)
}

/// `fprem`/`fprem1`. The flags carry the full condition protocol: C2 set on
/// an incomplete (partial) reduction; C2 clear with the quotient's low
/// three bits in C0/C3/C1 on completion. The result is always exact, so
/// rounding control plays no part.
pub fn partial_remainder(a: F80, b: F80, nearest: bool) -> Outcome {
    let ca = a.classify();
    let cb = b.classify();
    if is_invalid_operand(ca) || is_invalid_operand(cb) {
        return propagate(a, b);
    }
    let flags = denormal_flag(ca) | denormal_flag(cb);
    match (ca, cb) {
        // Invalid suppresses the denormal flag, same as division's
        // zero-divide does.
        (Class::Infinity, _) | (_, Class::Zero) => {
            return Outcome::with(F80::INDEFINITE, INVALID);
        }
        (_, Class::Infinity) | (Class::Zero, _) => {
            return Outcome::with(canonicalized(a, ca), flags);
        }
        _ => {}
    }

    let ua = a.unpack();
    let ub = b.unpack();
    let expdiff = ua.exp - ub.exp;

    if expdiff < -1 {
        // |a| < |b|/2: the quotient is zero under both rounding rules.
        return Outcome::with(canonicalized(a, ca), flags);
    }
    if expdiff == -1 {
        // fprem truncates to zero; fprem1 may round the quotient to ±1
        // when |a| is more than half of |b| (ties go to the even zero).
        if !nearest || ua.sig <= ub.sig {
            return Outcome::with(canonicalized(a, ca), flags);
        }
        // remainder = |b| − |a|, sign flipped; C1 carries the quotient's
        // low bit.
        let wide = ((ub.sig as u128) << 1) - ua.sig as u128;
        return Outcome::with(
            pack_exact(!ua.sign, ua.exp, wide),
            flags | C1,
        );
    }

    // A complete reduction takes all the quotient bits at once. A partial
    // one takes 32 + (D mod 32) of them, leaving a multiple of 32 for the
    // next pass — measured against this hardware (probed 2026-08-28,
    // D = 64..133 and 6670), and squarely inside the manual's
    // "implementation-dependent number between 32 and 63".
    let partial = expdiff > 63;
    let steps = if partial { 32 + expdiff % 32 } else { expdiff } as u32;
    let divisor = ub.sig as u128;
    let mut remainder = ua.sig as u128;
    let mut quotient: u64 = 0;
    if remainder >= divisor {
        remainder -= divisor;
        quotient = 1;
    }
    for _ in 0..steps {
        remainder <<= 1;
        quotient <<= 1;
        if remainder >= divisor {
            remainder -= divisor;
            quotient |= 1;
        }
    }

    let mut sign = ua.sign;
    let mut condition = 0u16;
    let mut scale = 0i32;
    if partial {
        condition |= C2;
        scale = expdiff - steps as i32;
    } else if nearest {
        let doubled = remainder << 1;
        if doubled > divisor || (doubled == divisor && quotient & 1 != 0) {
            quotient = quotient.wrapping_add(1);
            remainder = divisor - remainder;
            sign = !sign;
        }
    }
    if !partial {
        condition |= if quotient & 4 != 0 { C0 } else { 0 };
        condition |= if quotient & 2 != 0 { C3 } else { 0 };
        condition |= if quotient & 1 != 0 { C1 } else { 0 };
    }

    if remainder == 0 {
        return Outcome::with(F80::new(sign, 0, 0), flags | condition);
    }
    // The remainder sits at the divisor's fixed point: its value is
    // remainder × 2^(exp − 63) at the divisor's exponent, shifted further
    // by the partial reduction's leftover scale.
    Outcome::with(
        pack_exact(sign, ub.exp + scale, remainder),
        flags | condition,
    )
}

/// A value passed back through the FPU's packer: the one visible effect is
/// that a pseudo-denormal comes out as the equivalent smallest-exponent
/// normal, which is what the hardware delivers when `fprem` returns ST0
/// unreduced (probed 2026-08-28).
fn canonicalized(a: F80, class: Class) -> F80 {
    match class {
        Class::Normal | Class::Subnormal => {
            let u = a.unpack();
            F80::pack(u.sign, u.exp, u.sig)
        }
        _ => a,
    }
}

/// Packs `raw × 2^(exp − 63)` exactly, normalizing in either direction and
/// denormalizing below the extended range. The value must be exactly
/// representable — remainders are — and a lost bit is a bug.
fn pack_exact(sign: bool, exp: i32, raw: u128) -> F80 {
    debug_assert!(raw != 0);
    let shift = raw.leading_zeros() as i32 - 64;
    // shift > 0: normalize left; shift < 0: the value carried bits above
    // the integer position (the fprem1 −1 case) and moves right, exactly.
    let (sig, exp) = if shift >= 0 {
        ((raw << shift) as u64, exp - shift)
    } else {
        debug_assert!(raw & ((1 << (-shift)) - 1) == 0);
        ((raw >> -shift) as u64, exp - shift)
    };
    let minimum = 1 - BIAS;
    if exp < minimum {
        let down = (minimum - exp) as u32;
        debug_assert!(down < 64 && sig << (64 - down) == 0);
        F80::new(sign, 0, sig >> down)
    } else {
        F80::new(sign, (exp + BIAS) as u16, sig)
    }
}

/// `frndint`.
pub fn round_to_int(a: F80, rounding: Rounding) -> Outcome {
    let class = a.classify();
    if is_invalid_operand(class) {
        return propagate_one(a);
    }
    if matches!(class, Class::Zero | Class::Infinity) {
        return Outcome::exact(a);
    }
    let flags = denormal_flag(class);
    let u = a.unpack();
    if u.exp >= 63 {
        return Outcome::with(a, flags);
    }
    let (integer, inexact, above_half, tie) = split_at_integer(u);
    let increment = match rounding {
        Rounding::Nearest => above_half || (tie && integer & 1 != 0),
        Rounding::Down => u.sign && inexact,
        Rounding::Up => !u.sign && inexact,
        Rounding::Chop => false,
    };
    let mut flags = flags;
    if inexact {
        flags |= PRECISION;
    }
    if increment {
        flags |= C1;
    }
    let result = integer + increment as u64;
    if result == 0 {
        return Outcome::with(F80::new(u.sign, 0, 0), flags);
    }
    let shift = result.leading_zeros();
    Outcome::with(
        F80::new(u.sign, (63 - shift as i32 + BIAS) as u16, result << shift),
        flags,
    )
}

/// The integer part of an unpacked finite value below 2^63, plus what the
/// fractional tail looks like: nonzero, above half, exactly half.
fn split_at_integer(u: Unpacked) -> (u64, bool, bool, bool) {
    debug_assert!(u.exp < 63);
    let fractional_bits = 63 - u.exp;
    if fractional_bits >= 65 {
        // |value| < 1/2: integer zero, tail strictly below half.
        (0, true, false, false)
    } else if fractional_bits == 64 {
        // 1/2 ≤ |value| < 1: the half position is the significand's top.
        let half = 1u64 << 63;
        (0, true, u.sig > half, u.sig == half)
    } else {
        let n = fractional_bits as u32;
        let integer = u.sig >> n;
        let tail = u.sig & ((1 << n) - 1);
        let half = 1u64 << (n - 1);
        (integer, tail != 0, tail > half, tail == half)
    }
}

/// `fscale`: ST0 × 2^trunc(ST1). Exact — precision control does not apply
/// (probed 2026-08-28: the hardware keeps the full significand under
/// PC = single) — except for the range effects the rounding machinery
/// already handles.
pub fn scale(a: F80, b: F80, rounding: Rounding, _precision: Precision) -> Outcome {
    let ca = a.classify();
    let cb = b.classify();
    if is_invalid_operand(ca) || is_invalid_operand(cb) {
        return propagate(a, b);
    }
    let flags = denormal_flag(ca) | denormal_flag(cb);
    if cb == Class::Infinity {
        return match (b.sign(), ca) {
            (false, Class::Zero) | (true, Class::Infinity) => {
                Outcome::with(F80::INDEFINITE, INVALID)
            }
            (false, _) => Outcome::with(
                F80::new(a.sign(), crate::f80::EXPONENT_MAX, 1 << 63),
                flags,
            ),
            (true, _) => Outcome::with(F80::new(a.sign(), 0, 0), flags),
        };
    }
    if matches!(ca, Class::Zero | Class::Infinity) {
        return Outcome::with(a, flags);
    }
    let shift = truncated_exponent_shift(b, cb);
    let u = a.unpack();
    let exp = (u.exp as i64 + shift).clamp(-1_000_000, 1_000_000) as i32;
    let out = round_pack(
        u.sign,
        exp,
        (u.sig as u128) << 64,
        rounding,
        Precision::Extended,
    );
    Outcome::with(out.value, out.flags | flags)
}

/// trunc(ST1) as a shift amount, saturated far past the exponent range so
/// the clamp in `scale` is what bounds it.
fn truncated_exponent_shift(b: F80, class: Class) -> i64 {
    if class == Class::Zero {
        return 0;
    }
    let u = b.unpack();
    let magnitude = if u.exp < 0 {
        0
    } else if u.exp >= 63 {
        1 << 30
    } else {
        (u.sig >> (63 - u.exp)) as i64
    };
    if u.sign { -magnitude } else { magnitude }
}

/// `fxtract`: the significand as a value in [1, 2) and the unbiased
/// exponent as a value. Zero yields (±0, −∞) with a zero-divide; the flags
/// are shared across both results.
pub fn extract(a: F80) -> (F80, F80, u16) {
    let class = a.classify();
    if is_invalid_operand(class) {
        let out = propagate_one(a);
        return (out.value, out.value, out.flags);
    }
    match class {
        Class::Zero => (
            a,
            F80::new(true, crate::f80::EXPONENT_MAX, 1 << 63),
            ZERO_DIVIDE,
        ),
        Class::Infinity => (a, F80::new(false, crate::f80::EXPONENT_MAX, 1 << 63), 0),
        _ => {
            let u = a.unpack();
            (
                F80::new(u.sign, BIAS as u16, u.sig),
                crate::convert::from_i64(u.exp as i64),
                denormal_flag(class),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(value: f64) -> F80 {
        crate::convert::from_f64(value.to_bits()).value
    }

    fn assert_value(outcome: Outcome, expected: f64) {
        let (bits, _) = crate::convert::to_f64(outcome.value, Rounding::Nearest);
        assert_eq!(f64::from_bits(bits), expected, "{outcome:?}");
    }

    #[test]
    fn small_integers() {
        let (r, p) = (Rounding::Nearest, Precision::Extended);
        assert_value(add(f(1.0), f(2.0), r, p), 3.0);
        assert_value(sub(f(1.0), f(2.0), r, p), -1.0);
        assert_value(mul(f(3.0), f(7.0), r, p), 21.0);
        assert_value(div(f(1.0), f(4.0), r, p), 0.25);
        assert_value(sqrt(f(9.0), r, p), 3.0);
        assert_eq!(add(f(1.0), f(2.0), r, p).flags, 0);
    }

    #[test]
    fn exact_cancellation_sign_follows_rounding() {
        let p = Precision::Extended;
        let plus = sub(f(1.0), f(1.0), Rounding::Nearest, p).value;
        assert_eq!(plus, F80::ZERO);
        let minus = sub(f(1.0), f(1.0), Rounding::Down, p).value;
        assert_eq!(minus, F80::ZERO.negate());
    }

    #[test]
    fn division_is_correctly_rounded() {
        // 1/3 in extended: significand AAAA...AAAB under nearest (the tail
        // is 0101… which is over half at the cut), AAAA...AAAA chopped.
        let nearest = div(f(1.0), f(3.0), Rounding::Nearest, Precision::Extended);
        assert_eq!(nearest.value.significand, 0xAAAA_AAAA_AAAA_AAAB);
        assert!(nearest.flags & PRECISION != 0);
        assert!(nearest.flags & C1 != 0);
        let chopped = div(f(1.0), f(3.0), Rounding::Chop, Precision::Extended);
        assert_eq!(chopped.value.significand, 0xAAAA_AAAA_AAAA_AAAA);
        assert!(chopped.flags & C1 == 0);
    }

    #[test]
    fn precision_control_narrows_the_rounding() {
        // 1 + 2^-60 is exact in extended, rounds away in double.
        let tiny = F80::new(false, (BIAS - 60) as u16, 1 << 63);
        let extended = add(F80::ONE, tiny, Rounding::Nearest, Precision::Extended);
        assert_eq!(extended.flags & PRECISION, 0);
        let double = add(F80::ONE, tiny, Rounding::Nearest, Precision::Double);
        assert_eq!(double.value, F80::ONE);
        assert!(double.flags & PRECISION != 0);
    }

    #[test]
    fn overflow_masked_results_depend_on_rounding() {
        let max = F80::new(false, 0x7FFE, u64::MAX);
        let nearest = add(max, max, Rounding::Nearest, Precision::Extended);
        assert_eq!(nearest.value.exponent(), crate::f80::EXPONENT_MAX);
        assert!(nearest.flags & OVERFLOW != 0);
        let chopped = add(max, max, Rounding::Chop, Precision::Extended);
        assert_eq!(chopped.value, max);
    }

    #[test]
    fn underflow_denormalizes() {
        let smallest_normal = F80::new(false, 1, 1 << 63);
        let half = f(0.5);
        let out = mul(smallest_normal, half, Rounding::Nearest, Precision::Extended);
        assert_eq!(out.value, F80::new(false, 0, 1 << 62));
        // Exact denormalization: no underflow flag without inexactness.
        assert_eq!(out.flags & (UNDERFLOW | PRECISION), 0);
    }

    #[test]
    fn zero_divide_and_invalid() {
        let out = div(f(1.0), F80::ZERO, Rounding::Nearest, Precision::Extended);
        assert!(out.flags & ZERO_DIVIDE != 0);
        assert_eq!(out.value.classify(), Class::Infinity);
        let out = div(F80::ZERO, F80::ZERO, Rounding::Nearest, Precision::Extended);
        assert!(out.flags & INVALID != 0);
        assert_eq!(out.value, F80::INDEFINITE);
    }

    #[test]
    fn remainder_protocol() {
        // 7 rem 2 = 1, quotient 3: C0 clear, C3 set, C1 set.
        let out = partial_remainder(f(7.0), f(2.0), false);
        assert_value(out, 1.0);
        assert_eq!(out.flags & C2, 0);
        assert_eq!(out.flags & C3, C3);
        assert_eq!(out.flags & C1, C1);
        assert_eq!(out.flags & C0, 0);
        // fprem1: 5 rem 2 rounds the quotient to 2, remainder 1... the
        // nearest quotient of 2.5 is 2 (ties-even), remainder +1.
        let out = partial_remainder(f(5.0), f(2.0), true);
        assert_value(out, 1.0);
        // fprem1: 7 rem 2 has quotient 3.5 → 4, remainder −1.
        let out = partial_remainder(f(7.0), f(2.0), true);
        assert_value(out, -1.0);
        // A huge exponent gap leaves the reduction incomplete.
        let big = F80::new(false, (BIAS + 200) as u16, 1 << 63);
        let out = partial_remainder(big, f(3.0), false);
        assert_eq!(out.flags & C2, C2);
    }

    #[test]
    fn rounding_to_integer() {
        assert_value(round_to_int(f(2.5), Rounding::Nearest), 2.0);
        assert_value(round_to_int(f(3.5), Rounding::Nearest), 4.0);
        assert_value(round_to_int(f(-2.5), Rounding::Nearest), -2.0);
        assert_value(round_to_int(f(2.7), Rounding::Chop), 2.0);
        assert_value(round_to_int(f(-2.7), Rounding::Chop), -2.0);
        assert_value(round_to_int(f(2.2), Rounding::Up), 3.0);
        assert_value(round_to_int(f(-2.2), Rounding::Down), -3.0);
        assert_value(round_to_int(f(0.3), Rounding::Nearest), 0.0);
        let out = round_to_int(f(-0.3), Rounding::Nearest);
        assert!(out.value.sign(), "negative zero keeps its sign");
        assert_eq!(round_to_int(f(4.0), Rounding::Nearest).flags & PRECISION, 0);
    }

    #[test]
    fn scale_and_extract() {
        let out = scale(f(3.0), f(4.9), Rounding::Nearest, Precision::Extended);
        assert_value(out, 48.0);
        let out = scale(f(3.0), f(-2.0), Rounding::Nearest, Precision::Extended);
        assert_value(out, 0.75);
        let (mantissa, exponent, flags) = extract(f(48.0));
        assert_eq!(flags, 0);
        let (bits, _) = crate::convert::to_f64(mantissa, Rounding::Nearest);
        assert_eq!(f64::from_bits(bits), 1.5);
        let (bits, _) = crate::convert::to_f64(exponent, Rounding::Nearest);
        assert_eq!(f64::from_bits(bits), 5.0);
    }

    #[test]
    fn nan_rules() {
        let quiet_small = F80::new(false, crate::f80::EXPONENT_MAX, 0xC000_0000_0000_0001);
        let quiet_large = F80::new(true, crate::f80::EXPONENT_MAX, 0xC000_0000_0000_0002);
        let out = add(quiet_small, quiet_large, Rounding::Nearest, Precision::Extended);
        assert_eq!(out.value, quiet_large, "larger significand wins");
        assert_eq!(out.flags, 0, "quiet NaNs raise nothing");
        let signalling = F80::new(false, crate::f80::EXPONENT_MAX, (1 << 63) | 5);
        let out = add(signalling, f(1.0), Rounding::Nearest, Precision::Extended);
        assert!(out.flags & INVALID != 0);
        assert_eq!(out.value.classify(), Class::QuietNan);
        let unnormal = F80::new(false, 100, 1 << 62);
        let out = add(unnormal, f(1.0), Rounding::Nearest, Precision::Extended);
        assert_eq!(out.value, F80::INDEFINITE);
        assert!(out.flags & INVALID != 0);
    }
}
