//! The FPU's architectural state: the eight-register stack, TOP, the tag
//! word, FCW and FSW — and the memory images (`fnstenv`/`fnsave`, the
//! context-switch image) that move it.
//!
//! Everything behaves as-if-masked: faults record their flags (including
//! the stack fault and ES) and execution continues with the masked
//! response. Delivery of unmasked exceptions is not built; keeping ES
//! scrupulous now is what makes it a bolt-on then.

use crate::f80::{Class, F80};
use crate::flags::{C1, CONDITIONS, ERROR_SUMMARY, EXCEPTIONS, INVALID, STACK_FAULT};
use crate::{Precision, Rounding};

/// FCW after FNINIT: everything masked, extended precision, round nearest.
pub const CONTROL_INIT: u16 = 0x037F;

/// The size of the context-switch image `save`/`load` move: FCW, FSW,
/// tags, padding, then the eight registers in 10-byte physical order.
pub const IMAGE_SIZE: usize = 8 + 80;

/// The 32-bit-protected-mode environment image (`fnstenv`).
pub const ENVIRONMENT_SIZE: usize = 28;

/// Environment plus the eight registers in logical order (`fnsave`).
pub const SAVE_SIZE: usize = 108;

/// How much of an `fxsave` area belongs to the floating-point unit.
///
/// The whole area is 512 bytes; the first 160 are the unit's — the control
/// and status words, the tag byte, the instruction and data pointers, and
/// the eight stack registers — and everything above is `MXCSR` and the
/// vector registers, which are not this crate's.
pub const FX_FPU_SIZE: usize = 160;

/// Cloneable because the interpreter's thread control block owns one per
/// thread: a context switch is a struct move and a snapshot is a copy, and
/// both need this to come along. Nothing in the crate's own FFI arrangement
/// changes — the wasm statics are still the statics.
#[derive(Clone)]
pub struct X87State {
    control: u16,
    /// Sticky exception flags, ES, SF and the condition codes — everything
    /// in FSW except TOP, which lives beside it and is composed on read.
    status: u16,
    top: u8,
    /// Abridged tags: bit *i* set means physical register *i* is occupied,
    /// the FXSAVE convention. The full 2-bit encoding is reconstructed
    /// from the values when an environment image needs it.
    tags: u8,
    registers: [F80; 8],
}

impl Default for X87State {
    fn default() -> Self {
        Self::new()
    }
}

impl X87State {
    pub const fn new() -> Self {
        Self {
            control: CONTROL_INIT,
            status: 0,
            top: 0,
            tags: 0,
            registers: [F80::ZERO; 8],
        }
    }

    /// What `execve` hands a fresh process: the control word at its
    /// default, no exceptions, an empty stack — *and* the register data
    /// zeroed, because one process must not be handed another's bytes.
    ///
    /// This is deliberately not `FNINIT`. See [`X87State::reinitialize`]
    /// for the difference, which is small, guest-observable, and was found
    /// by comparing against hardware rather than by reading the manual.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// `FNINIT`, and the reinitialization half of `FNSAVE`: the control
    /// word to its default, the status word cleared, the stack marked
    /// empty — and the register *data* left exactly where it was.
    ///
    /// Hardware does not erase the eighty bytes; it marks them unreachable.
    /// The difference is observable, because a second `fnsave` stores the
    /// contents of registers the first one emptied, and because `frstor`
    /// of a tag word that claims occupancy resurrects whatever is there.
    /// Zeroing here instead is the kind of divergence that is invisible
    /// until a `feholdexcept`-and-restore sequence answers wrongly.
    pub fn reinitialize(&mut self) {
        self.control = CONTROL_INIT;
        self.status = 0;
        self.top = 0;
        self.tags = 0;
    }

    pub fn rounding(&self) -> Rounding {
        Rounding::from_control(self.control)
    }

    pub fn precision(&self) -> Precision {
        Precision::from_control(self.control)
    }

    pub fn control(&self) -> u16 {
        self.control
    }

