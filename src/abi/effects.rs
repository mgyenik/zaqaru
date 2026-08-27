//! What one instruction does to the emulated machine's registers.
//!
//! This is a *facts* layer, not a representation. It answers two questions —
//! which registers an instruction reads, and which it completely overwrites —
//! and nothing else. Signature inference needs those two sets; a later
//! flag-liveness pass will need the same shape; and neither needs the
//! instruction translated into anything first.
//!
//! **Why this is not built on the translator.** The analysis has to work on
//! every instruction in the input, including instructions the translator
//! cannot handle — which is the *main* case, not a corner one, because
//! pointing zaqaru at a stripped binary and asking what its signatures are is
//! most useful exactly when the binary does not yet fully translate. So the
//! register sets come from iced-x86's instruction information, which is
//! derived from the vendor tables and covers the whole instruction set.
//!
//! Three details are load-bearing, and all three were measured rather than
//! assumed:
//!
//! - A 32-bit write zero-extends into the full register on x86-64, and iced
//!   already reports it that way: `mov edi, eax` comes back as a write of
//!   `RDI`, eight bytes wide. So "does this write kill the register" reduces
//!   to a size check, with no special case of our own.
//! - A narrower write does *not* kill. `mov dil, al` leaves bits 8–63 alone,
//!   so a later read can still see an argument that arrived there.
//! - The SSE merge/zero asymmetry survives into the register sets: the
//!   register form of `movss` reports `ReadWrite` because it merges, while
//!   the memory form of `movsd` reports `Write` because it zeroes the upper
//!   lanes. Nothing here has to know that rule; it only has to not flatten
//!   the distinction iced is already making.

use iced_x86::{Instruction, InstructionInfoFactory, OpAccess, Register};

/// A place in the emulated machine that an argument can travel in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Location {
    /// A general-purpose register, numbered as the machine model numbers them.
    Integer(usize),
    /// An XMM register, by number.
    Float(usize),
}

/// A set of machine locations, one bit each.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct LocationSet {
    integers: u16,
    floats: u16,
}

impl LocationSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, location: Location) {
        match location {
            Location::Integer(number) => self.integers |= 1 << number,
            Location::Float(number) => self.floats |= 1 << number,
        }
    }

    pub fn contains(&self, location: Location) -> bool {
        match location {
            Location::Integer(number) => self.integers & (1 << number) != 0,
            Location::Float(number) => self.floats & (1 << number) != 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.integers == 0 && self.floats == 0
    }

    pub fn union_with(&mut self, other: Self) {
        self.integers |= other.integers;
        self.floats |= other.floats;
    }

    pub fn remove_all(&mut self, other: Self) {
        self.integers &= !other.integers;
        self.floats &= !other.floats;
    }

    pub fn union(mut self, other: Self) -> Self {
        self.union_with(other);
        self
    }

    /// Everything in `self` that is not in `other`.
    pub fn difference(mut self, other: Self) -> Self {
        self.remove_all(other);
        self
    }

    /// Every location in the set: integer registers in numeric order, then
    /// the XMM registers likewise.
    pub fn iter(self) -> impl Iterator<Item = Location> {
        (0..16)
            .filter(move |number| self.integers & (1 << number) != 0)
            .map(Location::Integer)
            .chain(
                (0..16)
                    .filter(move |number| self.floats & (1 << number) != 0)
                    .map(Location::Float),
            )
    }
}

/// What an instruction does to the machine's registers.
#[derive(Clone, Copy, Default, Debug)]
pub struct Effects {
    /// Locations any part of which is read.
    pub reads: LocationSet,
    /// Locations any part of which is written.
    ///
    /// Distinct from [`Self::kills`] on purpose: `add rax, 1` writes rax
    /// without killing it, because the old value contributed. Liveness wants
    /// the kills; "did this function produce a result" wants the writes.
    pub writes: LocationSet,
    /// Locations written in their entirety, so that a later read cannot see
    /// what was there before. A partial write is deliberately absent from
    /// this set: it leaves something behind.
    pub kills: LocationSet,
}

