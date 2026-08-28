//! The emulated x86-64 machine state, and how it is spelled in wasm.
//!
//! Registers are `i64` globals and flags are `i32` globals, defined *weakly*
//! by every translated object so that `wasm-ld` collapses them into one
//! register file when several objects are linked together. The guest stack
//! lives in linear memory with `x86_rsp` pointing into it, which is what
//! makes `push`/`pop`, spill slots and red-zone accesses work with no special
//! cases.

use anyhow::{Result, bail};
use iced_x86::Register;

use crate::abi::effects::{Location, LocationSet};
use crate::emitter::code::{FunctionBodyBuilder, GlobalReference};
use crate::emitter::linking::{Symbol, SymbolTarget, symbol_flags};
use crate::emitter::{
    DefinedGlobal, ENVIRONMENT_MODULE, ImportedGlobal, STACK_POINTER_IMPORT, ValueType, WasmObject,
};

/// The base of the `%fs` segment: the seventeenth register.
///
/// A register in every way that matters here — an `i64` global, promotion-
/// eligible, saved and restored with the other sixteen — because that is
/// what it is on x86-64. `%fs` has no selector semantics left in long mode;
/// it is a base address that `arch_prctl` writes and that every glibc and
/// musl access to the thread pointer, the stack canary and errno reads.
/// Modelling it as a register rather than as a segmentation feature is what
/// makes an `%fs`-prefixed operand cost one extra add and nothing else cost
/// anything.
///
/// Guest code never writes it: the only writers are the kernel — through
/// `arch_prctl`, a syscall, and through a new thread's control block — and
/// `wrfsbase`, which is not translated. So it is loaded at entry and after
/// calls, and never flushed.
pub const SEGMENT_BASE_NAME: &str = "x86_fs_base";

/// The sixteen general-purpose registers, in x86 encoding order.
pub const REGISTER_NAMES: [&str; 16] = [
    "x86_rax", "x86_rcx", "x86_rdx", "x86_rbx", "x86_rsp", "x86_rbp", "x86_rsi", "x86_rdi",
    "x86_r8", "x86_r9", "x86_r10", "x86_r11", "x86_r12", "x86_r13", "x86_r14", "x86_r15",
];

/// How many XMM registers the machine model carries.
pub const VECTOR_REGISTER_COUNT: usize = 16;

/// The two halves of an XMM register.
///
/// Each is an `i64` global of its own rather than the obvious single `v128`
/// global, because `wasm-ld` cannot link an object that defines one: LLD's
/// object reader has no case for a `v128.const` initializer. The pair turns
/// out to fit SSE's grain — a scalar operation writes the low 64 bits and
/// preserves the high 64, which here is "touch only the low global".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VectorHalf {
    Low,
    High,
}

impl VectorHalf {
    pub const BOTH: [VectorHalf; 2] = [VectorHalf::Low, VectorHalf::High];

    fn suffix(self) -> &'static str {
        match self {
            VectorHalf::Low => "lo",
            VectorHalf::High => "hi",
        }
    }

    pub fn index(self) -> usize {
        match self {
            VectorHalf::Low => 0,
            VectorHalf::High => 1,
        }
    }
}

/// The flags the machine model computes, all of them eagerly.
///
/// Parity is here because floating-point compares report *unordered* through
/// it and compilers branch on `jp` immediately afterwards. Adjust is still
/// absent: it exists only for binary-coded decimal, which compilers do not
/// emit, and an instruction that consumes it is a translation error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flag {
    Zero,
    Sign,
    Carry,
    Overflow,
    Parity,
    /// Which way the string instructions walk. Unlike the other five it is
    /// not a result of anything — it is *set* by `std` and cleared by `cld`,
    /// read by `movs` and `stos`, and by nothing else. A libc's `memmove`
    /// sets it to copy an overlapping range backwards and clears it again
    /// immediately after, which is the only reason it has to be modelled at
    /// all rather than assumed clear.
    Direction,
}

impl Flag {
    pub const ALL: [Flag; 6] = [
        Flag::Zero,
        Flag::Sign,
        Flag::Carry,
        Flag::Overflow,
        Flag::Parity,
        Flag::Direction,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Flag::Direction => "x86_df",
            Flag::Zero => "x86_zf",
            Flag::Sign => "x86_sf",
            Flag::Carry => "x86_cf",
            Flag::Overflow => "x86_of",
            Flag::Parity => "x86_pf",
        }
    }

    fn index(self) -> usize {
        match self {
            Flag::Zero => 0,
            Flag::Sign => 1,
            Flag::Carry => 2,
            Flag::Overflow => 3,
            Flag::Parity => 4,
            Flag::Direction => 5,
        }
    }
}