    /// `fldcw`. Reserved bit 6 reads back set, matching what the hardware
    /// stores; an unmasking is recorded (ES may go pending) but nothing is
    /// delivered yet.
    pub fn set_control(&mut self, value: u16) {
        self.control = (value & 0x1F3F) | 0x0040;
        self.refresh_summary();
    }

    /// `fnstsw`: the architectural FSW, TOP composed into bits 11–13.
    pub fn status_word(&self) -> u16 {
        self.status | ((self.top as u16) << 11)
    }

    /// `fnclex`: exceptions, the summary, and busy — the condition codes
    /// survive.
    pub fn clear_exceptions(&mut self) {
        self.status &= !(EXCEPTIONS | STACK_FAULT | ERROR_SUMMARY);
    }

    /// Folds an operation's flag word in: exceptions accumulate, the
    /// condition codes it produced replace the old ones it names, and C1
    /// is always overwritten — every operation states it.
    fn merge(&mut self, flags: u16, condition_mask: u16) {
        self.status |= flags & (EXCEPTIONS | STACK_FAULT);
        let conditions = (condition_mask | C1) & CONDITIONS;
        self.status = (self.status & !conditions) | (flags & conditions);
        self.refresh_summary();
    }

    /// An arithmetic result's flags: C1 stated, C0/C2/C3 untouched
    /// (architecturally undefined, and leaving them alone is a behavior
    /// the oracle cannot distinguish from any other).
    pub(crate) fn merge_arithmetic(&mut self, flags: u16) {
        self.merge(flags, C1);
    }

    /// A comparison or classification: all four condition codes stated.
    pub(crate) fn merge_conditions(&mut self, flags: u16) {
        self.merge(flags, CONDITIONS);
    }

    fn refresh_summary(&mut self) {
        let unmasked = self.status & EXCEPTIONS & !(self.control & 0x3F);
        if unmasked != 0 {
            self.status |= ERROR_SUMMARY;
        } else {
            self.status &= !ERROR_SUMMARY;
        }
    }

    fn physical(&self, index: u32) -> usize {
        (self.top as usize + index as usize) & 7
    }

    pub fn is_empty(&self, index: u32) -> bool {
        self.tags & (1 << self.physical(index)) == 0
    }

    /// Reads ST(i). An empty register is a stack underflow: invalid and
    /// stack-fault flags, C1 clear, and the indefinite as the masked
    /// operand.
    pub(crate) fn read(&mut self, index: u32) -> F80 {
        if self.is_empty(index) {
            self.merge(INVALID | STACK_FAULT, C1);
            F80::INDEFINITE
        } else {
            self.registers[self.physical(index)]
        }
    }

    /// Reads ST(i) without the fault machinery — for `fxam` and the
    /// images, which have their own rules for empty.
    pub(crate) fn peek(&self, index: u32) -> F80 {
        self.registers[self.physical(index)]
    }

    pub(crate) fn write(&mut self, index: u32, value: F80) {
        let physical = self.physical(index);
        self.registers[physical] = value;
        self.tags |= 1 << physical;
    }

    /// Pushes. A full destination is a stack overflow: invalid and stack
    /// fault with C1 set, and the indefinite replaces what would have been
    /// pushed.
    pub(crate) fn push(&mut self, value: F80) {
        self.top = self.top.wrapping_sub(1) & 7;
        if self.tags & (1 << self.top) != 0 {
            self.merge(INVALID | STACK_FAULT | C1, C1);
            self.registers[self.top as usize] = F80::INDEFINITE;
        } else {
            self.tags |= 1 << self.top;
            self.registers[self.top as usize] = value;
        }
    }

    pub(crate) fn pop(&mut self) {
        self.tags &= !(1 << self.top);
        self.top = (self.top + 1) & 7;
    }

    pub(crate) fn free(&mut self, index: u32) {
        let physical = self.physical(index);
        self.tags &= !(1 << physical);
    }