/// The machine location a register names, if it is one the model tracks.
///
/// Registers outside the model — the instruction pointer, segment registers,
/// the flags — return `None`. Flags are tracked separately by the translator
/// and are not part of any calling convention.
pub fn location_of(register: Register) -> Option<Location> {
    let full = register.full_register();
    if full.is_gpr64() {
        Some(Location::Integer(full.number()))
    } else if full.is_zmm() {
        // XMM, YMM and ZMM all normalise to ZMM; the number is the same, and
        // only the low 128 bits are ever part of the SysV scalar convention.
        Some(Location::Float(full.number()))
    } else {
        None
    }
}

/// Whether writing this register width overwrites the whole thing.
///
/// Eight bytes is a full general-purpose register. Sixteen is a full XMM
/// register. Anything narrower leaves bits behind — and iced has already
/// widened the 32-bit case, which zero-extends on x86-64, so it arrives here
/// as eight.
fn write_kills(register: Register) -> bool {
    let size = register.size();
    if register.is_gpr() {
        size >= 8
    } else {
        size >= 16
    }
}

/// Reads and kills for one instruction, taken on its own.
///
/// "On its own" is the limitation callers have to make up for: a `call`
/// reports nothing but its effect on the stack pointer, because what the
/// callee reads is not a property of the call instruction. Supplying that is
/// the interprocedural pass's job.
pub fn effects_of(instruction: &Instruction, factory: &mut InstructionInfoFactory) -> Effects {
    let mut effects = Effects::default();
    for used in factory.info(instruction).used_registers() {
        let Some(location) = location_of(used.register()) else {
            continue;
        };
        match used.access() {
            OpAccess::Read | OpAccess::CondRead => effects.reads.insert(location),
            OpAccess::Write => {
                effects.writes.insert(location);
                if write_kills(used.register()) {
                    effects.kills.insert(location);
                }
            }
            // Read *and* written, so the old value contributed to the new
            // one. That makes the location live whatever else is true, and
            // recording a kill as well would be a claim this cannot support:
            // the register form of `movss` reports sixteen bytes read-written
            // while actually replacing four, because the merge is expressed
            // through the access rather than the size.
            OpAccess::ReadWrite => {
                effects.reads.insert(location);
                effects.writes.insert(location);
            }
            // A conditional write may not happen, so it cannot be counted on
            // to have destroyed anything.
            OpAccess::CondWrite => effects.writes.insert(location),
            OpAccess::ReadCondWrite => {
                effects.reads.insert(location);
                effects.writes.insert(location);
            }
            OpAccess::NoMemAccess | OpAccess::None => {}
        }
    }
    effects
}

/// How wide a value in a register is being treated as, at one read of it.
///
/// This is the evidence integer width has to be reconstructed from, and it is
/// evidence rather than proof — which is why it is reported per read and
/// combined by the caller rather than decided here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WidthEvidence {
    /// Read as a full 64-bit register operand: the value really is 64 bits
    /// wide.
    Quad,
    /// Read as a narrower register operand, so the upper half is not part of
    /// the value.
    Narrow,
    /// Read as part of a memory address. On a wasm32 target this is the
    /// strongest evidence there is for `i32`, because an address *is* 32 bits
    /// there — and it has to be distinguished from a 64-bit register read,
    /// since address arithmetic always uses the full register even when the
    /// value is a 32-bit `int`. Without this distinction `lea eax,[rdi+1]`
    /// would make every `int` parameter look like an `i64`.
    Address,
}

