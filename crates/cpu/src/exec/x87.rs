//! The x87, which is the one subsystem that ports as a whole.
//!
//! The `x87` crate is already an interpreter for its domain, oracle and
//! all: extended-precision softfloat, the register stack with its fault
//! model, the status and control words, and a hardware-FPU differential
//! that found seven undocumented behaviours the first time it ran. Under
//! the transpiler it is reached through generated wasm calls that have to
//! save the linker's stack pointer, dodge the guest's red zone, and marshal
//! every operand through the emitter. Here it is a method call on a field.
//!
//! So this module is decode, not semantics. What lives here is the mapping
//! from an x86 encoding to a call — which `ST(i)` an operand names, which
//! width a memory operand is, which forms pop.
//!
//! One thing genuinely changes, and it is the change the thread design
//! asked for: **the state is per thread**. The crate's wasm arrangement is
//! a single static, which was correct under one interpreter thread of
//! execution; here it is a field of the [`crate::state::Tcb`], so a context
//! switch carries it with everything else and the cross-thread corruption
//! the thread design was written to avoid cannot arise.

use iced_x86::{Instruction, MemorySize, Mnemonic, OpKind, Register};
use x87::compare::NanPolicy;
use x87::ops::Binary;

use crate::flags::{Condition, bit};
use crate::state::Width;

use super::{Cpu, Step, Trap, Unsupported};

/// The size of an `fxsave` area.
const FX_AREA: usize = 512;

/// Where the vector registers start in it.
const VECTOR_AREA: usize = 160;

/// How many of them there are in a 64-bit area.
const VECTORS: usize = 16;

/// Which `MXCSR` bits this machine implements.
///
/// A fact about the *processor*, like anything `cpuid` answers, and settled
/// the same way: this container reports one machine on every host — a
/// baseline x86-64 with SSE2 and nothing later — so the mask is that
/// processor's rather than whatever die the container happens to be running
/// on. `0xffbf` is what a processor of that generation reports: every
/// `MXCSR` bit except the flush-to-zero control it does not have. Measured
/// 2026-08-30, the machine under this suite answers `0x0002ffff`, which is
/// a later processor's answer and deliberately not ours.
const MXCSR_MASK: u32 = 0x0000_ffbf;

/// The `ST(i)` an operand names, if it names one.
fn stack_operand(instruction: &Instruction, index: u32) -> Option<u32> {
    if index >= instruction.op_count() || instruction.op_kind(index) != OpKind::Register {
        return None;
    }
    let register = instruction.op_register(index);
    (Register::ST0..=Register::ST7)
        .contains(&register)
        .then(|| register.number() as u32)
}

/// The `ST(i)` the instruction's *last* operand names.
///
/// Which operand carries the index is not constant, and the difference is
/// easy to get backwards: `ffree st(2)`, `fld st(2)` and `fstp st(2)` have
/// one operand and it is the index, while `fxch st(1)`, `fcom st(2)` and
/// `fcmovb st, st(2)` have two of which the first is always `ST0`. The last
/// operand is the index in every one of them. `fcompp` and `fucompp` have
/// none at all and mean `st(1)`, which is what the default supplies.
fn last_stack_operand(instruction: &Instruction, default: u32) -> u32 {
    match instruction.op_count() {
        0 => default,
        count => stack_operand(instruction, count - 1).unwrap_or(default),
    }
}

/// Whether an instruction's memory operand is a register instead — the
/// register forms of the load, store, arithmetic and compare families.
fn is_register_form(instruction: &Instruction) -> bool {
    instruction.op_count() == 0 || stack_operand(instruction, 0).is_some()
}