/// The wasm integer type that carries a general-purpose operand. This is
/// [`ValueType`] narrowed to the two types an integer path can produce, so
/// that those paths stay exhaustive as the object format learns non-integer
/// types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Carrier {
    I32,
    I64,
}

impl Carrier {
    pub fn value_type(self) -> ValueType {
        match self {
            Carrier::I32 => ValueType::I32,
            Carrier::I64 => ValueType::I64,
        }
    }
}

/// The width of an operand, and hence which wasm value type carries it.
/// Everything narrower than eight bytes travels as `i32`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum OperandWidth {
    Byte,
    Word,
    DoubleWord,
    QuadWord,
}

impl OperandWidth {
    pub fn from_bytes(bytes: usize) -> Result<Self> {
        Ok(match bytes {
            1 => OperandWidth::Byte,
            2 => OperandWidth::Word,
            4 => OperandWidth::DoubleWord,
            8 => OperandWidth::QuadWord,
            other => bail!("unsupported operand width of {other} bytes"),
        })
    }

    pub fn bytes(self) -> usize {
        match self {
            OperandWidth::Byte => 1,
            OperandWidth::Word => 2,
            OperandWidth::DoubleWord => 4,
            OperandWidth::QuadWord => 8,
        }
    }

    pub fn bits(self) -> u32 {
        self.bytes() as u32 * 8
    }

    pub fn carrier(self) -> Carrier {
        match self {
            OperandWidth::QuadWord => Carrier::I64,
            _ => Carrier::I32,
        }
    }

    pub fn value_type(self) -> ValueType {
        self.carrier().value_type()
    }

    /// Mask of the significant bits, for widths that share the `i32` carrier.
    pub fn mask_i32(self) -> i32 {
        match self {
            OperandWidth::Byte => 0xff,
            OperandWidth::Word => 0xffff,
            _ => -1,
        }
    }

    pub fn alignment_log2(self) -> u32 {
        match self {
            OperandWidth::Byte => 0,
            OperandWidth::Word => 1,
            OperandWidth::DoubleWord => 2,
            OperandWidth::QuadWord => 3,
        }
    }
}

/// Which part of a 64-bit register an operand names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegisterSlice {
    /// Index into [`REGISTER_NAMES`].
    pub number: usize,
    pub width: OperandWidth,
    /// True for `ah`/`ch`/`dh`/`bh`, which name bits 8..16.
    pub high_byte: bool,
}

impl RegisterSlice {
    pub fn of(register: Register) -> Result<Self> {
        let high_byte = matches!(
            register,
            Register::AH | Register::CH | Register::DH | Register::BH
        );
        let full = register.full_register();
        let number = register_number(full)?;
        Ok(Self {
            number,
            width: OperandWidth::from_bytes(register.size())?,
            high_byte,
        })
    }

    /// The whole of a register named by its index — for the translator's own
    /// bookkeeping accesses, which always work full-width.
    pub fn quad(number: usize) -> Self {
        Self {
            number,
            width: OperandWidth::QuadWord,
            high_byte: false,
        }
    }
}

/// The XMM register a register operand names, if it names one at all.
pub fn vector_register_number(register: Register) -> Option<usize> {
    register.is_xmm().then(|| register.number())
}

