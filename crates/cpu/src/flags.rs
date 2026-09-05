//! Lazy flags: the last flag-writing operation, remembered, and read back
//! one question at a time.
//!
//! Almost every flag x86 computes is never read. A compiler emits `cmp`
//! then one `jcc`, and the `jcc` wants a single bit; computing six of them
//! eagerly at the `cmp` means five stores and five loads that nothing
//! consumes. So the record here stores what the operation *was* — its rule,
//! its width, its two inputs and its result — and each consumer derives
//! exactly the bit it needs. This is not a new idea: it is the lazy-flags
//! model every fast x86 emulator uses.
//!
//! The tail is handled by materialization rather than by more rules.
//! Shifts, rotates, multiplies, bit tests and the flag-writing instructions
//! themselves compute their bits eagerly into [`Flags::bits`] and set the
//! rule to [`Rule::Materialized`]; so does `pushf`, `lahf` and anything else
//! that wants all six at once. The laziness is therefore paid for exactly
//! where it pays — the ALU-then-branch pattern that is the overwhelming
//! majority of flag traffic — and the rare instruction is simple and
//! obviously correct instead of subtle and fast.
//!
//! The direction flag is not part of any of this. It is not the result of
//! anything: `std` sets it, `cld` clears it, the string instructions read
//! it, and nothing else touches it, so it is a `bool` beside the record.

/// The six status flags, at their RFLAGS bit positions.
///
/// The positions are the architecture's, not a convention of ours, because
/// `pushf` and `popf` and the signal frame's `eflags` word all have to agree
/// with what a guest already believes. Adjust is here because an
/// interpreter can answer it for free from the same three values every
/// other flag comes from, and because a faithful `pushf` needs it.
pub mod bit {
    pub const CARRY: u64 = 1 << 0;
    /// Architecturally reserved and architecturally *one*. A `pushf` that
    /// answers with it clear is a divergence a guest can see.
    pub const RESERVED_ONE: u64 = 1 << 1;
    pub const PARITY: u64 = 1 << 2;
    pub const ADJUST: u64 = 1 << 4;
    pub const ZERO: u64 = 1 << 6;
    pub const SIGN: u64 = 1 << 7;
    pub const TRAP: u64 = 1 << 8;
    pub const INTERRUPT: u64 = 1 << 9;
    pub const DIRECTION: u64 = 1 << 10;
    pub const OVERFLOW: u64 = 1 << 11;

    /// The six the record can answer.
    pub const STATUS: u64 = CARRY | PARITY | ADJUST | ZERO | SIGN | OVERFLOW;
}

use crate::state::Width;

/// Which operation the record remembers, and hence by what rule its flags
/// are derived.
///
/// `Increment` and `Decrement` are `Add` and `Sub` in every flag but the
/// carry, which they preserve — so they store a right-hand side of one and
/// differ from their parents in exactly one arm of [`Flags::carry`]. That is
/// `dec`'s famous property, and stating it as a variant rather than as a
/// special case at every `dec` site is what keeps it from being forgotten.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Rule {
    /// Nothing pending: all six flags are in [`Flags::bits`].
    Materialized = 0,
    /// `result = left + right`.
    Add = 1,
    /// `result = left - right`.
    Sub = 2,
    /// Bitwise. Carry and overflow are cleared.
    Logic = 3,
    /// `inc`: as [`Rule::Add`] with a right-hand side of one, but the carry
    /// is whatever it already was.
    Increment = 4,
    /// `dec`: as [`Rule::Sub`], carry preserved.
    Decrement = 5,
    /// `adc`: `result = left + right + carry_in`.
    AddCarry = 6,
    /// `sbb`: `result = left - right - carry_in`.
    SubBorrow = 7,
}

/// The last flag-writing operation, plus the bits no rule covers.
#[derive(Clone, Copy, Debug)]
pub struct Flags {
    rule: Rule,
    width: Width,
    /// The operands and the result, each already truncated to `width`.
    left: u64,
    right: u64,
    result: u64,
    /// The carry that went *into* an `adc` or `sbb`, as zero or one.
    carry_in: u64,
    /// Materialized bits, in RFLAGS positions. Under
    /// [`Rule::Materialized`] this is all six status flags; under
    /// [`Rule::Increment`] and [`Rule::Decrement`] it is the carry they
    /// preserve; under the other rules the status bits in it are stale and
    /// unread. The direction, trap and interrupt bits live here always.
    bits: u64,
}

impl Default for Flags {
    fn default() -> Self {
        Self::new()
    }
}

