//! The thread control block: the whole machine, as a struct.
//!
//! This is the design's load-bearing simplification. Under the transpiler a
//! guest register is a wasm global, a context switch is a marshalling
//! exercise through a 412-byte image, and a suspended thread is a chain of
//! resume IDs threaded through the guest stack. Here a register is a field,
//! a context switch is a pointer swap, and a suspended thread is a `Tcb`
//! nobody is advancing.
//!
//! The register file is an array indexed by the x86 encoding number rather
//! than sixteen named fields, because an interpreter's operand access is
//! indexed: `instruction.op_register(0)` produces a number and the read has
//! to be a subscript. On wasm32 a Rust array *is* linear memory, so the
//! indexing cost is the same load a global would have been, and the spike's
//! measurements already include it.

use iced_x86::Register;

use crate::flags::Flags;

/// How many general-purpose registers the machine has, and the length of
/// [`Tcb::registers`].
pub const REGISTER_COUNT: usize = 16;

/// How many XMM registers the machine has.
pub const VECTOR_REGISTER_COUNT: usize = 16;

/// Index of `%rsp` in the register file, which boot and every push and pop
/// name directly.
pub const STACK_POINTER: usize = 4;

/// The width of an operand.
///
/// Four widths and no more: x86-64's integer operands are one, two, four or
/// eight bytes, and everything wider is a vector operand with its own path.
/// The distinction that matters most is not the size but what a *write* of
/// each size does to the rest of the register, which [`Tcb::write_register`]
/// is the single statement of.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
#[repr(u8)]
pub enum Width {
    Byte = 0,
    Word = 1,
    Dword = 2,
    Qword = 3,
}

impl Width {
    pub fn from_bytes(bytes: usize) -> Option<Self> {
        match bytes {
            1 => Some(Width::Byte),
            2 => Some(Width::Word),
            4 => Some(Width::Dword),
            8 => Some(Width::Qword),
            _ => None,
        }
    }

    pub const fn bytes(self) -> u32 {
        match self {
            Width::Byte => 1,
            Width::Word => 2,
            Width::Dword => 4,
            Width::Qword => 8,
        }
    }

    pub const fn bits(self) -> u32 {
        self.bytes() * 8
    }

    /// The significant bits of a value at this width, as a mask.
    pub const fn mask(self) -> u64 {
        match self {
            Width::Qword => u64::MAX,
            width => (1u64 << width.bits()) - 1,
        }
    }

    /// The bit that carries the sign at this width.
    pub const fn sign_bit(self) -> u64 {
        1u64 << (self.bits() - 1)
    }

    /// Sign-extends a value held at this width into a full `u64`.
    pub const fn sign_extend(self, value: u64) -> u64 {
        match self {
            Width::Qword => value,
            width => {
                let shift = 64 - width.bits();
                (((value << shift) as i64) >> shift) as u64
            }
        }
    }

    /// Truncates a value to this width.
    pub const fn truncate(self, value: u64) -> u64 {
        value & self.mask()
    }
}

/// Which bits of the register file a register operand names.
///
/// The `high_byte` flag is the whole reason this is a struct rather than a
/// pair: `%ah` and `%spl` are both one byte of register four, and only the
/// flag tells them apart. Getting it wrong is silent — a byte written eight
/// bits too low corrupts `%al` and nothing complains.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slice {
    /// Index into the register file, in x86 encoding order.
    ///
    /// A byte, for a number that is zero to fifteen. It used to be a
    /// `usize`, which cost nothing where a `Slice` is a local and a great
    /// deal once [`crate::quick`] started storing two of them per
    /// instruction for the life of a block: `Quick` was ninety-six bytes
    /// against `Instruction`'s forty, and workloads made mostly of
    /// instructions the lowering *declines* paid that for no benefit at all.
    /// `float` measured slower than before the fast path existed.
    pub number: u8,
    pub width: Width,
    /// True for `%ah`, `%ch`, `%dh` and `%bh`, which name bits 8..16.
    pub high_byte: bool,
}

impl Slice {
    /// The slice a general-purpose register operand names, or `None` when the
    /// register is not one — a segment register, `%rip`, an XMM register, a
    /// control register. Callers turn the `None` into their own loud error,
    /// because what the register *was* is the interesting half of the report.
    pub fn of(register: Register) -> Option<Self> {
        if !register.is_gpr() {
            return None;
        }
        let high_byte = matches!(
            register,
            Register::AH | Register::CH | Register::DH | Register::BH
        );
        Some(Self {
            number: register.full_register().number() as u8,
            width: Width::from_bytes(register.size())?,
            high_byte,
        })
    }

