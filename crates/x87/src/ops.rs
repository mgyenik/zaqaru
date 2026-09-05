//! Instruction-level semantics: what each translated x87 instruction asks
//! the state to do. These methods are what the [`crate::ffi`] helpers call,
//! and what native tests drive on plain instances.
//!
//! The fault model runs through [`X87State::read`]/[`X87State::push`]: an
//! empty or full register records its stack fault and substitutes the
//! indefinite, and the operation then proceeds on the masked operand —
//! which is exactly the architecture's masked behavior.

use crate::arith::{self, Outcome};
use crate::compare::{self, NanPolicy, Relation};
use crate::convert;
use crate::f80::F80;
use crate::state::X87State;
use crate::transcendental;
use crate::{Precision, Rounding};

/// The two-operand arithmetic family, reversed forms included so the
/// direction is stated once, here, rather than at every call site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Binary {
    Add,
    Sub,
    SubReverse,
    Mul,
    Div,
    DivReverse,
}

impl Binary {
    fn apply(self, a: F80, b: F80, rounding: Rounding, precision: Precision) -> Outcome {
        match self {
            Binary::Add => arith::add(a, b, rounding, precision),
            Binary::Sub => arith::sub(a, b, rounding, precision),
            Binary::SubReverse => arith::sub(b, a, rounding, precision),
            Binary::Mul => arith::mul(a, b, rounding, precision),
            Binary::Div => arith::div(a, b, rounding, precision),
            Binary::DivReverse => arith::div(b, a, rounding, precision),
        }
    }
}

/// The flags a memory operand's *conversion* contributes, given what it is
/// about to meet.
///
/// A denormal `m32`/`m64` raises the denormal-operand exception on its way
/// into the eighty-bit format — but not when the other operand is a NaN,
/// because the NaN path is taken before the denormal one is signalled.
/// Measured 2026-08-30 against hardware, for `fadd`, `fmul` and `fcom`
/// alike, in the register form as well as the memory form; it is the same
/// precedence [`compare::compare`] was probed for two days earlier.
fn operand_flags(other: F80, operand: &Outcome) -> u16 {
    match arith::nan_present(other, operand.value) {
        true => operand.flags & !crate::flags::DENORMAL,
        false => operand.flags,
    }
}

impl X87State {
    fn modes(&self) -> (Rounding, Precision) {
        (self.rounding(), self.precision())
    }

    // --- loads ---

    /// `fld m80`: a verbatim move — no conversion, no numeric exceptions,
    /// whatever the pattern.
    pub fn fld_m80(&mut self, bytes: [u8; 10]) {
        self.merge_arithmetic(0);
        self.push(F80::from_bytes(bytes));
    }

    fn load(&mut self, outcome: Outcome) {
        self.merge_arithmetic(outcome.flags);
        self.push(outcome.value);
    }

    pub fn fld_m64(&mut self, bits: u64) {
        self.load(convert::from_f64(bits));
    }

    pub fn fld_m32(&mut self, bits: u32) {
        self.load(convert::from_f32(bits));
    }

    pub fn fld_sti(&mut self, index: u32) {
        let value = self.read(index);
        self.merge_arithmetic(0);
        self.push(value);
    }

    pub fn fild(&mut self, value: i64) {
        self.merge_arithmetic(0);
        self.push(convert::from_i64(value));
    }

    pub fn fld_constant(&mut self, index: u32) {
        let value = transcendental::constant(index, self.rounding());
        self.merge_arithmetic(0);
        self.push(value);
    }

    // --- stores ---

    /// `fstp m80`: verbatim bytes out; only an empty source objects.
    ///
    /// It still *writes* C1, to zero — nothing was rounded, because nothing
    /// was converted. Leaving the bit alone instead leaks the previous
    /// operation's rounding answer into the status word, where a `fnstsw`
    /// can read it; found by the lockstep oracle, which sees a status word
    /// after every instruction rather than after each one in isolation.
    pub fn fstp_m80(&mut self) -> [u8; 10] {
        let value = self.read(0);
        self.merge_arithmetic(0);
        self.pop();
        value.to_bytes()
    }