impl Flags {
    /// The flags a fresh process starts with: the reserved bit set,
    /// interrupts enabled, everything else clear — 0x202, which is what
    /// Linux leaves in `eflags` at `_start`.
    pub fn new() -> Self {
        Self {
            rule: Rule::Materialized,
            width: Width::Qword,
            left: 0,
            right: 0,
            result: 0,
            carry_in: 0,
            bits: bit::RESERVED_ONE | bit::INTERRUPT,
        }
    }

    /// Records an operation whose flags follow one of the rules.
    ///
    /// `left`, `right` and `result` are truncated here rather than at the
    /// hundred call sites, so an instruction arm can hand over whatever
    /// width-agnostic `u64`s its arithmetic produced.
    // Five field stores, on every arithmetic instruction, and showing
    // as its own frame at 4.2% of the Django import. Small enough that
    // a copy inside `step` costs nothing — unlike `read`, `write`,
    // `push` and `pop`, which were tried and reverted.
    #[inline(always)]
    pub fn record(&mut self, rule: Rule, width: Width, left: u64, right: u64, result: u64) {
        debug_assert!(rule != Rule::Materialized, "use `set_all` for that");
        // `inc` and `dec` preserve the carry, and preserving it means
        // *reading it from the record that is about to be replaced*. Doing
        // this at the store rather than at the load is what keeps
        // `Flags::carry` a pure read: were it left to the load, the carry
        // bit in `bits` would be whatever some earlier materialization put
        // there, which is right exactly when nothing lazy happened in
        // between and silently wrong otherwise.
        if matches!(rule, Rule::Increment | Rule::Decrement) {
            let preserved = self.carry();
            match preserved {
                true => self.bits |= bit::CARRY,
                false => self.bits &= !bit::CARRY,
            }
        }
        self.rule = rule;
        self.width = width;
        self.left = width.truncate(left);
        self.right = width.truncate(right);
        self.result = width.truncate(result);
        self.carry_in = 0;
    }

    /// As [`Flags::record`], for the two rules that also consume a carry.
    pub fn record_with_carry(
        &mut self,
        rule: Rule,
        width: Width,
        left: u64,
        right: u64,
        result: u64,
        carry_in: bool,
    ) {
        self.record(rule, width, left, right, result);
        self.carry_in = u64::from(carry_in);
    }

    /// Records the six status flags directly, for the instructions whose
    /// effects no rule describes.
    ///
    /// `status` carries them at their RFLAGS positions; the direction, trap
    /// and interrupt bits are kept from before, because nothing that goes
    /// through here is allowed to disturb them.
    pub fn set_all(&mut self, status: u64) {
        debug_assert_eq!(status & !bit::STATUS, 0, "only the six status flags");
        self.bits = (self.materialized() & !bit::STATUS) | status;
        self.rule = Rule::Materialized;
    }

    pub fn carry(&self) -> bool {
        match self.rule {
            Rule::Materialized | Rule::Increment | Rule::Decrement => self.bits & bit::CARRY != 0,
            Rule::Add => self.result < self.left,
            // With a carry in, a right-hand side of all ones lands the
            // result back on the left-hand side having wrapped once — the
            // one case a plain `<` cannot see.
            Rule::AddCarry => {
                self.result < self.left || (self.carry_in == 1 && self.result == self.left)
            }
            Rule::Sub => self.left < self.right,
            // Borrowing when the two are equal is exactly what the incoming
            // borrow adds.
            Rule::SubBorrow => {
                self.left < self.right || (self.carry_in == 1 && self.left == self.right)
            }
            Rule::Logic => false,
        }
    }

    pub fn zero(&self) -> bool {
        match self.rule {
            Rule::Materialized => self.bits & bit::ZERO != 0,
            _ => self.result == 0,
        }
    }

    pub fn sign(&self) -> bool {
        match self.rule {
            Rule::Materialized => self.bits & bit::SIGN != 0,
            _ => self.result & self.width.sign_bit() != 0,
        }
    }

    /// Parity of the low eight bits of the result — and only the low eight,
    /// at every width, which is the architecture's rule and a recurring
    /// surprise.
    pub fn parity(&self) -> bool {
        match self.rule {
            Rule::Materialized => self.bits & bit::PARITY != 0,
            _ => (self.result as u8).count_ones() % 2 == 0,
        }
    }

    /// The carry out of bit three, which is what "adjust" means. It is the
    /// bit that binary-coded-decimal arithmetic reads and that nothing else
    /// does — but `pushf` reads it, so it is answered rather than invented.
    pub fn adjust(&self) -> bool {
        match self.rule {
            Rule::Materialized => self.bits & bit::ADJUST != 0,
            // Undefined after a logical operation; the hardware clears it,
            // and so do we.
            Rule::Logic => false,
            // The carry into bit four is what the three values disagree
            // about at bit four, and an `adc`'s incoming carry needs no term
            // of its own: it is already inside the result.
            _ => (self.left ^ self.right ^ self.result) & 0x10 != 0,
        }
    }

