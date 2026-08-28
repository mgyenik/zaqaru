//! Ordering and classification: the compare families and `fxam`'s
//! condition-code encoding.

use crate::f80::{Class, F80};
use crate::flags::{C0, C1, C2, C3, DENORMAL, INVALID};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Relation {
    Less,
    Equal,
    Greater,
    Unordered,
}

/// How loudly a comparison objects to NaNs: `fcom`/`fcomi` raise invalid
/// for any NaN; `fucom`/`fucomi` only for signalling ones (and both do for
/// unsupported patterns).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NanPolicy {
    Signalling,
    Quiet,
}

/// Compares two values, returning the relation and the flags (denormal
/// operands, and invalid per the policy).
pub fn compare(a: F80, b: F80, policy: NanPolicy) -> (Relation, u16) {
    let ca = a.classify();
    let cb = b.classify();
    let mut flags = 0u16;
    for class in [ca, cb] {
        if class == Class::Subnormal {
            flags |= DENORMAL;
        }
    }
    let loud = |class: Class| match policy {
        NanPolicy::Signalling => matches!(
            class,
            Class::QuietNan | Class::SignallingNan | Class::Unsupported
        ),
        NanPolicy::Quiet => matches!(class, Class::SignallingNan | Class::Unsupported),
    };
    if matches!(
        ca,
        Class::QuietNan | Class::SignallingNan | Class::Unsupported
    ) || matches!(
        cb,
        Class::QuietNan | Class::SignallingNan | Class::Unsupported
    ) {
        // A NaN operand suppresses the denormal flag the other operand
        // would have raised (probed 2026-08-28).
        let invalid = if loud(ca) || loud(cb) { INVALID } else { 0 };
        return (Relation::Unordered, invalid);
    }

    (order(a, ca, b, cb), flags)
}

fn order(a: F80, ca: Class, b: F80, cb: Class) -> Relation {
    let a_zero = ca == Class::Zero;
    let b_zero = cb == Class::Zero;
    if a_zero && b_zero {
        return Relation::Equal;
    }
    if a_zero {
        return if b.sign() { Relation::Greater } else { Relation::Less };
    }
    if b_zero {
        return if a.sign() { Relation::Less } else { Relation::Greater };
    }
    match (a.sign(), b.sign()) {
        (false, true) => return Relation::Greater,
        (true, false) => return Relation::Less,
        _ => {}
    }
    let magnitude = magnitude_order(a, ca, b, cb);
    if a.sign() { magnitude.flip() } else { magnitude }
}

impl Relation {
    fn flip(self) -> Self {
        match self {
            Relation::Less => Relation::Greater,
            Relation::Greater => Relation::Less,
            other => other,
        }
    }

    /// The C3/C2/C0 encoding the `fcom` family writes, which is also —
    /// deliberately, in the architecture — ZF/PF/CF once `fcomi` moved the
    /// same three answers into EFLAGS.
    pub fn condition_codes(self) -> u16 {
        match self {
            Relation::Greater => 0,
            Relation::Less => C0,
            Relation::Equal => C3,
            Relation::Unordered => C3 | C2 | C0,
        }
    }

    /// Packed EFLAGS for the translator: CF bit 0, PF bit 2, ZF bit 6.
    pub fn eflags(self) -> u32 {
        match self {
            Relation::Greater => 0,
            Relation::Less => 1,
            Relation::Equal => 1 << 6,
            Relation::Unordered => 1 | (1 << 2) | (1 << 6),
        }
    }
}

fn magnitude_order(a: F80, ca: Class, b: F80, cb: Class) -> Relation {
    match (ca == Class::Infinity, cb == Class::Infinity) {
        (true, true) => return Relation::Equal,
        (true, false) => return Relation::Greater,
        (false, true) => return Relation::Less,
        _ => {}
    }
    let ua = a.unpack();
    let ub = b.unpack();
    match (ua.exp, ua.sig).cmp(&(ub.exp, ub.sig)) {
        core::cmp::Ordering::Less => Relation::Less,
        core::cmp::Ordering::Equal => Relation::Equal,
        core::cmp::Ordering::Greater => Relation::Greater,
    }
}

/// `fxam`'s classification of a value that is present. The empty case is
/// the state's to report, since only the state knows the tags.
pub fn examine(a: F80) -> u16 {
    let sign = if a.sign() { C1 } else { 0 };
    sign | match a.classify() {
        Class::Unsupported => 0,
        Class::QuietNan | Class::SignallingNan => C0,
        Class::Normal => C2,
        Class::Infinity => C2 | C0,
        Class::Zero => C3,
        Class::Subnormal => C3 | C2,
    }
}

/// What `fxam` says about an empty register.
pub const EXAMINE_EMPTY: u16 = C3 | C0;

#[cfg(test)]
mod tests {
    use super::*;

    fn f(value: f64) -> F80 {
        crate::convert::from_f64(value.to_bits()).value
    }

    #[test]
    fn ordering() {
        let cases = [
            (1.0, 2.0, Relation::Less),
            (2.0, 1.0, Relation::Greater),
            (1.0, 1.0, Relation::Equal),
            (-1.0, 1.0, Relation::Less),
            (-1.0, -2.0, Relation::Greater),
            (0.0, -0.0, Relation::Equal),
            (0.0, 1e-310, Relation::Less),
            (f64::INFINITY, f64::MAX, Relation::Greater),
            (f64::NEG_INFINITY, f64::MIN, Relation::Less),
            (f64::INFINITY, f64::INFINITY, Relation::Equal),
        ];
        for (a, b, expected) in cases {
            let (relation, _) = compare(f(a), f(b), NanPolicy::Quiet);
            assert_eq!(relation, expected, "{a} vs {b}");
        }
    }

    #[test]
    fn nan_policies() {
        let quiet = F80::INDEFINITE;
        let (relation, flags) = compare(quiet, f(1.0), NanPolicy::Quiet);
        assert_eq!(relation, Relation::Unordered);
        assert_eq!(flags & INVALID, 0, "fucom tolerates quiet NaNs");
        let (_, flags) = compare(quiet, f(1.0), NanPolicy::Signalling);
        assert!(flags & INVALID != 0, "fcom does not");
        let signalling = F80::new(false, crate::f80::EXPONENT_MAX, (1 << 63) | 1);
        let (_, flags) = compare(signalling, f(1.0), NanPolicy::Quiet);
        assert!(flags & INVALID != 0);
    }

    #[test]
    fn examine_classes() {
        assert_eq!(examine(f(1.0)), C2);
        assert_eq!(examine(f(-1.0)), C2 | C1);
        assert_eq!(examine(f(0.0)), C3);
        assert_eq!(examine(f(f64::INFINITY)), C2 | C0);
        assert_eq!(examine(F80::INDEFINITE), C0 | C1);
        assert_eq!(examine(F80::new(false, 0, 5)), C3 | C2);
    }
}