    pub fn fst_m64(&mut self, pop: bool) -> u64 {
        let value = self.read(0);
        let (bits, flags) = convert::to_f64(value, self.rounding());
        self.merge_arithmetic(flags);
        if pop {
            self.pop();
        }
        bits
    }

    pub fn fst_m32(&mut self, pop: bool) -> u32 {
        let value = self.read(0);
        let (bits, flags) = convert::to_f32(value, self.rounding());
        self.merge_arithmetic(flags);
        if pop {
            self.pop();
        }
        bits
    }

    pub fn fst_sti(&mut self, index: u32, pop: bool) {
        let value = self.read(0);
        self.write(index, value);
        if pop {
            self.pop();
        }
    }

    pub fn fist(&mut self, width_bits: u32, pop: bool) -> i64 {
        let value = self.read(0);
        let (result, flags) = convert::to_int(value, self.rounding(), width_bits);
        self.merge_arithmetic(flags);
        if pop {
            self.pop();
        }
        result
    }

    /// `fisttp`: truncation regardless of RC, always popping. (SSE3, but
    /// costs nothing here.)
    pub fn fisttp(&mut self, width_bits: u32) -> i64 {
        let value = self.read(0);
        let (result, flags) = convert::to_int(value, Rounding::Chop, width_bits);
        self.merge_arithmetic(flags);
        self.pop();
        result
    }

    // --- arithmetic ---

    /// The register-to-register forms: ST(dst) = ST(dst) op ST(src),
    /// optionally popping (`faddp` and family).
    pub fn binary_sti(&mut self, op: Binary, dst: u32, src: u32, pop: bool) {
        let a = self.read(dst);
        let b = self.read(src);
        let (rounding, precision) = self.modes();
        let out = op.apply(a, b, rounding, precision);
        self.merge_arithmetic(out.flags);
        self.write(dst, out.value);
        if pop {
            self.pop();
        }
    }

    fn binary_memory(&mut self, op: Binary, operand: Outcome) {
        let a = self.read(0);
        let (rounding, precision) = self.modes();
        let out = op.apply(a, operand.value, rounding, precision);
        self.merge_arithmetic(operand_flags(a, &operand) | out.flags);
        self.write(0, out.value);
    }

    pub fn binary_m64(&mut self, op: Binary, bits: u64) {
        self.binary_memory(op, convert::from_f64(bits));
    }

    pub fn binary_m32(&mut self, op: Binary, bits: u32) {
        self.binary_memory(op, convert::from_f32(bits));
    }

    /// `fiadd`/`fisub`/... : the integer-memory forms.
    pub fn binary_int(&mut self, op: Binary, value: i64) {
        self.binary_memory(op, Outcome::exact(convert::from_i64(value)));
    }

    fn unary(&mut self, f: impl FnOnce(F80, Rounding, Precision) -> Outcome) {
        let a = self.read(0);
        let (rounding, precision) = self.modes();
        let out = f(a, rounding, precision);
        self.merge_arithmetic(out.flags);
        self.write(0, out.value);
    }

    /// `fchs`/`fabs`: pure sign operations — they work on any pattern,
    /// even NaNs, and raise nothing but the stack fault.
    pub fn fchs(&mut self) {
        let a = self.read(0);
        self.merge_arithmetic(0);
        self.write(0, a.negate());
    }

    pub fn fabs(&mut self) {
        let a = self.read(0);
        self.merge_arithmetic(0);
        self.write(0, a.abs());
    }

    pub fn fsqrt(&mut self) {
        self.unary(arith::sqrt);
    }

    pub fn frndint(&mut self) {
        let a = self.read(0);
        let out = arith::round_to_int(a, self.rounding());
        self.merge_arithmetic(out.flags);
        self.write(0, out.value);
    }