    pub fn overflow(&self) -> bool {
        let sign = self.width.sign_bit();
        match self.rule {
            Rule::Materialized => self.bits & bit::OVERFLOW != 0,
            Rule::Logic => false,
            // Two operands of the same sign producing the other one.
            Rule::Add | Rule::AddCarry | Rule::Increment => {
                (self.left ^ self.result) & (self.right ^ self.result) & sign != 0
            }
            // Operands of opposite signs, and the result taking the
            // subtrahend's.
            Rule::Sub | Rule::SubBorrow | Rule::Decrement => {
                (self.left ^ self.right) & (self.left ^ self.result) & sign != 0
            }
        }
    }

    pub fn direction(&self) -> bool {
        self.bits & bit::DIRECTION != 0
    }

    pub fn set_direction(&mut self, value: bool) {
        self.assign_static(bit::DIRECTION, value);
    }

    pub fn trap(&self) -> bool {
        self.bits & bit::TRAP != 0
    }

    /// Sets the trap flag.
    ///
    /// The engine never *acts* on it — the loop is already single-stepping
    /// whenever it wants to be, and there is no hardware to ask. It is
    /// carried because it is part of the word `pushf` produces, and because
    /// the lockstep oracle runs against a process the kernel has forced it
    /// on: a guest reading its own flags under a debugger sees it set on
    /// hardware, so the comparison needs a way to say so.
    pub fn set_trap(&mut self, value: bool) {
        self.assign_static(bit::TRAP, value);
    }

    pub fn set_carry(&mut self, value: bool) {
        let status = self.status() & !bit::CARRY;
        self.set_all(status | if value { bit::CARRY } else { 0 });
    }

    /// The six status flags, at their RFLAGS positions.
    pub fn status(&self) -> u64 {
        let mut status = 0;
        if self.carry() {
            status |= bit::CARRY;
        }
        if self.parity() {
            status |= bit::PARITY;
        }
        if self.adjust() {
            status |= bit::ADJUST;
        }
        if self.zero() {
            status |= bit::ZERO;
        }
        if self.sign() {
            status |= bit::SIGN;
        }
        if self.overflow() {
            status |= bit::OVERFLOW;
        }
        status
    }

    /// The whole RFLAGS word a `pushf` pushes.
    pub fn materialized(&self) -> u64 {
        (self.bits & !bit::STATUS) | self.status()
    }

    /// Replaces the whole word, as `popf` and a `sigreturn` do.
    ///
    /// The reserved bits are forced to what the architecture forces them to,
    /// so a guest that pushes, mangles and pops cannot install a word the
    /// hardware would never have produced.
    pub fn load(&mut self, word: u64) {
        const WRITABLE: u64 = bit::STATUS | bit::TRAP | bit::INTERRUPT | bit::DIRECTION;
        self.rule = Rule::Materialized;
        self.bits = (word & WRITABLE) | bit::RESERVED_ONE;
    }

    /// Assigns one of the bits no rule covers.
    ///
    /// The lazy record is left alone, deliberately: the direction and trap
    /// flags are not results of anything, so writing one has no business
    /// forcing the six status flags to be computed. `cld` before a `rep
    /// movsb` would otherwise materialize the flags of whatever came
    /// before it, every time, for nothing.
    fn assign_static(&mut self, mask: u64, value: bool) {
        debug_assert_eq!(mask & bit::STATUS, 0, "a status flag has a rule");
        match value {
            true => self.bits |= mask,
            false => self.bits &= !mask,
        }
    }
}

/// The sixteen conditions a `jcc`, `setcc` or `cmovcc` can name.
///
/// Answering them from the record rather than from six materialized bits is
/// the point of the whole module: `jne` asks one question and gets one
/// comparison.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Condition {
    Overflow = 0,
    NoOverflow = 1,
    Below = 2,
    AboveOrEqual = 3,
    Equal = 4,
    NotEqual = 5,
    BelowOrEqual = 6,
    Above = 7,
    Sign = 8,
    NoSign = 9,
    Parity = 10,
    NoParity = 11,
    Less = 12,
    GreaterOrEqual = 13,
    LessOrEqual = 14,
    Greater = 15,
}