    pub(crate) fn rotate(&mut self, towards_top: bool) {
        self.top = if towards_top {
            self.top.wrapping_sub(1) & 7
        } else {
            (self.top + 1) & 7
        };
        // fincstp/fdecstp clear C1 and touch no tags.
        self.merge(0, C1);
    }

    /// The full 2-bit tag for a physical register, reconstructed from the
    /// value: 00 valid, 01 zero, 10 special, 11 empty.
    fn full_tag(&self, physical: usize) -> u16 {
        if self.tags & (1 << physical) == 0 {
            return 3;
        }
        match self.registers[physical].classify() {
            Class::Zero => 1,
            Class::Normal => 0,
            _ => 2,
        }
    }

    fn full_tag_word(&self) -> u16 {
        let mut word = 0;
        for physical in 0..8 {
            word |= self.full_tag(physical) << (physical * 2);
        }
        word
    }

    /// `fnstenv`, and — its documented side effect, the entire reason
    /// `feholdexcept` exists — masks every exception in the live FCW.
    /// FIP/FDP and their selectors are stored as zeros, which nothing in
    /// either libc reads.
    pub fn store_environment(&mut self, image: &mut [u8; ENVIRONMENT_SIZE]) {
        image.fill(0);
        image[0..2].copy_from_slice(&self.control.to_le_bytes());
        image[4..6].copy_from_slice(&self.status_word().to_le_bytes());
        image[8..10].copy_from_slice(&self.full_tag_word().to_le_bytes());
        self.control |= 0x3F;
        self.refresh_summary();
    }

    /// `fldenv`. Register contents are untouched; the tag word decides
    /// only occupancy, since the class is always recomputable from the
    /// value.
    pub fn load_environment(&mut self, image: &[u8; ENVIRONMENT_SIZE]) {
        self.set_control(u16::from_le_bytes(image[0..2].try_into().unwrap()));
        let status = u16::from_le_bytes(image[4..6].try_into().unwrap());
        self.top = ((status >> 11) & 7) as u8;
        self.status = status & !(7 << 11);
        let full = u16::from_le_bytes(image[8..10].try_into().unwrap());
        self.tags = 0;
        for physical in 0..8 {
            if (full >> (physical * 2)) & 3 != 3 {
                self.tags |= 1 << physical;
            }
        }
        self.refresh_summary();
    }

    /// `fnsave`: the environment, the registers in logical order, then
    /// FNINIT.
    pub fn store_and_reinitialize(&mut self, image: &mut [u8; SAVE_SIZE]) {
        let mut environment = [0u8; ENVIRONMENT_SIZE];
        // fnsave does not mask exceptions the way fnstenv does — the
        // reinitialization below resets the control word entirely, so the
        // side effect would be unobservable anyway; undo it to keep the
        // stored FCW honest.
        let control = self.control;
        self.store_environment(&mut environment);
        self.control = control;
        image[..ENVIRONMENT_SIZE].copy_from_slice(&environment);
        for logical in 0..8 {
            let bytes = self.peek(logical).to_bytes();
            let offset = ENVIRONMENT_SIZE + logical as usize * 10;
            image[offset..offset + 10].copy_from_slice(&bytes);
        }
        self.reinitialize();
    }

    /// `frstor`.
    pub fn load_saved(&mut self, image: &[u8; SAVE_SIZE]) {
        let environment: [u8; ENVIRONMENT_SIZE] =
            image[..ENVIRONMENT_SIZE].try_into().unwrap();
        self.load_environment(&environment);
        for logical in 0..8 {
            let offset = ENVIRONMENT_SIZE + logical as usize * 10;
            let bytes: [u8; 10] = image[offset..offset + 10].try_into().unwrap();
            let physical = self.physical(logical);
            self.registers[physical] = F80::from_bytes(bytes);
        }
    }