    pub fn fprem(&mut self, nearest: bool) {
        let a = self.read(0);
        let b = self.read(1);
        let out = arith::partial_remainder(a, b, nearest);
        // The remainder states the whole condition protocol: C2 for an
        // incomplete pass, the quotient bits on completion.
        self.merge_conditions(out.flags);
        self.write(0, out.value);
    }

    pub fn fscale(&mut self) {
        let a = self.read(0);
        let b = self.read(1);
        let (rounding, precision) = self.modes();
        let out = arith::scale(a, b, rounding, precision);
        self.merge_arithmetic(out.flags);
        self.write(0, out.value);
    }

    pub fn fxtract(&mut self) {
        let a = self.read(0);
        let (mantissa, exponent, flags) = arith::extract(a);
        self.merge_arithmetic(flags);
        self.write(0, exponent);
        self.push(mantissa);
    }

    // --- transcendentals ---

    pub fn f2xm1(&mut self) {
        self.unary(|a, _, _| transcendental::f2xm1(a));
    }

    fn towards_st1(&mut self, f: impl FnOnce(F80, F80, Rounding, Precision) -> Outcome) {
        let x = self.read(0);
        let y = self.read(1);
        let (rounding, precision) = self.modes();
        let out = f(x, y, rounding, precision);
        self.merge_arithmetic(out.flags);
        self.write(1, out.value);
        self.pop();
    }

    pub fn fyl2x(&mut self) {
        self.towards_st1(transcendental::fyl2x);
    }

    pub fn fyl2xp1(&mut self) {
        self.towards_st1(transcendental::fyl2xp1);
    }

    pub fn fpatan(&mut self) {
        self.towards_st1(transcendental::fpatan);
    }

    // --- stack manipulation ---

    pub fn fxch(&mut self, index: u32) {
        let a = self.read(0);
        let b = self.read(index);
        self.merge_arithmetic(0);
        self.write(0, b);
        self.write(index, a);
    }

    pub fn ffree(&mut self, index: u32, pop: bool) {
        self.free(index);
        if pop {
            self.pop();
        }
    }

    pub fn fincstp(&mut self) {
        self.rotate(false);
    }

    pub fn fdecstp(&mut self) {
        self.rotate(true);
    }

    /// `fcmovcc`: the predicate was already decided translator-side from
    /// the promoted flags; both registers are still checked, matching the
    /// hardware's stack-fault behavior for the untaken case.
    pub fn fcmov(&mut self, index: u32, take: bool) {
        let source = self.read(index);
        let _ = self.read(0);
        if take {
            self.write(0, source);
        }
    }

    // --- comparison ---

    fn compared(&mut self, other: F80, policy: NanPolicy, pops: u32) -> Relation {
        let a = self.read(0);
        let (relation, flags) = compare::compare(a, other, policy);
        self.merge_conditions(flags | relation.condition_codes());
        for _ in 0..pops {
            self.pop();
        }
        relation
    }

    pub fn fcom_sti(&mut self, index: u32, policy: NanPolicy, pops: u32) {
        let other = self.read(index);
        self.compared(other, policy, pops);
    }

    pub fn fcom_m64(&mut self, bits: u64, pops: u32) {
        let operand = convert::from_f64(bits);
        let flags = operand_flags(self.peek(0), &operand);
        self.merge_arithmetic(flags);
        self.compared(operand.value, NanPolicy::Signalling, pops);
    }

    pub fn fcom_m32(&mut self, bits: u32, pops: u32) {
        let operand = convert::from_f32(bits);
        let flags = operand_flags(self.peek(0), &operand);
        self.merge_arithmetic(flags);
        self.compared(operand.value, NanPolicy::Signalling, pops);
    }

    pub fn ficom(&mut self, value: i64, pops: u32) {
        self.compared(convert::from_i64(value), NanPolicy::Signalling, pops);
    }