impl Condition {
    /// The condition with x86's own encoding — the low four bits of a
    /// `jcc` opcode — which is what compiled code hands a helper.
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Condition::Overflow,
            1 => Condition::NoOverflow,
            2 => Condition::Below,
            3 => Condition::AboveOrEqual,
            4 => Condition::Equal,
            5 => Condition::NotEqual,
            6 => Condition::BelowOrEqual,
            7 => Condition::Above,
            8 => Condition::Sign,
            9 => Condition::NoSign,
            10 => Condition::Parity,
            11 => Condition::NoParity,
            12 => Condition::Less,
            13 => Condition::GreaterOrEqual,
            14 => Condition::LessOrEqual,
            15 => Condition::Greater,
            _ => return None,
        })
    }

    #[inline(always)]
    pub fn holds(self, flags: &Flags) -> bool {
        match self {
            Condition::Overflow => flags.overflow(),
            Condition::NoOverflow => !flags.overflow(),
            Condition::Below => flags.carry(),
            Condition::AboveOrEqual => !flags.carry(),
            Condition::Equal => flags.zero(),
            Condition::NotEqual => !flags.zero(),
            Condition::BelowOrEqual => flags.carry() || flags.zero(),
            Condition::Above => !flags.carry() && !flags.zero(),
            Condition::Sign => flags.sign(),
            Condition::NoSign => !flags.sign(),
            Condition::Parity => flags.parity(),
            Condition::NoParity => !flags.parity(),
            Condition::Less => flags.sign() != flags.overflow(),
            Condition::GreaterOrEqual => flags.sign() == flags.overflow(),
            Condition::LessOrEqual => flags.zero() || flags.sign() != flags.overflow(),
            Condition::Greater => !flags.zero() && flags.sign() == flags.overflow(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lazy record and an eager computation have to agree on every flag,
    /// for every width, at the boundaries where carries and overflows
    /// happen. This is the property the whole module rests on, so it is
    /// checked exhaustively over a set of values chosen to straddle every
    /// boundary rather than by spot-checking.
    fn interesting(width: Width) -> Vec<u64> {
        let mask = width.mask();
        let sign = width.sign_bit();
        vec![
            0,
            1,
            2,
            0x0f,
            0x10,
            0x7f,
            0x80,
            sign - 1,
            sign,
            sign + 1,
            mask - 1,
            mask,
        ]
    }

    #[test]
    fn addition_agrees_with_eager_flags() {
        for width in [Width::Byte, Width::Word, Width::Dword, Width::Qword] {
            for &left in &interesting(width) {
                for &right in &interesting(width) {
                    let result = width.truncate(left.wrapping_add(right));
                    let mut flags = Flags::new();
                    flags.record(Rule::Add, width, left, right, result);
                    let wide = (left as u128) + (right as u128);
                    assert_eq!(
                        flags.carry(),
                        wide > width.mask() as u128,
                        "carry of {left:#x} + {right:#x} at {width:?}"
                    );
                    let signed = width.sign_extend(left) as i64 as i128
                        + width.sign_extend(right) as i64 as i128;
                    let representable = (width.sign_extend(result) as i64 as i128) == signed;
                    assert_eq!(
                        flags.overflow(),
                        !representable,
                        "overflow of {left:#x} + {right:#x} at {width:?}"
                    );
                    assert_eq!(flags.zero(), result == 0);
                    assert_eq!(flags.sign(), result & width.sign_bit() != 0);
                    assert_eq!(flags.parity(), (result as u8).count_ones() % 2 == 0);
                    assert_eq!(
                        flags.adjust(),
                        ((left & 0xf) + (right & 0xf)) > 0xf,
                        "adjust of {left:#x} + {right:#x}"
                    );
                }
            }
        }
    }

    #[test]
    fn subtraction_agrees_with_eager_flags() {
        for width in [Width::Byte, Width::Word, Width::Dword, Width::Qword] {
            for &left in &interesting(width) {
                for &right in &interesting(width) {
                    let result = width.truncate(left.wrapping_sub(right));
                    let mut flags = Flags::new();
                    flags.record(Rule::Sub, width, left, right, result);
                    assert_eq!(flags.carry(), left < right);
                    let signed = width.sign_extend(left) as i64 as i128
                        - width.sign_extend(right) as i64 as i128;
                    let representable = (width.sign_extend(result) as i64 as i128) == signed;
                    assert_eq!(
                        flags.overflow(),
                        !representable,
                        "overflow of {left:#x} - {right:#x} at {width:?}"
                    );
                    assert_eq!(flags.zero(), result == 0);
                    assert_eq!(flags.sign(), result & width.sign_bit() != 0);
                    assert_eq!(flags.adjust(), (left & 0xf) < (right & 0xf));
                }
            }
        }
    }

    #[test]
    fn add_with_carry_agrees_with_eager_flags() {
        for width in [Width::Byte, Width::Dword, Width::Qword] {
            for &left in &interesting(width) {
                for &right in &interesting(width) {
                    for carry_in in [false, true] {
                        let incoming = u64::from(carry_in);
                        let result =
                            width.truncate(left.wrapping_add(right).wrapping_add(incoming));
                        let mut flags = Flags::new();
                        flags.record_with_carry(
                            Rule::AddCarry,
                            width,
                            left,
                            right,
                            result,
                            carry_in,
                        );
                        let wide = left as u128 + right as u128 + incoming as u128;
                        assert_eq!(
                            flags.carry(),
                            wide > width.mask() as u128,
                            "carry of {left:#x} + {right:#x} + {incoming} at {width:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn subtract_with_borrow_agrees_with_eager_flags() {
        for width in [Width::Byte, Width::Dword, Width::Qword] {
            for &left in &interesting(width) {
                for &right in &interesting(width) {
                    for borrow_in in [false, true] {
                        let incoming = u64::from(borrow_in);
                        let result =
                            width.truncate(left.wrapping_sub(right).wrapping_sub(incoming));
                        let mut flags = Flags::new();
                        flags.record_with_carry(
                            Rule::SubBorrow,
                            width,
                            left,
                            right,
                            result,
                            borrow_in,
                        );
                        let borrowed = (left as u128) < (right as u128 + incoming as u128);
                        assert_eq!(
                            flags.carry(),
                            borrowed,
                            "borrow of {left:#x} - {right:#x} - {incoming} at {width:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn increment_preserves_the_carry_and_decrement_does_too() {
        let mut flags = Flags::new();
        flags.set_carry(true);
        flags.record(Rule::Increment, Width::Dword, 0xffff_ffff, 1, 0);
        assert!(flags.carry(), "inc must not clear a set carry");
        assert!(flags.zero());
        assert!(!flags.overflow());

        flags.set_carry(false);
        flags.record(Rule::Decrement, Width::Dword, 0x8000_0000, 1, 0x7fff_ffff);
        assert!(!flags.carry(), "dec must not set a clear carry");
        assert!(flags.overflow(), "the sign boundary is an overflow");
    }

    /// The carry `inc` preserves is the one the *lazy* record would have
    /// produced, not whatever some earlier materialization happened to
    /// leave behind. Found by the lockstep oracle at the ninth instruction
    /// of the first probe it ever ran.
    #[test]
    fn increment_preserves_a_carry_that_was_never_materialized() {
        for (left, right, expected) in [
            (1u64, 2u64, true),   // 1 - 2 borrows
            (2, 1, false),        // 2 - 1 does not
        ] {
            let mut flags = Flags::new();
            // Start from the opposite of the answer, so a stale read is
            // visible rather than accidentally right.
            flags.set_carry(!expected);
            let result = Width::Qword.truncate(left.wrapping_sub(right));
            flags.record(Rule::Sub, Width::Qword, left, right, result);
            flags.record(Rule::Increment, Width::Qword, 5, 1, 6);
            assert_eq!(
                flags.carry(),
                expected,
                "inc after {left} - {right} kept the wrong carry"
            );
        }
    }

    #[test]
    fn a_pushed_word_carries_the_reserved_bit_and_survives_a_pop() {
        let mut flags = Flags::new();
        flags.record(Rule::Sub, Width::Dword, 1, 1, 0);
        let word = flags.materialized();
        assert_eq!(word & bit::RESERVED_ONE, bit::RESERVED_ONE);
        assert_eq!(word & bit::ZERO, bit::ZERO);
        let mut restored = Flags::new();
        restored.load(word);
        assert_eq!(restored.materialized(), word);
    }

    #[test]
    fn loading_a_word_cannot_clear_the_reserved_bit() {
        let mut flags = Flags::new();
        flags.load(0);
        assert_eq!(flags.materialized(), bit::RESERVED_ONE);
    }

    #[test]
    fn the_signed_conditions_read_sign_against_overflow() {
        // -1 vs 1: less, and not below.
        let mut flags = Flags::new();
        let result = Width::Dword.truncate(0xffff_ffffu64.wrapping_sub(1));
        flags.record(Rule::Sub, Width::Dword, 0xffff_ffff, 1, result);
        assert!(Condition::Less.holds(&flags));
        assert!(!Condition::Below.holds(&flags));
        assert!(Condition::LessOrEqual.holds(&flags));
        assert!(!Condition::Greater.holds(&flags));
    }
}