    /// The whole of a register named by its index, for the interpreter's own
    /// bookkeeping accesses — the stack pointer, the string-instruction
    /// pointers, the implicit accumulator — which always work full width.
    pub const fn quad(number: u8) -> Self {
        Self {
            number,
            width: Width::Qword,
            high_byte: false,
        }
    }
}

/// One thread's machine state, in full.
///
/// Everything a context switch has to move is here and nothing else is: no
/// wasm globals, no shadow stack, no resume chain. A snapshot is a copy of
/// this struct plus the pages of linear memory the thread can reach.
#[derive(Clone)]
pub struct Tcb {
    /// The general-purpose registers, indexed by encoding number.
    pub registers: [u64; REGISTER_COUNT],
    pub rip: u64,
    /// Retired instructions, this thread's share. The counter is the
    /// preemption quantum and the deterministic time base `rdtsc` answers
    /// from: one mechanism, two jobs, no polling emitted anywhere.
    pub retired: u64,
    /// The `%fs` base — the thread pointer, as far as every libc is
    /// concerned. `arch_prctl` is what moves it; guest code never writes it,
    /// because `wrfsbase` is not implemented and would be a loud error.
    pub fs_base: u64,
    /// The lazy-flag record. See [`crate::flags`] for why the flags are not
    /// six booleans.
    pub flags: Flags,
    /// The XMM registers, low half first. Two `u64`s rather than a `u128`
    /// because SSE's grain is the half — a scalar operation writes the low
    /// 64 bits and preserves the high 64, which here is "touch element 0".
    pub vectors: [[u64; 2]; VECTOR_REGISTER_COUNT],
    /// MXCSR. Held so that `stmxcsr`/`ldmxcsr` round-trip and so a guest
    /// that saves and restores it sees what it wrote; the rounding mode it
    /// carries is honoured by the scalar and packed conversions that read it.
    pub mxcsr: u32,
    /// The x87 and MMX state, per thread.
    ///
    /// The crate is already the interpreter for its domain, oracle and all;
    /// what changes here is only that its state is a field instead of a
    /// static, which is the `x87_save`/`x87_load` integration the thread
    /// design named, arriving in the simpler form.
    pub x87: x87::state::X87State,
    /// Instructions retired inside a bytecode trace, this thread's share —
    /// the numerator of the accelerated share, a run's measure of how much
    /// of the workload the transpiler covers and how much still defers to
    /// the interpreter.
    pub accelerated: u64,
}

impl Default for Tcb {
    fn default() -> Self {
        Self::new()
    }
}

impl Tcb {
    /// What `rdtsc` answers.
    ///
    /// A function of the retired-instruction counter, which makes it
    /// deterministic and replayable — two runs of the same container see the
    /// same timestamps — and monotone, because the counter is.
    ///
    /// It is not a clock, and the guest must not treat it as one: it has no
    /// relationship to elapsed time that the guest can know. The *kernel*
    /// may relate the two, and does — it holds both this counter and the
    /// host's clock, samples them together, and publishes the ratio for the
    /// vDSO to interpolate with. That is calibration done by the one party
    /// entitled to do it, and it is the whole of `kernel::vdso`.
    ///
    /// The multiplier is large so that a guest spinning on a deadline —
    /// "wait until the counter passes now plus n" — crosses it in a few
    /// reads instead of burning millions of iterations, and odd so that the
    /// low bits cycle: glibc's adaptive mutex takes exactly those bits as
    /// jitter for its backoff, and an even step would hand it the same
    /// value every time.
    pub fn timestamp(&self) -> u64 {
        self.retired.wrapping_mul(crate::exec::TIMESTAMP_STEP)
    }

    /// The state Linux hands `_start`: every register zero, and a stack
    /// pointer boot is expected to write before anything runs.
    pub fn new() -> Self {
        Self {
            registers: [0; REGISTER_COUNT],
            rip: 0,
            flags: Flags::new(),
            fs_base: 0,
            vectors: [[0; 2]; VECTOR_REGISTER_COUNT],
            // The reset value: every exception masked, round to nearest.
            mxcsr: 0x1f80,
            x87: x87::state::X87State::new(),
            retired: 0,
            accelerated: 0,
        }
    }

    /// Reads a register slice, zero-extended into a `u64`.
    // Two or three instructions once the width is known, and called
    // for every operand of every instruction. As its own function it
    // was a wasm call per register access.
    #[inline(always)]
    pub fn read_register(&self, slice: Slice) -> u64 {
        let whole = self.registers[slice.number as usize];
        let shifted = if slice.high_byte { whole >> 8 } else { whole };
        shifted & slice.width.mask()
    }