    pub fn ftst(&mut self) {
        self.compared(F80::ZERO, NanPolicy::Signalling, 0);
    }

    /// `fcomi`/`fucomi`: the relation goes to EFLAGS — returned packed for
    /// the translator (CF bit 0, PF bit 2, ZF bit 6) — and only C1 is
    /// touched in the status word.
    pub fn fcomi(&mut self, index: u32, policy: NanPolicy, pop: bool) -> u32 {
        let a = self.read(0);
        let b = self.read(index);
        let (relation, flags) = compare::compare(a, b, policy);
        self.merge_arithmetic(flags);
        if pop {
            self.pop();
        }
        relation.eflags()
    }

    /// `fxam` — the one classifier that must see emptiness, and the one
    /// operation on this file that faults on nothing.
    pub fn fxam(&mut self) {
        let code = if self.is_empty(0) {
            let sign = if self.peek(0).sign() {
                crate::flags::C1
            } else {
                0
            };
            compare::EXAMINE_EMPTY | sign
        } else {
            compare::examine(self.peek(0))
        };
        self.merge_conditions(code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags;

    fn st0_f64(state: &mut X87State) -> f64 {
        f64::from_bits(convert::to_f64(state.peek(0), Rounding::Nearest).0)
    }

    #[test]
    fn a_small_program() {
        // The compiler's bread and butter: load, arithmetic against
        // memory, store-and-pop.
        let mut state = X87State::new();
        state.fld_m64(3.0f64.to_bits());
        state.fld_m64(4.0f64.to_bits());
        state.binary_sti(Binary::Mul, 1, 0, true); // fmulp
        state.binary_m64(Binary::Add, 2.5f64.to_bits()); // fadd m64
        assert_eq!(st0_f64(&mut state), 14.5);
        let bits = state.fst_m64(true);
        assert_eq!(f64::from_bits(bits), 14.5);
        assert!(state.is_empty(0), "the stack drained");
    }

    #[test]
    fn reversed_and_popping_forms() {
        let mut state = X87State::new();
        state.fld_m64(10.0f64.to_bits());
        state.fld_m64(4.0f64.to_bits());
        // fdivrp st(1), st(0): st1 = st0 / st1 = 4/10, pop.
        state.binary_sti(Binary::DivReverse, 1, 0, true);
        assert_eq!(st0_f64(&mut state), 0.4);
        state.binary_m32(Binary::SubReverse, 1.0f32.to_bits());
        assert_eq!(st0_f64(&mut state), 0.6);
    }

    #[test]
    fn integer_forms() {
        let mut state = X87State::new();
        state.fild(-7);
        state.binary_int(Binary::Mul, 3);
        assert_eq!(state.fist(32, false), -21);
        assert_eq!(state.fisttp(16), -21);
        assert!(state.is_empty(0));
    }

    #[test]
    fn the_truncation_dance() {
        // What compilers emit for (long)x without SSE3: set RC to chop via
        // fldcw, fistp, restore.
        let mut state = X87State::new();
        state.fld_m64(2.9f64.to_bits());
        let saved = state.control();
        state.set_control(saved | 0x0C00);
        let value = state.fist(64, true);
        state.set_control(saved);
        assert_eq!(value, 2);
    }

    #[test]
    fn exchange_and_conditional_move() {
        let mut state = X87State::new();
        state.fld_m64(1.0f64.to_bits());
        state.fld_m64(2.0f64.to_bits());
        state.fxch(1);
        assert_eq!(st0_f64(&mut state), 1.0);
        state.fcmov(1, false);
        assert_eq!(st0_f64(&mut state), 1.0);
        state.fcmov(1, true);
        assert_eq!(st0_f64(&mut state), 2.0);
    }

    #[test]
    fn comparison_writes_condition_codes() {
        let mut state = X87State::new();
        state.fld_m64(1.0f64.to_bits());
        state.fcom_m64(2.0f64.to_bits(), 0);
        let status = state.status_word();
        assert!(status & flags::C0 != 0, "1 < 2 sets C0");
        assert_eq!(status & flags::C3, 0);
        state.fcom_m64(1.0f64.to_bits(), 1);
        assert!(state.status_word() & flags::C3 != 0, "equal sets C3");
        assert!(state.is_empty(0), "fcomp popped");
    }

    #[test]
    fn fcomi_returns_eflags() {
        let mut state = X87State::new();
        state.fld_m64(2.0f64.to_bits());
        state.fld_m64(1.0f64.to_bits());
        // st0 = 1, st1 = 2: less → CF.
        assert_eq!(state.fcomi(1, NanPolicy::Quiet, false), 1);
        state.fxch(1);
        assert_eq!(state.fcomi(1, NanPolicy::Quiet, false), 0);
        state.fld_m64(f64::NAN.to_bits());
        let flags = state.fcomi(1, NanPolicy::Quiet, true);
        assert_eq!(flags, 1 | (1 << 2) | (1 << 6), "unordered sets all three");
    }

    #[test]
    fn fxam_and_the_empty_stack() {
        let mut state = X87State::new();
        state.fxam();
        let status = state.status_word();
        assert!(status & flags::C3 != 0 && status & flags::C0 != 0, "empty");
        assert_eq!(
            status & (flags::INVALID | flags::STACK_FAULT),
            0,
            "fxam never faults"
        );
        state.fld_m64((-0.0f64).to_bits());
        state.fxam();
        let status = state.status_word();
        assert!(status & flags::C3 != 0 && status & flags::C2 == 0, "zero");
        assert!(status & flags::C1 != 0, "and negative");
    }

    #[test]
    fn the_fmodl_loop_shape() {
        // musl's fmodl: fprem until C2 clears.
        let mut state = X87State::new();
        state.fld_m64(3.0f64.to_bits()); // divisor first
        let huge = F80::new(false, (crate::f80::BIAS + 200) as u16, 1 << 63);
        state.fld_m80(huge.to_bytes()); // dividend in st0
        let mut passes = 0;
        loop {
            state.fprem(false);
            passes += 1;
            if state.status_word() & flags::C2 == 0 {
                break;
            }
            assert!(passes < 8, "fprem failed to converge");
        }
        assert!(passes > 1, "the partial path was actually exercised");
        let remainder = st0_f64(&mut state);
        assert!((0.0..3.0).contains(&remainder));
        // The oracle proves the value; here we prove the protocol.
    }

    #[test]
    fn transcendental_stack_shapes() {
        let mut state = X87State::new();
        // musl expl's core shape: fyl2x-style two-in-one-out.
        state.fld_m64(1.0f64.to_bits()); // y
        state.fld_m64(8.0f64.to_bits()); // x
        state.fyl2x();
        assert_eq!(st0_f64(&mut state), 3.0);
        assert!(state.is_empty(1), "fyl2x consumed a slot");
        // fpatan is atan(ST1/ST0): with 3.0 beneath and 1.0 on top, the
        // quotient is 3.
        state.fld_m64(1.0f64.to_bits());
        state.fpatan();
        let angle = st0_f64(&mut state);
        assert!((angle - 3.0f64.atan()).abs() < 1e-15);
    }

    #[test]
    fn fxtract_pushes() {
        let mut state = X87State::new();
        state.fld_m64(48.0f64.to_bits());
        state.fxtract();
        assert_eq!(st0_f64(&mut state), 1.5);
        state.pop();
        assert_eq!(st0_f64(&mut state), 5.0);
    }

    #[test]
    fn store_from_empty_is_the_indefinite() {
        let mut state = X87State::new();
        let bits = state.fst_m64(false);
        assert_eq!(bits, 0xFFF8_0000_0000_0000);
        let status = state.status_word();
        assert!(status & flags::INVALID != 0 && status & flags::STACK_FAULT != 0);
    }
}