fn register_number(full: Register) -> Result<usize> {
    const ORDER: [Register; 16] = [
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
    ORDER
        .iter()
        .position(|candidate| *candidate == full)
        .ok_or_else(|| anyhow::anyhow!("register {full:?} is outside the emulated register file"))
}

/// Index of `rsp` in [`REGISTER_NAMES`].
pub const STACK_POINTER_REGISTER: usize = 4;

/// The value a host-entry wrapper leaves where a return address would be, so
/// guest code that inspects the slot sees something recognisable rather than
/// whatever the stack happened to hold.
pub const RETURN_ADDRESS_SENTINEL: i64 = 0x7a61_7161_7275_0000u64 as i64;

/// The wasm globals holding the emulated machine state, plus the linker's
/// stack pointer that wrappers start the guest stack from.
pub struct MachineState {
    registers: [GlobalReference; 16],
    /// The low and high halves of each XMM register, in that order.
    vector_registers: [[GlobalReference; 2]; VECTOR_REGISTER_COUNT],
    flags: [GlobalReference; Flag::ALL.len()],
    segment_base: GlobalReference,
    pub linker_stack_pointer: GlobalReference,
}

impl MachineState {
    /// Defines the machine state in an object: every register and flag as a
    /// weakly-bound mutable global, plus the imported `__stack_pointer`.
    pub fn define(object: &mut WasmObject) -> Self {
        // The imported stack pointer must come first: import indices precede
        // definition indices in the global index space.
        let stack_pointer_index = object.next_defined_global_index();
        object.imported_globals.push(ImportedGlobal {
            module: ENVIRONMENT_MODULE.to_string(),
            field: STACK_POINTER_IMPORT.to_string(),
            value_type: ValueType::I32,
            mutable: true,
        });
        let stack_pointer_symbol = object.add_symbol(Symbol {
            name: STACK_POINTER_IMPORT.to_string(),
            target: SymbolTarget::Global(stack_pointer_index),
            flags: symbol_flags::UNDEFINED,
        });
        let linker_stack_pointer = GlobalReference {
            symbol_index: stack_pointer_symbol,
            global_index: stack_pointer_index,
        };

        let define_state_global = |object: &mut WasmObject, name: &str, value_type| {
            let global_index = object.next_defined_global_index();
            object.defined_globals.push(DefinedGlobal {
                value_type,
                mutable: true,
                initial_value: 0,
            });
            let symbol_index = object.add_symbol(Symbol {
                name: name.to_string(),
                target: SymbolTarget::Global(global_index),
                // Weak so that separately transpiled objects collapse onto one
                // register file; visible so that collapsing can happen at all.
                flags: symbol_flags::WEAK,
            });
            GlobalReference {
                symbol_index,
                global_index,
            }
        };

        let registers = std::array::from_fn(|index| {
            define_state_global(object, REGISTER_NAMES[index], ValueType::I64)
        });
        let vector_registers = std::array::from_fn(|number| {
            [VectorHalf::Low, VectorHalf::High].map(|half| {
                let name = format!("x86_xmm{number}_{}", half.suffix());
                define_state_global(object, &name, ValueType::I64)
            })
        });
        let flags = std::array::from_fn(|index| {
            define_state_global(object, Flag::ALL[index].name(), ValueType::I32)
        });
        // Last, so that adding it leaves every other global's index where it
        // was — the indices are what a reader of an emitted object matches
        // against the register names.
        let segment_base = define_state_global(object, SEGMENT_BASE_NAME, ValueType::I64);

        Self {
            registers,
            vector_registers,
            flags,
            segment_base,
            linker_stack_pointer,
        }
    }

    pub fn register(&self, number: usize) -> GlobalReference {
        self.registers[number]
    }

    /// One half of an XMM register.
    pub fn vector_register(&self, number: usize, half: VectorHalf) -> GlobalReference {
        self.vector_registers[number][half.index()]
    }

    pub fn flag(&self, flag: Flag) -> GlobalReference {
        self.flags[flag.index()]
    }

    pub fn segment_base(&self) -> GlobalReference {
        self.segment_base
    }
}

/// Where one piece of machine state lives, from the point of view of the
/// function body being built: in its weak global — the convention every
/// seam speaks — or in a local of that body, once promotion has moved it
/// there.
#[derive(Clone, Copy)]
enum Storage {
    Global(GlobalReference),
    Local(u32),
}

impl Storage {
    fn get(self, body: &mut FunctionBodyBuilder) {
        match self {
            Storage::Global(global) => body.global_get(global),
            Storage::Local(local) => body.local_get(local),
        }
    }

    fn set(self, body: &mut FunctionBodyBuilder) {
        match self {
            Storage::Global(global) => body.global_set(global),
            Storage::Local(local) => body.local_set(local),
        }
    }
}

/// The per-function promotion decision: which cells live in locals of the
/// body being built, and which of those the body may write and therefore
/// owes back to the globals at escape points.
struct Promotion {
    registers: [Option<u32>; 16],
    /// The low and high halves of each XMM register, in that order.
    vector_halves: [[Option<u32>; 2]; VECTOR_REGISTER_COUNT],
    /// Flags are always promoted, so their locals are unconditional.
    flags: [u32; Flag::ALL.len()],
    /// Present when the function has an `%fs`-prefixed operand at all. Never
    /// in the written set: guest code cannot write the segment base.
    segment_base: Option<u32>,
    written: LocationSet,
}

/// One function's view of the machine state.
///
/// Every read and write a translated body makes goes through here, so this
/// is the one place that decides whether a cell lives in its global or in a
/// local. Without a promotion map every cell resolves to its global, which
/// is the unpromoted configuration — not a separate code path.
pub struct FunctionState<'a> {
    machine: &'a MachineState,
    promotion: Option<Promotion>,
}