    /// The floating-point half of an `fxsave` area.
    ///
    /// A different layout from [`X87State::store_and_reinitialize`] in three
    /// ways that all matter: the tag *word* becomes a tag *byte*, one
    /// abridged bit per physical register rather than two bits of class;
    /// each stack register occupies sixteen bytes of which ten are used; and
    /// the registers are in logical order, `ST(0)` first, so a rotation by
    /// `TOP` stands between this and how they are held.
    ///
    /// `FIP` and `FDP` are stored as zeros, as they are for `fnstenv` and
    /// for the same reason: they are the address of the last x87 instruction
    /// and of its operand, nothing in either libc reads them, and the one
    /// caller that matters — `_dl_runtime_resolve`, which saves and restores
    /// the whole unit around a symbol lookup — only ever hands them back.
    pub fn store_fx(&self, image: &mut [u8; FX_FPU_SIZE]) {
        image.fill(0);
        image[0..2].copy_from_slice(&self.control.to_le_bytes());
        image[2..4].copy_from_slice(&self.status_word().to_le_bytes());
        image[4] = self.tags;
        for logical in 0..8u32 {
            let bytes = self.peek(logical).to_bytes();
            let offset = 32 + logical as usize * 16;
            image[offset..offset + 10].copy_from_slice(&bytes);
        }
    }

    /// The inverse: `fxrstor`.
    pub fn load_fx(&mut self, image: &[u8; FX_FPU_SIZE]) {
        self.set_control(u16::from_le_bytes(image[0..2].try_into().unwrap()));
        let status = u16::from_le_bytes(image[2..4].try_into().unwrap());
        self.top = ((status >> 11) & 7) as u8;
        self.status = status & !(7 << 11);
        self.tags = image[4];
        // The registers are logical, and `TOP` has just been set from the
        // status word, so `physical` means the right thing here.
        for logical in 0..8u32 {
            let offset = 32 + logical as usize * 16;
            let bytes: [u8; 10] = image[offset..offset + 10].try_into().unwrap();
            let physical = self.physical(logical);
            self.registers[physical] = F80::from_bytes(bytes);
        }
        self.refresh_summary();
    }

    /// The context-switch image: everything, byte-faithful, in physical
    /// order. Layout is this crate's to own — the kernel copies blobs of
    /// [`IMAGE_SIZE`] and never looks inside.
    pub fn save_image(&self, image: &mut [u8; IMAGE_SIZE]) {
        image[0..2].copy_from_slice(&self.control.to_le_bytes());
        image[2..4].copy_from_slice(&self.status.to_le_bytes());
        image[4] = self.top;
        image[5] = self.tags;
        image[6] = 0;
        image[7] = 0;
        for physical in 0..8 {
            let offset = 8 + physical * 10;
            image[offset..offset + 10].copy_from_slice(&self.registers[physical].to_bytes());
        }
    }