/// Width evidence from one instruction, for each location it *reads*.
///
/// Taken from the per-register access information rather than from the
/// operands, because a location is not fine-grained enough to filter on:
/// `movslq %edi,%rdi` reads EDI and writes RDI, and both are the same
/// *location*, so anything deciding "is this location read here" would let
/// the 64-bit write through and conclude the incoming value was 64 bits wide.
/// Each register entry carries its own access and its own size, which is
/// exactly the distinction needed.
///
/// A register being written says nothing about the width of the value that
/// arrived in it — `lea 0xc(%rsp),%rdi` names rdi as a 64-bit operand while
/// destroying whatever the caller put there — so only reads are reported.
pub fn width_evidence(
    instruction: &Instruction,
    factory: &mut InstructionInfoFactory,
    evidence: &mut Vec<(Location, WidthEvidence)>,
) {
    evidence.clear();

    // `lea` borrows the address syntax to do arithmetic: it computes what an
    // address *would* be and never touches memory. Its base and index are
    // ordinary arithmetic operands, so how wide they are is said by the
    // destination — `lea (%rdi,%rsi),%rax` is a 64-bit add of two 64-bit
    // values, while `lea 0x1(%rdi),%eax` is a 32-bit one. Treating either as
    // an address would be the mirror of the mistake this function exists to
    // avoid.
    let computing_an_address = instruction.mnemonic() == iced_x86::Mnemonic::Lea;
    let addressing = if computing_an_address {
        let destination = instruction.op0_register();
        if destination.is_gpr() && destination.size() >= 8 {
            WidthEvidence::Quad
        } else {
            WidthEvidence::Narrow
        }
    } else {
        WidthEvidence::Address
    };

    let mut addresses = LocationSet::new();
    for register in [instruction.memory_base(), instruction.memory_index()] {
        if let Some(location) = location_of(register) {
            addresses.insert(location);
        }
    }

    for used in factory.info(instruction).used_registers() {
        if !matches!(
            used.access(),
            OpAccess::Read | OpAccess::CondRead | OpAccess::ReadWrite | OpAccess::ReadCondWrite
        ) {
            continue;
        }
        let Some(location) = location_of(used.register()) else {
            continue;
        };
        let width = if addresses.contains(location) {
            addressing
        } else if used.register().is_gpr() && used.register().size() >= 8 {
            WidthEvidence::Quad
        } else {
            WidthEvidence::Narrow
        };
        evidence.push((location, width));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Instruction {
        let mut decoder = iced_x86::Decoder::with_ip(64, bytes, 0, iced_x86::DecoderOptions::NONE);
        decoder.decode()
    }

    fn effects(bytes: &[u8]) -> Effects {
        let mut factory = InstructionInfoFactory::new();
        effects_of(&decode(bytes), &mut factory)
    }

    const RDI: Location = Location::Integer(7);
    const RSI: Location = Location::Integer(6);
    const RAX: Location = Location::Integer(0);
    const XMM0: Location = Location::Float(0);
    const XMM1: Location = Location::Float(1);

    #[test]
    fn a_thirty_two_bit_write_kills_the_whole_register() {
        // mov edi, eax — zero-extends, so nothing of the old rdi survives.
        let effects = effects(&[0x89, 0xc7]);
        assert!(effects.kills.contains(RDI));
        assert!(!effects.reads.contains(RDI));
        assert!(effects.reads.contains(RAX));
    }

    #[test]
    fn a_byte_write_leaves_the_rest_alone() {
        // mov dil, al — bits 8..63 of rdi survive, so this is not a kill.
        let effects = effects(&[0x40, 0x88, 0xc7]);
        assert!(!effects.kills.contains(RDI));
    }

    #[test]
    fn the_zeroing_idiom_reads_nothing() {
        // xor eax, eax is a definition, not a use of the old value.
        let effects = effects(&[0x31, 0xc0]);
        assert!(effects.kills.contains(RAX));
        assert!(!effects.reads.contains(RAX));
    }

    #[test]
    fn the_sse_merge_and_zero_forms_differ() {
        // movss xmm0, xmm1 merges into the low lane: xmm0 is read.
        let merging = effects(&[0xf3, 0x0f, 0x10, 0xc1]);
        assert!(merging.reads.contains(XMM0));
        assert!(merging.reads.contains(XMM1));
        assert!(!merging.kills.contains(XMM0));

        // movsd xmm0, [rax] zeroes the upper lane: xmm0 is not read.
        let zeroing = effects(&[0xf2, 0x0f, 0x10, 0x00]);
        assert!(!zeroing.reads.contains(XMM0));
        assert!(zeroing.kills.contains(XMM0));
        assert!(zeroing.reads.contains(RAX));
    }

    #[test]
    fn a_call_says_nothing_about_the_callee() {
        // Only the stack pointer. What the callee reads is not a property of
        // this instruction, which is why the analysis has to be
        // interprocedural.
        let effects = effects(&[0xe8, 0x00, 0x00, 0x00, 0x00]);
        assert!(!effects.reads.contains(RDI));
        assert!(!effects.kills.contains(RDI));
    }

    fn evidence(bytes: &[u8]) -> Vec<(Location, WidthEvidence)> {
        let instruction = decode(bytes);
        let mut factory = InstructionInfoFactory::new();
        let mut found = Vec::new();
        width_evidence(&instruction, &mut factory, &mut found);
        found
    }

    #[test]
    fn a_widening_move_is_evidence_of_the_narrow_source() {
        // movslq %edi,%rdi reads EDI and writes RDI. Both are the same
        // location, so only the per-register access distinguishes the
        // 32-bit value that arrived from the 64-bit one being made.
        let found = evidence(&[0x48, 0x63, 0xff]);
        assert!(found.contains(&(RDI, WidthEvidence::Narrow)));
        assert!(!found.contains(&(RDI, WidthEvidence::Quad)));
    }

    #[test]
    fn lea_is_arithmetic_not_addressing() {
        // lea (%rdi,%rsi,1),%rax adds two 64-bit values; nothing is read
        // from memory, so neither operand is an address.
        let found = evidence(&[0x48, 0x8d, 0x04, 0x37]);
        assert!(found.contains(&(RDI, WidthEvidence::Quad)));
        assert!(found.contains(&(RSI, WidthEvidence::Quad)));
        assert!(
            !found
                .iter()
                .any(|(_, width)| *width == WidthEvidence::Address)
        );
    }

    #[test]
    fn writing_a_register_is_not_evidence_about_what_arrived_in_it() {
        // lea 0xc(%rsp),%rdi names rdi as a 64-bit operand while destroying
        // the argument that was there. Counting it would make every reused
        // argument register look like an i64.
        let found = evidence(&[0x48, 0x8d, 0x7c, 0x24, 0x0c]);
        assert!(!found.iter().any(|(location, _)| *location == RDI));
    }

    #[test]
    fn a_write_is_recorded_even_when_it_is_not_a_kill() {
        // add rax, 1 writes rax without killing it. Result inference needs
        // to see the write; liveness needs to not see a kill.
        let mut factory = InstructionInfoFactory::new();
        let effects = effects_of(&decode(&[0x48, 0x83, 0xc0, 0x01]), &mut factory);
        assert!(effects.writes.contains(RAX));
        assert!(effects.reads.contains(RAX));
        assert!(!effects.kills.contains(RAX));
    }

    #[test]
    fn a_real_memory_access_marks_its_base_as_an_address() {
        // mov (%rdi),%eax — rdi is an address, which on a wasm32 target is
        // 32 bits however wide the register holding it is.
        let found = evidence(&[0x8b, 0x07]);
        assert!(found.contains(&(RDI, WidthEvidence::Address)));
        assert!(!found.contains(&(RDI, WidthEvidence::Quad)));
    }

    #[test]
    fn lea_takes_its_width_from_the_destination() {
        // lea 0x1(%rdi),%eax computes a 32-bit value, so rdi is being used as
        // a 32-bit one. Reading this as a 64-bit access — which it is, at the
        // hardware level — would mistype every `int` reached through `lea`.
        let found = evidence(&[0x8d, 0x47, 0x01]);
        assert!(found.contains(&(RDI, WidthEvidence::Narrow)));
        assert!(!found.contains(&(RDI, WidthEvidence::Quad)));
    }

    #[test]
    fn a_sixty_four_bit_operand_is_evidence_of_a_wide_value() {
        // add rdi, rsi
        let found = evidence(&[0x48, 0x01, 0xf7]);
        assert!(found.contains(&(RDI, WidthEvidence::Quad)));
        assert!(found.contains(&(RSI, WidthEvidence::Quad)));
    }

    #[test]
    fn a_narrow_operand_is_evidence_of_a_narrow_value() {
        // mov eax, edi
        let found = evidence(&[0x89, 0xf8]);
        assert!(found.contains(&(RDI, WidthEvidence::Narrow)));
        assert!(!found.contains(&(RDI, WidthEvidence::Quad)));
    }
}