impl Cpu<'_> {
    /// Executes an x87 instruction, or reports that it is not one.
    pub(super) fn x87_step(&mut self, instruction: &Instruction) -> Result<Option<Step>, Trap> {
        let step = match instruction.mnemonic() {
            // ---- loads ----
            Mnemonic::Fld => {
                if let Some(index) = stack_operand(instruction, 0) {
                    self.tcb.x87.fld_sti(index);
                    return Ok(Some(Step::Retired));
                }
                match instruction.memory_size() {
                    MemorySize::Float32 => {
                        let bits = self.read(instruction, 0, Width::Dword)? as u32;
                        self.tcb.x87.fld_m32(bits);
                    }
                    MemorySize::Float64 => {
                        let bits = self.read(instruction, 0, Width::Qword)?;
                        self.tcb.x87.fld_m64(bits);
                    }
                    // Ten bytes are not a width the register file has, so
                    // they travel as bytes.
                    MemorySize::Float80 => {
                        let bytes = self.read_extended(instruction)?;
                        self.tcb.x87.fld_m80(bytes);
                    }
                    other => return Err(self.unsupported_size(instruction, other)),
                }
                Step::Retired
            }
            // The seven constants, in the order the opcodes number them:
            // 1, log₂10, log₂e, π, log₁₀2, ln 2, 0.
            Mnemonic::Fld1
            | Mnemonic::Fldl2t
            | Mnemonic::Fldl2e
            | Mnemonic::Fldpi
            | Mnemonic::Fldlg2
            | Mnemonic::Fldln2
            | Mnemonic::Fldz => {
                let index = match instruction.mnemonic() {
                    Mnemonic::Fld1 => 0,
                    Mnemonic::Fldl2t => 1,
                    Mnemonic::Fldl2e => 2,
                    Mnemonic::Fldpi => 3,
                    Mnemonic::Fldlg2 => 4,
                    Mnemonic::Fldln2 => 5,
                    _ => 6,
                };
                self.tcb.x87.fld_constant(index);
                Step::Retired
            }
            Mnemonic::Fild => {
                let value = self.read_integer_operand(instruction)?;
                self.tcb.x87.fild(value);
                Step::Retired
            }

            // ---- stores ----
            Mnemonic::Fst | Mnemonic::Fstp => {
                let pop = instruction.mnemonic() == Mnemonic::Fstp;
                if let Some(index) = stack_operand(instruction, 0) {
                    self.tcb.x87.fst_sti(index, pop);
                    return Ok(Some(Step::Retired));
                }
                match instruction.memory_size() {
                    MemorySize::Float32 => {
                        let bits = self.tcb.x87.fst_m32(pop);
                        self.write(instruction, 0, Width::Dword, u64::from(bits))?
                    }
                    MemorySize::Float64 => {
                        let bits = self.tcb.x87.fst_m64(pop);
                        self.write(instruction, 0, Width::Qword, bits)?
                    }
                    MemorySize::Float80 => {
                        let bytes = self.tcb.x87.fstp_m80();
                        let at = self.address(instruction)?;
                        self.space.write(at, &bytes)?;
                        Step::Retired
                    }
                    other => return Err(self.unsupported_size(instruction, other)),
                }
            }
            Mnemonic::Fist | Mnemonic::Fistp | Mnemonic::Fisttp => {
                let truncating = instruction.mnemonic() == Mnemonic::Fisttp;
                let pop = truncating || instruction.mnemonic() == Mnemonic::Fistp;
                let width = match instruction.memory_size() {
                    MemorySize::Int16 => Width::Word,
                    MemorySize::Int32 => Width::Dword,
                    MemorySize::Int64 => Width::Qword,
                    other => return Err(self.unsupported_size(instruction, other)),
                };
                let bits = width.bits();
                let value = match truncating {
                    true => self.tcb.x87.fisttp(bits),
                    false => self.tcb.x87.fist(bits, pop),
                };
                self.write(instruction, 0, width, value as u64)?
            }

            // ---- arithmetic ----
            Mnemonic::Fadd
            | Mnemonic::Faddp
            | Mnemonic::Fsub
            | Mnemonic::Fsubp
            | Mnemonic::Fsubr
            | Mnemonic::Fsubrp
            | Mnemonic::Fmul
            | Mnemonic::Fmulp
            | Mnemonic::Fdiv
            | Mnemonic::Fdivp
            | Mnemonic::Fdivr
            | Mnemonic::Fdivrp => {
                let operation = match instruction.mnemonic() {
                    Mnemonic::Fadd | Mnemonic::Faddp => Binary::Add,
                    Mnemonic::Fsub | Mnemonic::Fsubp => Binary::Sub,
                    Mnemonic::Fsubr | Mnemonic::Fsubrp => Binary::SubReverse,
                    Mnemonic::Fmul | Mnemonic::Fmulp => Binary::Mul,
                    Mnemonic::Fdiv | Mnemonic::Fdivp => Binary::Div,
                    _ => Binary::DivReverse,
                };
                if is_register_form(instruction) {
                    let popping = matches!(
                        instruction.mnemonic(),
                        Mnemonic::Faddp
                            | Mnemonic::Fsubp
                            | Mnemonic::Fsubrp
                            | Mnemonic::Fmulp
                            | Mnemonic::Fdivp
                            | Mnemonic::Fdivrp
                    );
                    // With no operands at all the instruction means
                    // `st(1), st(0)` and pops, which is the bare `faddp`
                    // spelling.
                    let (destination, source) = match instruction.op_count() {
                        0 => (1, 0),
                        _ => (
                            stack_operand(instruction, 0).unwrap_or(0),
                            stack_operand(instruction, 1).unwrap_or(0),
                        ),
                    };
                    self.tcb
                        .x87
                        .binary_sti(operation, destination, source, popping);
                    return Ok(Some(Step::Retired));
                }
                match instruction.memory_size() {
                    MemorySize::Float32 => {
                        let bits = self.read(instruction, 0, Width::Dword)? as u32;
                        self.tcb.x87.binary_m32(operation, bits);
                    }
                    MemorySize::Float64 => {
                        let bits = self.read(instruction, 0, Width::Qword)?;
                        self.tcb.x87.binary_m64(operation, bits);
                    }
                    other => return Err(self.unsupported_size(instruction, other)),
                }
                Step::Retired
            }
            Mnemonic::Fiadd
            | Mnemonic::Fisub
            | Mnemonic::Fisubr
            | Mnemonic::Fimul
            | Mnemonic::Fidiv
            | Mnemonic::Fidivr => {
                let operation = match instruction.mnemonic() {
                    Mnemonic::Fiadd => Binary::Add,
                    Mnemonic::Fisub => Binary::Sub,
                    Mnemonic::Fisubr => Binary::SubReverse,
                    Mnemonic::Fimul => Binary::Mul,
                    Mnemonic::Fidiv => Binary::Div,
                    _ => Binary::DivReverse,
                };
                let value = self.read_integer_operand(instruction)?;
                self.tcb.x87.binary_int(operation, value);
                Step::Retired
            }

            // ---- the operand-free family ----
            Mnemonic::Fchs => {
                self.tcb.x87.fchs();
                Step::Retired
            }
            Mnemonic::Fabs => {
                self.tcb.x87.fabs();
                Step::Retired
            }
            Mnemonic::Fsqrt => {
                self.tcb.x87.fsqrt();
                Step::Retired
            }
            Mnemonic::Frndint => {
                self.tcb.x87.frndint();
                Step::Retired
            }
            Mnemonic::Fprem => {
                self.tcb.x87.fprem(false);
                Step::Retired
            }
            Mnemonic::Fprem1 => {
                self.tcb.x87.fprem(true);
                Step::Retired
            }
            Mnemonic::Fscale => {
                self.tcb.x87.fscale();
                Step::Retired
            }
            Mnemonic::Fxtract => {
                self.tcb.x87.fxtract();
                Step::Retired
            }
            Mnemonic::F2xm1 => {
                self.tcb.x87.f2xm1();
                Step::Retired
            }
            Mnemonic::Fyl2x => {
                self.tcb.x87.fyl2x();
                Step::Retired
            }
            Mnemonic::Fyl2xp1 => {
                self.tcb.x87.fyl2xp1();
                Step::Retired
            }
            Mnemonic::Fpatan => {
                self.tcb.x87.fpatan();
                Step::Retired
            }
            Mnemonic::Fincstp => {
                self.tcb.x87.fincstp();
                Step::Retired
            }
            Mnemonic::Fdecstp => {
                self.tcb.x87.fdecstp();
                Step::Retired
            }
            Mnemonic::Ftst => {
                self.tcb.x87.ftst();
                Step::Retired
            }
            Mnemonic::Fxam => {
                self.tcb.x87.fxam();
                Step::Retired
            }
            Mnemonic::Fnclex => {
                self.tcb.x87.clear_exceptions();
                Step::Retired
            }
            Mnemonic::Fninit => {
                // Not `reset`: `fninit` empties the stack, it does not
                // erase it. Erasing belongs to `execve`.
                self.tcb.x87.reinitialize();
                Step::Retired
            }
            // Everything behaves as-if-masked, so there is never a pending
            // unmasked exception for `fwait` to deliver.
            Mnemonic::Wait | Mnemonic::Fnop => Step::Retired,
            Mnemonic::Fxch => {
                let index = last_stack_operand(instruction, 1);
                self.tcb.x87.fxch(index);
                Step::Retired
            }
            Mnemonic::Ffree | Mnemonic::Ffreep => {
                let index = last_stack_operand(instruction, 0);
                self.tcb
                    .x87
                    .ffree(index, instruction.mnemonic() == Mnemonic::Ffreep);
                Step::Retired
            }

            // ---- conditional move, on the integer flags ----
            Mnemonic::Fcmovb
            | Mnemonic::Fcmove
            | Mnemonic::Fcmovbe
            | Mnemonic::Fcmovu
            | Mnemonic::Fcmovnb
            | Mnemonic::Fcmovne
            | Mnemonic::Fcmovnbe
            | Mnemonic::Fcmovnu => {
                let condition = match instruction.mnemonic() {
                    Mnemonic::Fcmovb => Condition::Below,
                    Mnemonic::Fcmove => Condition::Equal,
                    Mnemonic::Fcmovbe => Condition::BelowOrEqual,
                    Mnemonic::Fcmovu => Condition::Parity,
                    Mnemonic::Fcmovnb => Condition::AboveOrEqual,
                    Mnemonic::Fcmovne => Condition::NotEqual,
                    Mnemonic::Fcmovnbe => Condition::Above,
                    _ => Condition::NoParity,
                };
                let index = last_stack_operand(instruction, 0);
                let take = condition.holds(&self.tcb.flags);
                self.tcb.x87.fcmov(index, take);
                Step::Retired
            }

            // ---- comparison, into the status word ----
            Mnemonic::Fcom
            | Mnemonic::Fcomp
            | Mnemonic::Fcompp
            | Mnemonic::Fucom
            | Mnemonic::Fucomp
            | Mnemonic::Fucompp => {
                let policy = match instruction.mnemonic() {
                    Mnemonic::Fucom | Mnemonic::Fucomp | Mnemonic::Fucompp => NanPolicy::Quiet,
                    _ => NanPolicy::Signalling,
                };
                let pops = match instruction.mnemonic() {
                    Mnemonic::Fcompp | Mnemonic::Fucompp => 2,
                    Mnemonic::Fcomp | Mnemonic::Fucomp => 1,
                    _ => 0,
                };
                if is_register_form(instruction) {
                    let index = last_stack_operand(instruction, 1);
                    self.tcb.x87.fcom_sti(index, policy, pops);
                    return Ok(Some(Step::Retired));
                }
                match instruction.memory_size() {
                    MemorySize::Float32 => {
                        let bits = self.read(instruction, 0, Width::Dword)? as u32;
                        self.tcb.x87.fcom_m32(bits, pops);
                    }
                    MemorySize::Float64 => {
                        let bits = self.read(instruction, 0, Width::Qword)?;
                        self.tcb.x87.fcom_m64(bits, pops);
                    }
                    other => return Err(self.unsupported_size(instruction, other)),
                }
                Step::Retired
            }
            Mnemonic::Ficom | Mnemonic::Ficomp => {
                let value = self.read_integer_operand(instruction)?;
                let pops = u32::from(instruction.mnemonic() == Mnemonic::Ficomp);
                self.tcb.x87.ficom(value, pops);
                Step::Retired
            }

            // ---- comparison, into the integer flags ----
            Mnemonic::Fcomi | Mnemonic::Fcomip | Mnemonic::Fucomi | Mnemonic::Fucomip => {
                let policy = match instruction.mnemonic() {
                    Mnemonic::Fucomi | Mnemonic::Fucomip => NanPolicy::Quiet,
                    _ => NanPolicy::Signalling,
                };
                let pop = matches!(
                    instruction.mnemonic(),
                    Mnemonic::Fcomip | Mnemonic::Fucomip
                );
                let index = last_stack_operand(instruction, 1);
                // The crate answers in the layout the hardware uses: carry
                // at bit zero, parity at bit two, zero at bit six. Sign,
                // overflow and adjust are cleared, which is what the
                // instruction defines.
                let packed = u64::from(self.tcb.x87.fcomi(index, policy, pop));
                self.tcb
                    .flags
                    .set_all(packed & (bit::CARRY | bit::PARITY | bit::ZERO));
                Step::Retired
            }

            // ---- the words ----
            Mnemonic::Fnstsw | Mnemonic::Fnstcw => {
                let value = match instruction.mnemonic() {
                    Mnemonic::Fnstsw => self.tcb.x87.status_word(),
                    _ => self.tcb.x87.control(),
                };
                // `fnstsw %ax` writes the word and leaves the rest of
                // `%rax` alone, which a wider write would not.
                self.write(instruction, 0, Width::Word, u64::from(value))?
            }
            Mnemonic::Fldcw => {
                let value = self.read(instruction, 0, Width::Word)? as u16;
                self.tcb.x87.set_control(value);
                Step::Retired
            }
            Mnemonic::Fnstenv | Mnemonic::Fldenv => {
                let at = self.address(instruction)?;
                let mut image = [0u8; x87::state::ENVIRONMENT_SIZE];
                match instruction.mnemonic() {
                    Mnemonic::Fnstenv => {
                        self.tcb.x87.store_environment(&mut image);
                        self.space.write(at, &image)?;
                    }
                    _ => {
                        self.space.read(at, &mut image)?;
                        self.tcb.x87.load_environment(&image);
                    }
                }
                Step::Retired
            }
            // `fxsave`/`fxrstor`: the whole unit *and* the vector file, in
            // one 512-byte area.
            //
            // Reached by every dynamically linked program that binds lazily:
            // `_dl_runtime_resolve` saves the vector registers around a
            // symbol lookup, because the resolver is ordinary C and would
            // otherwise clobber arguments passing through them. Every byte
            // this writes is a
            // field of the control block.
            //
            // The area is split between two owners and assembled here: the
            // `x87` crate owns bytes 0..160, which are its layout, and the
            // vector half is this module's.
            Mnemonic::Fxsave | Mnemonic::Fxsave64 => {
                let at = self.address(instruction)?;
                let mut image = [0u8; FX_AREA];
                let mut unit = [0u8; x87::state::FX_FPU_SIZE];
                self.tcb.x87.store_fx(&mut unit);
                image[..x87::state::FX_FPU_SIZE].copy_from_slice(&unit);
                image[24..28].copy_from_slice(&self.tcb.mxcsr.to_le_bytes());
                // The mask says which `MXCSR` bits this processor
                // implements. A zero here means "assume the default", which
                // is what a guest that reads it does with it; naming the
                // default outright is what real hardware does.
                image[28..32].copy_from_slice(&MXCSR_MASK.to_le_bytes());
                for number in 0..VECTORS {
                    let offset = VECTOR_AREA + number * 16;
                    let value = self.vector(number);
                    image[offset..offset + 16].copy_from_slice(&value.to_le_bytes());
                }
                self.space.write(at, &image)?;
                Step::Retired
            }
            Mnemonic::Fxrstor | Mnemonic::Fxrstor64 => {
                let at = self.address(instruction)?;
                let mut image = [0u8; FX_AREA];
                self.space.read(at, &mut image)?;
                let unit: [u8; x87::state::FX_FPU_SIZE] = image[..x87::state::FX_FPU_SIZE]
                    .try_into()
                    .expect("the unit's half of the area");
                self.tcb.x87.load_fx(&unit);
                self.tcb.mxcsr = u32::from_le_bytes(
                    image[24..28].try_into().expect("four bytes of MXCSR"),
                );
                for number in 0..VECTORS {
                    let offset = VECTOR_AREA + number * 16;
                    let value = u128::from_le_bytes(
                        image[offset..offset + 16]
                            .try_into()
                            .expect("sixteen bytes of a vector register"),
                    );
                    self.set_vector(number, value);
                }
                Step::Retired
            }
            Mnemonic::Fnsave | Mnemonic::Frstor => {
                let at = self.address(instruction)?;
                let mut image = [0u8; x87::state::SAVE_SIZE];
                match instruction.mnemonic() {
                    Mnemonic::Fnsave => {
                        self.tcb.x87.store_and_reinitialize(&mut image);
                        self.space.write(at, &image)?;
                    }
                    _ => {
                        self.space.read(at, &mut image)?;
                        self.tcb.x87.load_saved(&image);
                    }
                }
                Step::Retired
            }

            _ => return Ok(None),
        };
        Ok(Some(step))
    }

    /// The ten bytes of an extended-precision operand.
    fn read_extended(&mut self, instruction: &Instruction) -> Result<[u8; 10], Trap> {
        let at = self.address(instruction)?;
        let mut bytes = [0u8; 10];
        self.space.read(at, &mut bytes)?;
        Ok(bytes)
    }

    /// A `fild`/`fiadd`/`ficom` integer operand, sign-extended.
    fn read_integer_operand(&mut self, instruction: &Instruction) -> Result<i64, Trap> {
        let width = match instruction.memory_size() {
            MemorySize::Int16 => Width::Word,
            MemorySize::Int32 => Width::Dword,
            MemorySize::Int64 => Width::Qword,
            other => return Err(self.unsupported_size(instruction, other)),
        };
        let raw = self.read(instruction, 0, width)?;
        Ok(width.sign_extend(raw) as i64)
    }

    fn unsupported_size(&self, instruction: &Instruction, size: MemorySize) -> Trap {
        let _ = size;
        Trap::Unsupported(Unsupported::at(
            instruction,
            Some("an x87 memory operand of a width this form does not take"),
        ))
    }
}