    /// Writes a register slice, following x86's write semantics exactly: a
    /// four-byte write zeroes the upper half of the register, and every
    /// narrower write leaves the rest of it alone.
    ///
    /// That asymmetry is not a curiosity. `xor %eax, %eax` is how every
    /// compiler zeroes `%rax`, and it only works because the 32-bit write
    /// clears the top; `mov %al, ...` next to it must not. One function
    /// states the rule so no instruction arm can get it wrong privately.
    // Two or three instructions once the width is known, and called
    // for every operand of every instruction. As its own function it
    // was a wasm call per register access.
    #[inline(always)]
    pub fn write_register(&mut self, slice: Slice, value: u64) {
        let slot = &mut self.registers[slice.number as usize];
        match (slice.width, slice.high_byte) {
            (Width::Qword, _) => *slot = value,
            (Width::Dword, _) => *slot = value & 0xffff_ffff,
            (width, high_byte) => {
                let shift = if high_byte { 8 } else { 0 };
                let field = width.mask() << shift;
                *slot = (*slot & !field) | ((value & width.mask()) << shift);
            }
        }
    }

    pub fn stack_pointer(&self) -> u64 {
        self.registers[STACK_POINTER]
    }

    pub fn set_stack_pointer(&mut self, value: u64) {
        self.registers[STACK_POINTER] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_four_byte_write_clears_the_upper_half() {
        let mut tcb = Tcb::new();
        tcb.registers[0] = 0xffff_ffff_ffff_ffff;
        tcb.write_register(
            Slice {
                number: 0,
                width: Width::Dword,
                high_byte: false,
            },
            0x1234_5678,
        );
        assert_eq!(tcb.registers[0], 0x1234_5678);
    }

    #[test]
    fn narrower_writes_preserve_the_rest_of_the_register() {
        let mut tcb = Tcb::new();
        tcb.registers[0] = 0xffff_ffff_ffff_ffff;
        tcb.write_register(
            Slice {
                number: 0,
                width: Width::Byte,
                high_byte: false,
            },
            0x12,
        );
        assert_eq!(tcb.registers[0], 0xffff_ffff_ffff_ff12);
        tcb.write_register(
            Slice {
                number: 0,
                width: Width::Word,
                high_byte: false,
            },
            0xabcd,
        );
        assert_eq!(tcb.registers[0], 0xffff_ffff_ffff_abcd);
    }

    #[test]
    fn the_high_byte_registers_name_bits_eight_to_sixteen() {
        let mut tcb = Tcb::new();
        tcb.registers[0] = 0x0000_0000_0000_ff00;
        let ah = Slice::of(Register::AH).expect("ah is a general-purpose register");
        assert_eq!(ah.number, 0);
        assert!(ah.high_byte);
        assert_eq!(tcb.read_register(ah), 0xff);
        tcb.write_register(ah, 0x42);
        assert_eq!(tcb.registers[0], 0x4200);
    }

    #[test]
    fn spl_and_ah_are_different_bytes_of_different_registers() {
        let spl = Slice::of(Register::SPL).expect("spl is a general-purpose register");
        assert_eq!(usize::from(spl.number), STACK_POINTER);
        assert!(!spl.high_byte);
        let ah = Slice::of(Register::AH).expect("ah is a general-purpose register");
        assert_eq!(ah.number, 0);
        assert!(ah.high_byte);
    }

    #[test]
    fn every_encoding_number_maps_to_its_own_slot() {
        const ORDER: [Register; REGISTER_COUNT] = [
            Register::RAX,
            Register::RCX,
            Register::RDX,
            Register::RBX,
            Register::RSP,
            Register::RBP,
            Register::RSI,
            Register::RDI,
            Register::R8,
            Register::R9,
            Register::R10,
            Register::R11,
            Register::R12,
            Register::R13,
            Register::R14,
            Register::R15,
        ];
        for (expected, register) in ORDER.iter().enumerate() {
            let slice = Slice::of(*register).expect("a general-purpose register");
            assert_eq!(usize::from(slice.number), expected, "{register:?}");
            assert_eq!(slice.width, Width::Qword);
        }
    }

    #[test]
    fn sign_extension_follows_the_width() {
        assert_eq!(Width::Byte.sign_extend(0xff), u64::MAX);
        assert_eq!(Width::Word.sign_extend(0x8000), 0xffff_ffff_ffff_8000);
        assert_eq!(Width::Dword.sign_extend(0x7fff_ffff), 0x7fff_ffff);
        assert_eq!(Width::Qword.sign_extend(0x8000_0000), 0x8000_0000);
    }
}