impl<'a> FunctionState<'a> {
    pub fn new(machine: &'a MachineState) -> Self {
        Self {
            machine,
            promotion: None,
        }
    }

    fn register_storage(&self, number: usize) -> Storage {
        self.promotion
            .as_ref()
            .and_then(|promotion| promotion.registers[number])
            .map(Storage::Local)
            .unwrap_or(Storage::Global(self.machine.registers[number]))
    }

    fn vector_storage(&self, number: usize, half: VectorHalf) -> Storage {
        self.promotion
            .as_ref()
            .and_then(|promotion| promotion.vector_halves[number][half.index()])
            .map(Storage::Local)
            .unwrap_or(Storage::Global(self.machine.vector_register(number, half)))
    }

    fn flag_storage(&self, flag: Flag) -> Storage {
        match &self.promotion {
            Some(promotion) => Storage::Local(promotion.flags[flag.index()]),
            None => Storage::Global(self.machine.flag(flag)),
        }
    }

    /// Moves every touched cell into a local of the body being built:
    /// declares the locals, emits the entry copies, and records what the
    /// escape points must flush.
    ///
    /// Flags are always promoted, and their entry copy is not a nicety: a
    /// function entered by a tail jump — the compiler's cold-path split —
    /// may read flags its jumper set, which the jumper flushed on the way
    /// out.
    pub fn promote(
        &mut self,
        body: &mut FunctionBodyBuilder,
        touched: LocationSet,
        written: LocationSet,
        segment_base: bool,
    ) {
        let mut registers = [None; 16];
        let mut vector_halves = [[None; 2]; VECTOR_REGISTER_COUNT];
        for location in touched.iter() {
            match location {
                Location::Integer(number) => {
                    let local = body.declare_local(ValueType::I64);
                    body.global_get(self.machine.registers[number]);
                    body.local_set(local);
                    registers[number] = Some(local);
                }
                Location::Float(number) => {
                    for half in VectorHalf::BOTH {
                        let local = body.declare_local(ValueType::I64);
                        body.global_get(self.machine.vector_register(number, half));
                        body.local_set(local);
                        vector_halves[number][half.index()] = Some(local);
                    }
                }
            }
        }
        let flags = Flag::ALL.map(|flag| {
            let local = body.declare_local(ValueType::I32);
            body.global_get(self.machine.flag(flag));
            body.local_set(local);
            local
        });
        let segment_base = segment_base.then(|| {
            let local = body.declare_local(ValueType::I64);
            body.global_get(self.machine.segment_base);
            body.local_set(local);
            local
        });
        self.promotion = Some(Promotion {
            registers,
            vector_halves,
            flags,
            segment_base,
            written,
        });
    }

    /// Publishes every promoted cell the body may have written back to its
    /// global — before anything that can observe the globals, which is a
    /// call or leaving the function. Flags are exempt on the ABI's
    /// authority: they are call-clobbered, and keeping them out of the
    /// flush is what lets the engine delete the flag computations nothing
    /// reads.
    pub fn flush_written(&self, body: &mut FunctionBodyBuilder) {
        let Some(promotion) = &self.promotion else {
            return;
        };
        for location in promotion.written.iter() {
            match location {
                Location::Integer(number) => {
                    if let Some(local) = promotion.registers[number] {
                        body.local_get(local);
                        body.global_set(self.machine.registers[number]);
                    }
                }
                Location::Float(number) => {
                    for half in VectorHalf::BOTH {
                        if let Some(local) = promotion.vector_halves[number][half.index()] {
                            body.local_get(local);
                            body.global_set(self.machine.vector_register(number, half));
                        }
                    }
                }
            }
        }
    }