    pub fn load_image(&mut self, image: &[u8; IMAGE_SIZE]) {
        self.control = u16::from_le_bytes(image[0..2].try_into().unwrap());
        self.status = u16::from_le_bytes(image[2..4].try_into().unwrap());
        self.top = image[4] & 7;
        self.tags = image[5];
        for physical in 0..8 {
            let offset = 8 + physical * 10;
            self.registers[physical] =
                F80::from_bytes(image[offset..offset + 10].try_into().unwrap());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags;

    #[test]
    fn push_pop_and_top() {
        let mut state = X87State::new();
        state.push(F80::ONE);
        assert_eq!(state.top, 7);
        assert!(!state.is_empty(0));
        assert!(state.is_empty(1));
        assert_eq!(state.read(0), F80::ONE);
        state.pop();
        assert_eq!(state.top, 0);
        assert!(state.is_empty(0));
    }

    #[test]
    fn stack_overflow_is_a_masked_fault() {
        let mut state = X87State::new();
        for _ in 0..8 {
            state.push(F80::ONE);
        }
        assert_eq!(state.status_word() & (INVALID | STACK_FAULT), 0);
        state.push(F80::ONE);
        let status = state.status_word();
        assert!(status & INVALID != 0);
        assert!(status & STACK_FAULT != 0);
        assert!(status & C1 != 0, "overflow direction");
        assert_eq!(state.read(0), F80::INDEFINITE);
    }

    #[test]
    fn stack_underflow_reads_indefinite() {
        let mut state = X87State::new();
        assert_eq!(state.read(0), F80::INDEFINITE);
        let status = state.status_word();
        assert!(status & INVALID != 0 && status & STACK_FAULT != 0);
        assert_eq!(status & C1, 0, "underflow direction");
    }

    #[test]
    fn summary_follows_masks() {
        let mut state = X87State::new();
        state.merge_arithmetic(flags::PRECISION);
        assert_eq!(
            state.status_word() & ERROR_SUMMARY,
            0,
            "masked exceptions do not summarize"
        );
        state.set_control(CONTROL_INIT & !flags::PRECISION);
        assert!(
            state.status_word() & ERROR_SUMMARY != 0,
            "unmasking a pending exception raises ES"
        );
        state.clear_exceptions();
        assert_eq!(state.status_word() & ERROR_SUMMARY, 0);
    }

    #[test]
    fn environment_round_trip_and_the_fnstenv_mask() {
        let mut state = X87State::new();
        state.push(F80::ONE);
        state.push(F80::ZERO);
        state.push(F80::INDEFINITE);
        state.set_control(0x0C00 | 0x001F); // chop, one exception unmasked
        state.merge_arithmetic(flags::PRECISION);
        let control_before = state.control();
        let status_before = state.status_word();
        let mut image = [0u8; ENVIRONMENT_SIZE];
        state.store_environment(&mut image);
        assert_eq!(
            state.control() & 0x3F,
            0x3F,
            "fnstenv masks all exceptions"
        );
        let mut other = X87State::new();
        other.load_environment(&image);
        assert_eq!(other.control(), control_before);
        assert_eq!(other.status_word(), status_before);
        assert_eq!(other.is_empty(0), false);
        assert_eq!(other.is_empty(3), true);
        // Full tags in the image: ST(0) is a NaN (special), ST(1) zero,
        // ST(2) valid, the rest empty. Physical slots 5,6,7 hold logical
        // 2,1,0.
        let full = u16::from_le_bytes(image[8..10].try_into().unwrap());
        assert_eq!((full >> (5 * 2)) & 3, 0b10, "NaN tags special");
        assert_eq!((full >> (6 * 2)) & 3, 0b01, "zero tags zero");
        assert_eq!((full >> (7 * 2)) & 3, 0b00, "one tags valid");
        assert_eq!((full >> (0 * 2)) & 3, 0b11, "unused tags empty");
    }

    #[test]
    fn save_reinitializes_and_restore_round_trips() {
        let mut state = X87State::new();
        state.push(F80::ONE);
        state.push(F80::INDEFINITE);
        state.merge_conditions(flags::C3 | flags::PRECISION);
        let mut image = [0u8; SAVE_SIZE];
        let status_before = state.status_word();
        state.store_and_reinitialize(&mut image);
        assert_eq!(state.control(), CONTROL_INIT);
        assert_eq!(state.status_word(), 0);
        assert!(state.is_empty(0));
        let mut other = X87State::new();
        other.load_saved(&image);
        assert_eq!(other.status_word(), status_before);
        assert_eq!(other.read(0), F80::INDEFINITE);
        assert_eq!(other.read(1), F80::ONE);
    }

    #[test]
    fn switch_image_round_trips() {
        let mut state = X87State::new();
        state.push(F80::ONE);
        state.push(F80::ONE.negate());
        state.set_control(0x0F3F);
        state.merge_conditions(flags::C0 | flags::INVALID);
        let mut image = [0u8; IMAGE_SIZE];
        state.save_image(&mut image);
        let mut other = X87State::new();
        other.load_image(&image);
        assert_eq!(other.control(), state.control());
        assert_eq!(other.status_word(), state.status_word());
        assert_eq!(other.peek(0), F80::ONE.negate());
        assert_eq!(other.peek(1), F80::ONE);
        assert_eq!(other.is_empty(2), true);
    }
}