    /// Refreshes every promoted register and XMM half from its global,
    /// after a call: the callee may have rewritten any of them, and the
    /// callee-saved ones came back through the globals too, restored by the
    /// callee's own push/pop emulation. Flags are exempt for the same
    /// reason they are exempt from the flush: nothing conforming reads a
    /// flag it did not set since the call.
    pub fn reload(&self, body: &mut FunctionBodyBuilder) {
        let Some(promotion) = &self.promotion else {
            return;
        };
        // The segment base is reloaded for a reason the other cells do not
        // have: `arch_prctl` is a syscall, and a syscall is a call, so the
        // callee is exactly what changes it.
        if let Some(local) = promotion.segment_base {
            body.global_get(self.machine.segment_base);
            body.local_set(local);
        }
        for (number, local) in promotion.registers.iter().enumerate() {
            if let Some(local) = *local {
                body.global_get(self.machine.registers[number]);
                body.local_set(local);
            }
        }
        for (number, halves) in promotion.vector_halves.iter().enumerate() {
            for half in VectorHalf::BOTH {
                if let Some(local) = halves[half.index()] {
                    body.global_get(self.machine.vector_register(number, half));
                    body.local_set(local);
                }
            }
        }
    }

    /// Publishes the flags. Only a tail jump needs this: it is the one exit
    /// whose target may legitimately read what this function's last compare
    /// left behind, because the compiler splits single functions across
    /// section boundaries and jumps between the halves.
    pub fn flush_flags(&self, body: &mut FunctionBodyBuilder) {
        let Some(promotion) = &self.promotion else {
            return;
        };
        for flag in Flag::ALL {
            body.local_get(promotion.flags[flag.index()]);
            body.global_set(self.machine.flag(flag));
        }
    }

    /// Pushes the value of a register slice: `i32` for widths below eight
    /// bytes, `i64` for the full register.
    pub fn read_register(&self, body: &mut FunctionBodyBuilder, slice: RegisterSlice) {
        self.register_storage(slice.number).get(body);
        if slice.high_byte {
            body.i64_const(8);
            body.i64_shr_unsigned();
        }
        match slice.width {
            OperandWidth::QuadWord => {}
            width => {
                body.i32_wrap_i64();
                if width != OperandWidth::DoubleWord {
                    body.i32_const(width.mask_i32());
                    body.i32_and();
                }
            }
        }
    }

    /// Stores the value on top of the stack into a register slice, following
    /// x86's write semantics: a 32-bit write zeroes the upper half, while
    /// narrower writes leave the rest of the register alone.
    pub fn write_register(&self, body: &mut FunctionBodyBuilder, slice: RegisterSlice) {
        let storage = self.register_storage(slice.number);
        match (slice.width, slice.high_byte) {
            (OperandWidth::QuadWord, _) => {
                storage.set(body);
            }
            (OperandWidth::DoubleWord, _) => {
                body.i64_extend_i32_unsigned();
                storage.set(body);
            }
            (width, high_byte) => {
                let shift = if high_byte { 8 } else { 0 };
                let value_mask = u64::from(width.mask_i32() as u32);
                let field_mask = value_mask << shift;
                body.i64_extend_i32_unsigned();
                body.i64_const(value_mask as i64);
                body.i64_and();
                if shift != 0 {
                    body.i64_const(shift);
                    body.i64_shl();
                }
                storage.get(body);
                body.i64_const(!field_mask as i64);
                body.i64_and();
                body.i64_or();
                storage.set(body);
            }
        }
    }

    /// Pushes one half of an XMM register as an `i64`.
    pub fn read_vector(&self, body: &mut FunctionBodyBuilder, number: usize, half: VectorHalf) {
        self.vector_storage(number, half).get(body);
    }

    /// Stores the `i64` on top of the stack into one half of an XMM register.
    pub fn write_vector(&self, body: &mut FunctionBodyBuilder, number: usize, half: VectorHalf) {
        self.vector_storage(number, half).set(body);
    }

    /// Pushes the `%fs` base, which an `%fs`-prefixed operand adds into its
    /// effective address.
    pub fn read_segment_base(&self, body: &mut FunctionBodyBuilder) {
        match self.promotion.as_ref().and_then(|p| p.segment_base) {
            Some(local) => body.local_get(local),
            None => body.global_get(self.machine.segment_base),
        }
    }

    /// Pushes a flag as an `i32`, `1` or `0`.
    pub fn read_flag(&self, body: &mut FunctionBodyBuilder, flag: Flag) {
        self.flag_storage(flag).get(body);
    }

    /// Stores the `i32` on top of the stack into a flag.
    pub fn write_flag(&self, body: &mut FunctionBodyBuilder, flag: Flag) {
        self.flag_storage(flag).set(body);
    }
}
