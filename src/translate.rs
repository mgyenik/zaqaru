//! Translating x86-64 instructions into WebAssembly against the emulated
//! machine model.
//!
//! Two invariants hold throughout:
//!
//! * **Narrow values are zero-extended.** Any value of width 1, 2 or 4 bytes
//!   travels in an `i32` with the bits above its width cleared. Operations
//!   that can overflow that width re-mask their result.
//! * **Nothing is silently skipped.** An instruction, operand form or flag
//!   the translator does not model is an error naming the instruction.

mod vector;

use anyhow::{Context, Result, bail};
use iced_x86::{
    ConditionCode, FlowControl, Formatter, Instruction, IntelFormatter, Mnemonic, OpKind, Register,
};

use crate::abi::effects::{LocationSet, effects_of};
use crate::emitter::ValueType;
use crate::emitter::code::{DataReference, FunctionBodyBuilder, FunctionReference, TableReference};
use crate::lifter::{LiftedFunction, LiftedInstruction, SymbolReference};
use crate::machine::{
    Carrier, Flag, FunctionState, MachineState, OperandWidth, RegisterSlice, STACK_POINTER_REGISTER,
};

/// What a symbolic operand denotes once translated.
///
/// The two are different kinds of number in wasm, which is the whole of the
/// address-space split: a data address indexes linear memory, a function
/// pointer indexes the table of functions.
#[derive(Clone, Copy, Debug)]
pub enum SymbolValue {
    Address(DataReference),
    FunctionPointer(TableReference),
}

/// How the translator reaches the wasm symbols an instruction may name.
pub trait SymbolResolver {
    /// The guest function a transfer names.
    ///
    /// The reference may be a section symbol plus an offset rather than the
    /// function's own symbol — which is what the assembler leaves behind for
    /// a local callee, and for a cold path split into a section of its own.
    fn function(&self, elf_symbol: usize, addend: i64) -> Result<FunctionReference>;
    /// The guest function that begins at a section offset.
    ///
    /// The assembler resolves calls to functions defined in the same section
    /// itself, leaving no relocation behind, so a call target can be a bare
    /// offset rather than a symbol.
    fn function_at(&self, section: usize, offset: u64) -> Result<FunctionReference>;
    /// What a symbolic operand's value is: an address in linear memory, or a
    /// slot in the indirect function table.
    fn value(&self, elf_symbol: usize, addend: i64) -> Result<SymbolValue>;
    /// The address of a recovered jump table, whose entries the transpiler
    /// rewrote to hold their own index.
    fn jump_table_address(&self, section: usize, offset: u64) -> Result<DataReference>;
    /// The table slot of the function beginning at a section offset.
    ///
    /// As with calls, the assembler resolves a reference to a function in the
    /// same section itself, so taking its address leaves no relocation to
    /// read — only a displacement from the program counter.
    fn table_slot_at(&self, section: usize, offset: u64) -> Result<TableReference>;
    /// Whether the input is a linked executable rather than a relocatable
    /// object.
    ///
    /// The two answer the same question from different evidence. A
    /// relocatable object has no addresses, so every reference is a
    /// relocation naming a symbol; a linked executable has already been
    /// placed, so a reference *is* the address it means. Nothing has to be
    /// symbolised in linked mode — the loader puts each segment at its own
    /// virtual address, and linear memory is the address space, so a number
    /// in the code is already the number the guest needs.
    fn linked(&self) -> bool {
        false
    }

    /// The guest function beginning at a virtual address, for a linked
    /// input where a call can cross from one placed section into another.
    fn function_at_address(&self, _address: u64) -> Result<FunctionReference> {
        bail!("this input has no addresses to resolve")
    }

    /// The virtual address a section was placed at, or zero for a
    /// relocatable object where nothing has been placed.
    ///
    /// The decoder runs with a section-relative program counter, so a
    /// program-counter-relative operand resolves to a section offset. Adding
    /// this is what turns it into the address the guest will actually see,
    /// once the loader has put the section where it belongs.
    fn section_address(&self, _section: usize) -> u64 {
        0
    }

    /// The function that turns a virtual address into an indirect-table
    /// slot, for a linked input where a function pointer is an address.
    fn exec_map(&self) -> Result<FunctionReference> {
        bail!("this input has no exec map")
    }

    /// The virtual address of a recovered jump table, for a linked input
    /// where the loader places it and no symbol is needed to find it.
    fn jump_table_at(&self, _section: usize, _offset: u64) -> Result<u64> {
        bail!("this input has no addresses to resolve")
    }

    /// The kernel-seam entry a `syscall` instruction calls.
    ///
    /// `syscall` names nothing — it has no operand at all — so there is no
    /// symbol in the input to resolve. The transpiler declares the seam's
    /// symbol itself when the object contains one, and this is how the
    /// translation reaches it.
    fn syscall_entry(&self) -> Result<FunctionReference>;
}

/// How far a translated `syscall` moves `%rsp`: the 128-byte red zone it must
/// not touch, plus the eight-byte slot carrying the resume ID.
///
/// `136 % 16 == 8`, the same alignment parity a `call` leaves behind, so
/// nothing downstream can tell the two apart by alignment.
pub const SYSCALL_RESERVATION: i64 = 136;

/// Marks a resume ID as naming a site that reserved [`SYSCALL_RESERVATION`]
/// rather than a call site's eight bytes.
///
/// It rides in the entry-index half of the ID because that is the half with
/// room, and the driver masks it off before using the index. Sites are what
/// know their own frame size; the driver is generic and must be told.
pub const RED_ZONE_RESERVED: i64 = 1 << 63;

/// The entry index of a resume ID, once the frame-size marker is removed.
pub const RESUME_ENTRY_MASK: i64 = 0x7fff_ffff;

/// The three values a flag rule reads: an operation's two inputs and its
/// result, each parked in a local by the time flags are computed.
#[derive(Clone, Copy)]
struct OperationValues {
    left: u32,
    right: u32,
    result: u32,
}

/// What a bit-test instruction does to the bit it selected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BitAction {
    /// `bt`: reads it and writes nothing back.
    Read,
    Set,
    Clear,
    Complement,
}

/// Which flags an operation writes, and by what rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FlagRule {
    /// `result = left + right`.
    Addition,
    /// `result = left - right`.
    Subtraction,
    /// Bitwise: carry and overflow are cleared.
    Logical,
}

pub struct FunctionTranslator<'a> {
    symbols: &'a dyn SymbolResolver,
    state: FunctionState<'a>,
    /// Section the function being translated lives in, which is what a
    /// relocation-free call target is an offset into.
    section: usize,
    /// Type index of the guest calling convention. Every translated function
    /// has it, so every `call_indirect` names it and the signature agreement
    /// that normally makes indirect calls difficult never arises.
    guest_type: u32,
    temporaries: Temporaries,
    /// When present, every call site stores a resume ID in its
    /// return-address slot instead of the sentinel; see [`ResumeSites`].
    resume_sites: Option<ResumeSites>,
    /// Set when translating a resume body: `ret` loads the ID it is popping
    /// and returns it, so the resume driver can re-enter the frame above.
    yield_on_return: bool,
}

/// The resume IDs of one function's call sites.
///
/// A resume ID is what a return-address slot holds when checkpointing is on:
/// the table slot of the enclosing function's resume body in the low 32 bits,
/// and the resume body's entry index — the post-call block, or the epilogue
/// arm for a tail-call site — in the high 32. The chain of these slots on the
/// guest stack is a serialization of the frames above any call, which is what
/// lets a restored snapshot be walked back to life without re-execution.
pub struct ResumeSites {
    /// Table slot of the enclosing function's resume body.
    pub table_slot: TableReference,
    /// Resume-body entry index for each transfer instruction, keyed by the
    /// instruction's section offset.
    pub entries: std::collections::HashMap<u64, u32>,
}

/// A reusable pool of scratch locals. Instructions are translated one at a
/// time, so temporaries can be recycled between them.
#[derive(Default)]
struct Temporaries {
    i32_locals: Vec<u32>,
    i64_locals: Vec<u32>,
    f32_locals: Vec<u32>,
    f64_locals: Vec<u32>,
    v128_locals: Vec<u32>,
    i32_used: usize,
    i64_used: usize,
    f32_used: usize,
    f64_used: usize,
    v128_used: usize,
}

impl Temporaries {
    fn reset(&mut self) {
        self.i32_used = 0;
        self.i64_used = 0;
        self.f32_used = 0;
        self.f64_used = 0;
        self.v128_used = 0;
    }

    fn take(&mut self, body: &mut FunctionBodyBuilder, value_type: ValueType) -> u32 {
        let (pool, used) = match value_type {
            ValueType::I32 => (&mut self.i32_locals, &mut self.i32_used),
            ValueType::I64 => (&mut self.i64_locals, &mut self.i64_used),
            ValueType::F32 => (&mut self.f32_locals, &mut self.f32_used),
            ValueType::F64 => (&mut self.f64_locals, &mut self.f64_used),
            ValueType::V128 => (&mut self.v128_locals, &mut self.v128_used),
        };
        if *used == pool.len() {
            pool.push(body.declare_local(value_type));
        }
        let local = pool[*used];
        *used += 1;
        local
    }
}

impl<'a> FunctionTranslator<'a> {
    pub fn new(
        symbols: &'a dyn SymbolResolver,
        machine: &'a MachineState,
        section: usize,
        guest_type: u32,
    ) -> Self {
        Self {
            symbols,
            state: FunctionState::new(machine),
            section,
            guest_type,
            temporaries: Temporaries::default(),
            resume_sites: None,
            yield_on_return: false,
        }
    }

    /// Makes every call site store its resume ID instead of the sentinel.
    pub fn enable_resume(&mut self, sites: ResumeSites) {
        self.resume_sites = Some(sites);
    }

    /// Makes `ret` return the ID it pops — the shape a resume body has, so
    /// the driver learns where the frame above continues.
    pub fn yield_next_site_on_return(&mut self) {
        self.yield_on_return = true;
    }

    /// Builds the promotion plan for the function being translated and emits
    /// its entry copies, so that everything after works on locals.
    ///
    /// The touched and written sets come from the same instruction facts the
    /// signature inference uses, which cover the whole instruction set — an
    /// instruction's registers are known even where its translation is not.
    /// A cell the scan misses stays in its global, which is slower, never
    /// wrong: every access to a cell resolves the same way for the whole
    /// function. That holds for cells only the *guest* writes. The stack
    /// pointer is not one of them — see below.
    pub fn begin_function(&mut self, body: &mut FunctionBodyBuilder, lifted: &LiftedFunction) {
        use crate::abi::effects::Location;
        let mut factory = iced_x86::InstructionInfoFactory::new();
        let mut touched = LocationSet::new();
        let mut written = LocationSet::new();
        // The stack pointer is written by the *translation*, not only by the
        // guest's instructions, and the scan cannot see that. A call site
        // reserves a return-address slot and a `syscall` reserves the red
        // zone as well; neither is an instruction `iced` reports as touching
        // `%rsp` — it reports `syscall` as writing `rcx` and `r11` and
        // nothing else. A function that reads `%rsp` without ever writing it
        // through a `push`, `pop`, `call` or `ret` — a leaf that exits by
        // tail jump — would therefore promote `%rsp` to a local, have the
        // reservation update that local, and never publish it, leaving the
        // guest's stack pointer shifted by the reservation for the rest of
        // its life with nothing to say so.
        //
        // Unconditional rather than derived from which transfers the
        // function contains, because "which instructions make the translator
        // move `%rsp`" is a second rule that can drift from the first.
        touched.insert(Location::Integer(STACK_POINTER_REGISTER));
        written.insert(Location::Integer(STACK_POINTER_REGISTER));

        // The segment base is not in the location sets, and deliberately so:
        // those describe where an *argument* can travel, which is what
        // signature inference reads them for, and `%fs` never carries one.
        // Its promotion is decided the same way, by a scan of the function,
        // and recorded separately.
        let mut segment_base = false;
        for instruction in &lifted.instructions {
            let effects = effects_of(&instruction.instruction, &mut factory);
            touched.union_with(effects.reads);
            touched.union_with(effects.writes);
            written.union_with(effects.writes);
            segment_base |= instruction.instruction.segment_prefix() == Register::FS;
        }
        self.state.promote(body, touched, written, segment_base);
    }

    /// Pushes the arm a recovered `switch` selects.
    ///
    /// The dispatch's own address arithmetic has already run, and the entries
    /// were rewritten so that whatever it computed equals the table's address
    /// plus the arm's index — so the index is what is left after taking the
    /// table's address back out. Nothing about how the guest got there needs
    /// to be understood.
    pub fn emit_switch_index(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        table: &crate::jump_table::JumpTable,
    ) -> Result<()> {
        self.temporaries.reset();
        self.read_operand(body, lifted, 0, OperandWidth::QuadWord)
            .with_context(|| format!("translating `{}`", render(&lifted.instruction)))?;
        body.i32_wrap_i64();
        if self.symbols.linked() {
            // The table is at its virtual address, because that is where the
            // loader puts the segment holding it and linear memory is the
            // address space. So the subtraction is a constant rather than a
            // symbol the linker will fill in.
            body.i32_const(
                self.symbols
                    .jump_table_at(table.table_section, table.table_offset)? as i32,
            );
        } else {
            let address = self
                .symbols
                .jump_table_address(table.table_section, table.table_offset)?;
            body.i32_const_data_address(address);
        }
        body.i32_sub();
        Ok(())
    }

    /// Translates one instruction that is not a control transfer.
    ///
    /// Transfers — `jcc`, `jmp`, `ret` — belong to the structurer, which is
    /// the only place that knows how a given control-flow translation spells
    /// them; see [`branch_condition`](Self::branch_condition),
    /// [`emit_tail_call`](Self::emit_tail_call) and
    /// [`emit_return`](Self::emit_return).
    pub fn translate_instruction(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        self.temporaries.reset();
        self.translate_dispatch(body, lifted)
            .with_context(|| format!("translating `{}`", render(&lifted.instruction)))
    }

    /// Pushes `1` or `0` according to a conditional jump's condition, so the
    /// structurer can turn it into whatever branch its mode uses.
    pub fn branch_condition(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        self.temporaries.reset();
        if !is_conditional_jump(&lifted.instruction) {
            bail!(
                "`{}` is not a conditional jump",
                render(&lifted.instruction)
            );
        }
        self.emit_condition(body, lifted.instruction.condition_code())
            .with_context(|| format!("translating `{}`", render(&lifted.instruction)))
    }

    /// A tail jump: call the target, then let the structurer return.
    pub fn emit_tail_call(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        self.temporaries.reset();
        // A tail jump is the one exit whose target may read this function's
        // flags: the compiler splits a single function across sections and
        // jumps between the halves, and the cold half can branch on what the
        // hot half compared.
        self.state.flush_flags(body);
        self.emit_transfer(body, lifted)
            .with_context(|| format!("translating `{}`", render(&lifted.instruction)))?;
        self.emit_return(body);
        Ok(())
    }

    /// A call or tail jump, direct or indirect.
    fn emit_transfer(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        match self.call_target(lifted)? {
            CallTarget::Direct(target) => {
                self.reserve_return_address(body, lifted)?;
                self.state.flush_written(body);
                body.call(target);
                self.state.reload(body);
            }
            CallTarget::Indirect => {
                // The table slot has to be read *before* the stack pointer
                // moves: a call through a stack slot would otherwise be read
                // from the wrong address.
                let slot = self.temporaries.take(body, ValueType::I64);
                self.read_operand(body, lifted, 0, OperandWidth::QuadWord)?;
                body.local_set(slot);
                self.reserve_return_address(body, lifted)?;
                self.state.flush_written(body);
                body.local_get(slot);
                if self.symbols.linked() {
                    // The value is a virtual address, because that is what a
                    // function pointer is once a linker has placed
                    // everything. The map turns it into the slot the table
                    // is indexed by; an address that is not a function's
                    // traps there rather than calling whatever occupies the
                    // slot it would have been mistaken for.
                    body.call(self.symbols.exec_map()?);
                } else {
                    body.i32_wrap_i64();
                }
                body.call_indirect(self.guest_type);
                self.state.reload(body);
            }
        }
        Ok(())
    }

    /// The stack bookkeeping a `ret` performs: the caller reserved a
    /// return-address slot, and leaving gives it back — followed by the
    /// flush, because leaving is where the caller starts reading the
    /// globals.
    ///
    /// In a resume body the popped slot is also the answer: it holds the
    /// resume ID of the frame above, which the driver needs next, so it is
    /// read before the pop and left on the stack for the `return`.
    pub fn emit_return(&mut self, body: &mut FunctionBodyBuilder) {
        if self.yield_on_return {
            let next = self.temporaries.take(body, ValueType::I64);
            self.push_stack_pointer_address(body);
            body.i64_load(OperandWidth::QuadWord.alignment_log2(), 0);
            body.local_set(next);
            self.adjust_stack_pointer(body, 8);
            self.state.flush_written(body);
            body.local_get(next);
        } else {
            self.adjust_stack_pointer(body, 8);
            self.state.flush_written(body);
        }
    }

    fn translate_dispatch(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let instruction = &lifted.instruction;

        if matches!(
            instruction.flow_control(),
            FlowControl::ConditionalBranch
                | FlowControl::UnconditionalBranch
                | FlowControl::IndirectBranch
                | FlowControl::Return
        ) {
            bail!(
                "`{}` ends a basic block; the structurer translates it, not \
                 the instruction translator",
                render(instruction)
            );
        }
        if is_set_condition(instruction.mnemonic()) {
            return self.translate_set_condition(body, lifted);
        }
        if is_conditional_move(instruction.mnemonic()) {
            return self.translate_conditional_move(body, lifted);
        }

        match instruction.mnemonic() {
            // `endbr64` is a landing pad for control-flow enforcement, and
            // there is nothing in wasm for it to enforce.
            Mnemonic::Nop | Mnemonic::Endbr64 => Ok(()),
            Mnemonic::Neg => self.translate_negate(body, lifted),
            Mnemonic::Not => self.translate_complement(body, lifted),
            Mnemonic::Inc => self.translate_step(body, lifted, FlagRule::Addition),
            Mnemonic::Dec => self.translate_step(body, lifted, FlagRule::Subtraction),
            Mnemonic::Rol => self.translate_rotate(body, lifted, true),
            Mnemonic::Ror => self.translate_rotate(body, lifted, false),
            Mnemonic::Bt => self.translate_bit_test(body, lifted, BitAction::Read),
            Mnemonic::Bts => self.translate_bit_test(body, lifted, BitAction::Set),
            Mnemonic::Btr => self.translate_bit_test(body, lifted, BitAction::Clear),
            Mnemonic::Btc => self.translate_bit_test(body, lifted, BitAction::Complement),
            Mnemonic::Shl | Mnemonic::Sal => self.translate_shift(body, lifted, ShiftKind::Left),
            Mnemonic::Shr => self.translate_shift(body, lifted, ShiftKind::RightLogical),
            Mnemonic::Sar => self.translate_shift(body, lifted, ShiftKind::RightArithmetic),
            // The one-operand form is a different instruction wearing the same
            // mnemonic: it multiplies the accumulator and writes a result
            // twice as wide, where the two- and three-operand forms keep only
            // the low half.
            Mnemonic::Imul if lifted.instruction.op_count() == 1 => {
                self.translate_wide_multiply(body, lifted, true)
            }
            Mnemonic::Imul => self.translate_signed_multiply(body, lifted),
            Mnemonic::Mul => self.translate_wide_multiply(body, lifted, false),
            Mnemonic::Idiv => self.translate_divide(body, lifted, true),
            Mnemonic::Div => self.translate_divide(body, lifted, false),
            Mnemonic::Cbw => self.translate_widen_accumulator(body, OperandWidth::Byte),
            Mnemonic::Cwde => self.translate_widen_accumulator(body, OperandWidth::Word),
            Mnemonic::Cdqe => self.translate_widen_accumulator(body, OperandWidth::DoubleWord),
            Mnemonic::Cwd => self.translate_sign_into_data_register(body, OperandWidth::Word),
            Mnemonic::Cdq => self.translate_sign_into_data_register(body, OperandWidth::DoubleWord),
            Mnemonic::Cqo => self.translate_sign_into_data_register(body, OperandWidth::QuadWord),
            Mnemonic::Mov => self.translate_move(body, lifted),
            Mnemonic::Xchg => self.translate_exchange(body, lifted),
            Mnemonic::Xadd => self.translate_exchange_and_add(body, lifted),
            Mnemonic::Cmpxchg => self.translate_compare_and_exchange(body, lifted),
            Mnemonic::Movzx | Mnemonic::Movsx | Mnemonic::Movsxd => {
                self.translate_extending_move(body, lifted)
            }
            Mnemonic::Lea => self.translate_load_effective_address(body, lifted),
            Mnemonic::Add => self.translate_arithmetic(body, lifted, FlagRule::Addition, true),
            Mnemonic::Sub => self.translate_arithmetic(body, lifted, FlagRule::Subtraction, true),
            Mnemonic::Adc => self.translate_carrying(body, lifted, FlagRule::Addition),
            Mnemonic::Sbb => self.translate_carrying(body, lifted, FlagRule::Subtraction),
            Mnemonic::Cmp => self.translate_arithmetic(body, lifted, FlagRule::Subtraction, false),
            Mnemonic::And => self.translate_arithmetic(body, lifted, FlagRule::Logical, true),
            Mnemonic::Or => self.translate_arithmetic(body, lifted, FlagRule::Logical, true),
            Mnemonic::Xor => self.translate_arithmetic(body, lifted, FlagRule::Logical, true),
            Mnemonic::Test => self.translate_arithmetic(body, lifted, FlagRule::Logical, false),
            Mnemonic::Call => self.translate_call(body, lifted),
            Mnemonic::Syscall => self.translate_syscall(body, lifted),
            // `leave` is `mov rsp, rbp` followed by `pop rbp`, which is how
            // an unoptimised frame is torn down.
            Mnemonic::Leave => {
                let frame = RegisterSlice::of(Register::RBP)?;
                let stack = RegisterSlice::of(Register::RSP)?;
                self.state.read_register(body, frame);
                self.state.write_register(body, stack);
                self.push_stack_pointer_address(body);
                body.i64_load(OperandWidth::QuadWord.alignment_log2(), 0);
                let value = self.temporaries.take(body, ValueType::I64);
                body.local_set(value);
                self.adjust_stack_pointer(body, 8);
                body.local_get(value);
                self.state.write_register(body, frame);
                Ok(())
            }
            Mnemonic::Push => self.translate_push(body, lifted),
            Mnemonic::Pop => self.translate_pop(body, lifted),
            other => match self.translate_vector(body, lifted)? {
                vector::VectorOutcome::Translated => Ok(()),
                vector::VectorOutcome::NotAVectorInstruction => bail!(
                    "instruction `{}` ({other:?}) is not implemented",
                    render(&lifted.instruction)
                ),
            },
        }
    }

    // ---- operand plumbing ------------------------------------------------

    /// Width of an operand, in bytes.
    fn operand_width(&self, instruction: &Instruction, index: u32) -> Result<OperandWidth> {
        match instruction.op_kind(index) {
            OpKind::Register => OperandWidth::from_bytes(instruction.op_register(index).size()),
            OpKind::Memory => OperandWidth::from_bytes(instruction.memory_size().size()),
            _ => bail!("operand {index} has no width of its own"),
        }
    }

    /// Pushes the value of an operand, zero-extended into its carrier.
    fn read_operand(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        index: u32,
        width: OperandWidth,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        match instruction.op_kind(index) {
            OpKind::Register => {
                let slice = RegisterSlice::of(instruction.op_register(index))?;
                self.state.read_register(body, slice);
                Ok(())
            }
            OpKind::Memory => {
                // A load through a global offset table slot is the
                // indirection a linker relaxes away: the operand's value is
                // the symbol's address, not what is stored at it.
                if let Some(reference) = lifted.displacement
                    && reference.via_global_offset_table
                {
                    // This path returns the symbol's address without ever
                    // building an effective address, so it is the one place a
                    // segment prefix could slip through unexamined.
                    Self::check_segment_prefix(&lifted.instruction)?;
                    if width != OperandWidth::QuadWord {
                        bail!(
                            "a global offset table slot is eight bytes wide, \
                             but the operand is {width:?}"
                        );
                    }
                    return self.push_symbol_value_wide(body, reference);
                }
                self.emit_effective_address(body, lifted)?;
                emit_load(body, width);
                Ok(())
            }
            _ => self.read_immediate(body, lifted, index, width),
        }
    }

    fn read_immediate(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        index: u32,
        width: OperandWidth,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        if !is_immediate(instruction.op_kind(index)) {
            bail!("operand {index} is not an immediate");
        }

        // A relocated immediate names an address — of data, or of a
        // function — rather than a number.
        if let Some(reference) = lifted.immediate {
            self.push_symbol_value(body, reference)?;
            match width {
                OperandWidth::QuadWord => body.i64_extend_i32_unsigned(),
                OperandWidth::DoubleWord => {}
                _ => bail!("a relocated immediate cannot fit in {width:?}"),
            }
            return Ok(());
        }

        let value = mask_to_width(instruction.immediate(index), width);
        match width.carrier() {
            Carrier::I32 => body.i32_const(value as i32),
            Carrier::I64 => body.i64_const(value as i64),
        }
        Ok(())
    }

    /// Stores the value held in `value_local` into an operand.
    fn write_operand(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        index: u32,
        width: OperandWidth,
        value_local: u32,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        match instruction.op_kind(index) {
            OpKind::Register => {
                let slice = RegisterSlice::of(instruction.op_register(index))?;
                body.local_get(value_local);
                self.state.write_register(body, slice);
                Ok(())
            }
            OpKind::Memory => {
                // A store takes the address first, so the value has to have
                // been parked in a local by now.
                self.emit_effective_address(body, lifted)?;
                body.local_get(value_local);
                emit_store(body, width);
                Ok(())
            }
            kind => bail!("cannot write to a {kind:?} operand"),
        }
    }

    /// Pushes the effective address of the instruction's memory operand,
    /// wrapped to the 32-bit linear-memory address space.
    ///
    /// This is the single choke point the design keeps for the pointer-width
    /// decision: address arithmetic happens in `i64`, and exactly one place
    /// narrows it.
    fn emit_effective_address(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        self.emit_address_arithmetic(body, lifted)?;
        body.i32_wrap_i64();
        Ok(())
    }

    /// Pushes the address a memory operand computes, as a full `i64`.
    ///
    /// An `%fs`-prefixed operand adds the segment base into the effective
    /// address, and that is the entire cost of thread-local storage: one
    /// extra add on the instructions that carry the prefix, and nothing
    /// anywhere else. `%gs` stays a loud error until something real needs
    /// it — no libc on this path uses it, and guessing at a second segment
    /// nobody exercises is how a silent divergence gets built.
    fn emit_address_arithmetic(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let segment = Self::check_segment_prefix(&lifted.instruction)?;
        self.emit_unsegmented_address(body, lifted)?;
        if segment == Register::FS {
            self.state.read_segment_base(body);
            body.i64_add();
        }
        Ok(())
    }

    /// The segment a memory operand is prefixed with, refusing any this
    /// translation cannot honour.
    ///
    /// `%gs` is deliberately loud rather than approximated: nothing on this
    /// path uses it, and a libc that reached for it would be a libc nothing
    /// here has been tested against.
    fn check_segment_prefix(instruction: &Instruction) -> Result<Register> {
        match instruction.segment_prefix() {
            segment @ (Register::None | Register::FS) => Ok(segment),
            other => bail!(
                "segment-prefixed memory operand ({other:?}) is out of scope; \
                 only `%fs` is translated"
            ),
        }
    }

    /// The address a memory operand computes before any segment base is
    /// added: base, index, scale and displacement.
    fn emit_unsegmented_address(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let base = instruction.memory_base();
        let index = instruction.memory_index();
        let scale = instruction.memory_index_scale();
        let mut have_term = false;

        if base != Register::None && base != Register::RIP {
            if base.size() != 8 {
                bail!("32-bit addressing (base {base:?}) is out of scope");
            }
            self.state.read_register(body, RegisterSlice::of(base)?);
            have_term = true;
        }

        if index != Register::None {
            if index.size() != 8 {
                bail!("32-bit addressing (index {index:?}) is out of scope");
            }
            self.state.read_register(body, RegisterSlice::of(index)?);
            if scale > 1 {
                body.i64_const(scale.trailing_zeros() as i64);
                body.i64_shl();
            }
            if have_term {
                body.i64_add();
            }
            have_term = true;
        }

        match lifted.displacement {
            Some(reference) => {
                self.push_symbol_value_wide(body, reference)?;
                if have_term {
                    body.i64_add();
                }
            }
            None => {
                if base == Register::RIP {
                    if self.symbols.linked() {
                        // Nothing to resolve — only to place. The
                        // displacement is a section offset, because that is
                        // what the decoder's program counter is; the address
                        // the guest sees is that offset in the section the
                        // loader put somewhere. Linear memory is the address
                        // space, so the sum is the number the guest needs.
                        // Wrapping for the same reason a backwards call
                        // wraps: an operand below its own section's start
                        // is a negative offset already wrapped.
                        let address = self
                            .symbols
                            .section_address(self.section)
                            .wrapping_add(instruction.memory_displacement64());
                        body.i64_const(address as i64);
                        if have_term {
                            body.i64_add();
                        }
                        return Ok(());
                    }
                    // No relocation, so the assembler already resolved this
                    // against something in the same section: the address of a
                    // function, which is a table slot.
                    let target = instruction.memory_displacement64();
                    let slot = self.symbols.table_slot_at(self.section, target)?;
                    body.i32_const_table_index(slot);
                    body.i64_extend_i32_unsigned();
                    if have_term {
                        body.i64_add();
                    }
                    return Ok(());
                }
                let displacement = instruction.memory_displacement64() as i64;
                if displacement != 0 || !have_term {
                    body.i64_const(displacement);
                    if have_term {
                        body.i64_add();
                    }
                }
            }
        }

        Ok(())
    }

    /// Pushes what a symbolic operand denotes, as an `i32`: an address in
    /// linear memory, or a slot in the indirect function table.
    fn push_symbol_value(
        &mut self,
        body: &mut FunctionBodyBuilder,
        reference: SymbolReference,
    ) -> Result<()> {
        match self.symbols.value(reference.symbol, reference.addend)? {
            SymbolValue::Address(data) => body.i32_const_data_address(data),
            SymbolValue::FunctionPointer(function) => body.i32_const_table_index(function),
        }
        Ok(())
    }

    /// The same, widened to an `i64` for address arithmetic.
    fn push_symbol_value_wide(
        &mut self,
        body: &mut FunctionBodyBuilder,
        reference: SymbolReference,
    ) -> Result<()> {
        self.push_symbol_value(body, reference)?;
        body.i64_extend_i32_unsigned();
        Ok(())
    }

    // ---- instructions ----------------------------------------------------

    fn translate_move(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let width = self.destination_width(instruction)?;
        self.read_operand(body, lifted, 1, width)?;
        let value = self.temporaries.take(body, width.value_type());
        body.local_set(value);
        self.write_operand(body, lifted, 0, width, value)
    }

    fn translate_extending_move(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let destination_width = self.destination_width(instruction)?;
        let source_width = self.operand_width(instruction, 1)?;
        let signed = instruction.mnemonic() != Mnemonic::Movzx;

        self.read_operand(body, lifted, 1, source_width)?;
        emit_widen(body, source_width, destination_width, signed);

        let value = self.temporaries.take(body, destination_width.value_type());
        body.local_set(value);
        self.write_operand(body, lifted, 0, destination_width, value)
    }

    fn translate_load_effective_address(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let width = self.destination_width(instruction)?;
        // `lea` is pure arithmetic: no memory access, no flags, and no
        // narrowing to the linear-memory address space. It also ignores a
        // segment override — there is no access for a segment to apply to,
        // and gas says as much ("segment override on `lea' is ineffectual")
        // while still emitting the prefix byte. Honouring it here would add
        // the thread pointer to an address the hardware computes without it.
        self.emit_unsegmented_address(body, lifted)?;
        match width {
            OperandWidth::QuadWord => {}
            OperandWidth::DoubleWord => body.i32_wrap_i64(),
            other => bail!("`lea` into a {other:?} destination is out of scope"),
        }
        let value = self.temporaries.take(body, width.value_type());
        body.local_set(value);
        self.write_operand(body, lifted, 0, width, value)
    }

    /// Two-operand arithmetic and logic: `destination op= source`, with
    /// flags. `writes_back` is false for `cmp` and `test`, which set flags
    /// and discard the result.
    /// `adc` and `sbb`: the same addition and subtraction with the carry flag
    /// folded in, and folded back out again.
    ///
    /// Compilers reach for these well outside multi-word arithmetic. clang
    /// turns `value == 0 ? 0 : f(value - 1)` into an `adc` against the flag a
    /// comparison already set, which is how this arrived: as a mutual
    /// recursion in the inference corpus.
    ///
    /// The carry *out* is the part that is not just the ordinary rule. A sum
    /// that lands exactly on its left operand has not wrapped when nothing was
    /// carried in, and has wrapped when something was — `left + 0xff...ff + 1`
    /// is `left` again, one whole turn around.
    fn translate_carrying(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        rule: FlagRule,
    ) -> Result<()> {
        let width = self.destination_width(&lifted.instruction)?;
        let value_type = width.value_type();

        let left = self.temporaries.take(body, value_type);
        let right = self.temporaries.take(body, value_type);
        let carried = self.temporaries.take(body, value_type);
        let result = self.temporaries.take(body, value_type);
        let carry_in = self.temporaries.take(body, ValueType::I32);

        self.read_operand(body, lifted, 0, width)?;
        body.local_set(left);
        self.read_operand(body, lifted, 1, width)?;
        body.local_set(right);

        self.state.read_flag(body, Flag::Carry);
        body.local_tee(carry_in);
        if width.carrier() == Carrier::I64 {
            body.i64_extend_i32_unsigned();
        }
        body.local_set(carried);

        let operation = match rule {
            FlagRule::Addition => BinaryOperation::Add,
            _ => BinaryOperation::Subtract,
        };
        body.local_get(left);
        body.local_get(right);
        emit_binary(body, width, operation);
        body.local_get(carried);
        emit_binary(body, width, operation);
        body.local_set(result);

        self.write_operand(body, lifted, 0, width, result)?;

        // Carry out. The `_ == _` half only matters when a carry came in, so
        // it is guarded by the incoming flag rather than folded in
        // arithmetically — the arithmetic version would have to widen to
        // avoid wrapping in exactly the case being detected.
        let (low, high) = match rule {
            FlagRule::Addition => (result, left),
            _ => (left, right),
        };
        body.local_get(low);
        body.local_get(high);
        emit_less_than_unsigned(body, width);
        body.local_get(low);
        body.local_get(high);
        emit_equal(body, width);
        body.local_get(carry_in);
        body.i32_and();
        body.i32_or();
        self.state.write_flag(body, Flag::Carry);

        self.emit_flags(
            body,
            rule,
            width,
            OperationValues {
                left,
                right,
                result,
            },
            false,
        );
        Ok(())
    }

    fn translate_arithmetic(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        rule: FlagRule,
        writes_back: bool,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let width = self.destination_width(instruction)?;
        let value_type = width.value_type();

        let left = self.temporaries.take(body, value_type);
        let right = self.temporaries.take(body, value_type);
        let result = self.temporaries.take(body, value_type);

        self.read_operand(body, lifted, 0, width)?;
        body.local_set(left);
        self.read_operand(body, lifted, 1, width)?;
        body.local_set(right);

        body.local_get(left);
        body.local_get(right);
        match (rule, instruction.mnemonic()) {
            (FlagRule::Addition, _) => emit_binary(body, width, BinaryOperation::Add),
            (FlagRule::Subtraction, _) => emit_binary(body, width, BinaryOperation::Subtract),
            (FlagRule::Logical, Mnemonic::And | Mnemonic::Test) => {
                emit_binary(body, width, BinaryOperation::And)
            }
            (FlagRule::Logical, Mnemonic::Or) => emit_binary(body, width, BinaryOperation::Or),
            (FlagRule::Logical, Mnemonic::Xor) => emit_binary(body, width, BinaryOperation::Xor),
            (rule, mnemonic) => bail!("no {rule:?} rule for {mnemonic:?}"),
        }
        body.local_set(result);

        if writes_back {
            self.write_operand(body, lifted, 0, width, result)?;
        }
        self.emit_flags(
            body,
            rule,
            width,
            OperationValues {
                left,
                right,
                result,
            },
            true,
        );
        Ok(())
    }

    fn translate_push(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        // `push imm` has no operand width of its own; on x86-64 it pushes a
        // sign-extended quadword.
        let width = if is_immediate(lifted.instruction.op_kind(0)) {
            OperandWidth::QuadWord
        } else {
            self.operand_width(&lifted.instruction, 0)?
        };
        if width != OperandWidth::QuadWord {
            bail!("`push` of a {width:?} operand is out of scope");
        }
        let value = self.temporaries.take(body, ValueType::I64);
        self.read_operand(body, lifted, 0, OperandWidth::QuadWord)?;
        body.local_set(value);

        self.adjust_stack_pointer(body, -8);
        self.push_stack_pointer_address(body);
        body.local_get(value);
        body.i64_store(OperandWidth::QuadWord.alignment_log2(), 0);
        Ok(())
    }

    fn translate_pop(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let width = self.operand_width(&lifted.instruction, 0)?;
        if width != OperandWidth::QuadWord {
            bail!("`pop` into a {width:?} operand is out of scope");
        }
        let value = self.temporaries.take(body, ValueType::I64);
        self.push_stack_pointer_address(body);
        body.i64_load(OperandWidth::QuadWord.alignment_log2(), 0);
        body.local_set(value);
        self.adjust_stack_pointer(body, 8);
        self.write_operand(body, lifted, 0, OperandWidth::QuadWord, value)
    }

    // ---- control transfers ----------------------------------------------

    fn translate_call(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        self.emit_transfer(body, lifted)
    }

    /// `syscall`, translated as what it is here: a direct call to the kernel
    /// seam, over a stack reservation that skips the guest's red zone.
    ///
    /// It is emitted through the same machinery as a `call` — reserve the
    /// return-address slot, flush, call, reload — rather than as a special
    /// case, and everything that buys follows from that one decision. The
    /// flush discipline puts the whole machine state in the globals before
    /// the kernel sees it, which is the invariant the scheduler rests on;
    /// the reserved slot means that under `--resume` every syscall site is a
    /// resume site with nothing extra built, since `syscall` classifies as a
    /// call and therefore already has a site-map entry.
    ///
    /// The hardware's `rcx`/`r11` clobber needs no code: `iced` reports both
    /// as written, so the promotion scan has them in the written set, they
    /// are flushed here like any other written register, and the seam's own
    /// thunk is what puts a value in them.
    ///
    /// What a `syscall` may *not* borrow from a `call` is the stack. A callee
    /// is allowed to destroy the 128 bytes below `%rsp`; the kernel is not,
    /// and compilers depend on the difference — gcc at `-O2` keeps a leaf
    /// function's locals in the red zone across an inline `syscall` without
    /// moving `%rsp` at all. So the slot goes *below* the red zone rather
    /// than on top of it. The reservation is the same size for every build,
    /// because one seam serves guests translated with and without resume, and
    /// it moves `%rsp` by a multiple of sixteen plus eight — the same
    /// alignment parity a `call` produces.
    fn translate_syscall(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let target = self.symbols.syscall_entry()?;
        self.reserve_syscall_frame(body, lifted)?;
        self.state.flush_written(body);
        // Flags are exempt from the flush at an ordinary call on the ABI's
        // authority: they are call-clobbered, so nothing conforming reads one
        // it did not set since. A `syscall` has no such authority — Linux
        // restores `RFLAGS` from `r11`, so a guest may legitimately branch on
        // a flag it set before one. More decisively, the kernel *snapshots*
        // this state: a thread that blocks here has its register file saved
        // from the globals, and flags left in locals would be saved stale and
        // restored wrong on the far side of a context switch. No reload
        // afterwards, because the kernel does not change the guest's flags —
        // when signal delivery starts editing a `ucontext`, that is what has
        // to change.
        self.state.flush_flags(body);
        body.call(target);
        self.state.reload(body);
        Ok(())
    }

    /// The syscall's stack frame: the red zone stepped over, then the slot
    /// that carries the resume ID, at the stack pointer where the driver
    /// expects to find it.
    fn reserve_syscall_frame(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        self.adjust_stack_pointer(body, -SYSCALL_RESERVATION);
        self.push_stack_pointer_address(body);
        match &self.resume_sites {
            None => body.i64_const(crate::machine::RETURN_ADDRESS_SENTINEL),
            Some(resume) => {
                let entry = *resume.entries.get(&lifted.offset).ok_or_else(|| {
                    anyhow::anyhow!(
                        "the syscall at {:#x} has no resume entry; the site map \
                         and the translation disagree about what reserves a slot",
                        lifted.offset
                    )
                })?;
                body.i32_const_table_index(resume.table_slot);
                body.i64_extend_i32_unsigned();
                // The driver has to give back the whole reservation, not the
                // eight bytes a call site takes, and the only thing it has to
                // decide that from is the ID it just popped.
                body.i64_const(((entry as i64) << 32) | RED_ZONE_RESERVED);
                body.i64_or();
            }
        }
        body.i64_store(OperandWidth::QuadWord.alignment_log2(), 0);
        Ok(())
    }

    /// Where a direct call or tail jump goes.
    fn call_target(&self, lifted: &LiftedInstruction) -> Result<CallTarget> {
        // A call through a global offset table slot names its callee
        // directly; loading the slot is the indirection a linker removes.
        if let Some(reference) = lifted.displacement
            && reference.via_global_offset_table
        {
            return Ok(CallTarget::Direct(
                self.symbols.function(reference.symbol, reference.addend)?,
            ));
        }

        if let Some(reference) = lifted.immediate {
            return Ok(CallTarget::Direct(
                self.symbols.function(reference.symbol, reference.addend)?,
            ));
        }

        if lifted.instruction.op_kind(0) == OpKind::NearBranch64 {
            let target = lifted.instruction.near_branch64();
            // The decoder runs with the instruction's offset within its
            // section as the program counter, so a relative branch resolves
            // to another offset in the same section.
            //
            // In a relocatable object that is the whole story — the
            // assembler could only resolve a branch within the section it
            // was assembling. A linked one has had everything placed, so a
            // call crosses sections freely: `.text.unlikely` calls `.text`,
            // and the offset is then relative to a section the callee is not
            // in. Adding the section's own address makes it an address,
            // which is a thing the whole program shares.
            //
            // Wrapping, because a target below its own section's start is a
            // negative offset, which as an unsigned value has already
            // wrapped — and adding the base is what wraps it back. Calling
            // into the linkage table does exactly this: `.plt` sits below
            // `.text`, so every call to a stub is a backwards reach.
            return Ok(CallTarget::Direct(if self.symbols.linked() {
                self.symbols.function_at_address(
                    self.symbols
                        .section_address(self.section)
                        .wrapping_add(target),
                )?
            } else {
                self.symbols.function_at(self.section, target)?
            }));
        }

        // Anything else computes its target: a function pointer, which is a
        // slot in the indirect function table.
        Ok(CallTarget::Indirect)
    }

    /// Reserves the return-address slot the callee's `ret` will pop.
    ///
    /// The slot's value is never consulted by translated code — guest frames
    /// and wasm frames stay aligned, so the pop is pure bookkeeping — which
    /// is why it can hold the sentinel.
    ///
    /// A **tail jump** reserves one too, because it is translated as a call
    /// followed by a return. That makes a tail-called function see `%rsp`
    /// eight bytes lower than it would natively, where a `jmp` reuses the
    /// caller's slot. The offset is self-consistent — the target's own `ret`
    /// gives back exactly what was reserved, and every address the target
    /// computes is relative to the `%rsp` it was entered with — so nothing
    /// observes it except a guest that compares a stack address against one
    /// taken before the jump. `tests/corpus/syscall_leaf.s` records the
    /// property by storing `%rsp` on both sides of a tail jump; a guest that
    /// depended on the native value would be depending on the depth of its
    /// own call chain, which no compiler emits. With resume on, it holds the call
    /// site's resume ID instead: still never consulted in ordinary running,
    /// but a restored snapshot's driver reads the chain of them to rebuild
    /// the frames above the checkpoint.
    fn reserve_return_address(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        self.adjust_stack_pointer(body, -8);
        self.push_stack_pointer_address(body);
        match &self.resume_sites {
            None => body.i64_const(crate::machine::RETURN_ADDRESS_SENTINEL),
            Some(resume) => {
                let entry = *resume.entries.get(&lifted.offset).ok_or_else(|| {
                    anyhow::anyhow!(
                        "the transfer at {:#x} has no resume entry; the site map \
                         and the translation disagree about what reserves a slot",
                        lifted.offset
                    )
                })?;
                body.i32_const_table_index(resume.table_slot);
                body.i64_extend_i32_unsigned();
                body.i64_const((entry as i64) << 32);
                body.i64_or();
            }
        }
        body.i64_store(OperandWidth::QuadWord.alignment_log2(), 0);
        Ok(())
    }

    /// Pushes `1` or `0` according to a condition code, read from the
    /// modelled flags.
    fn emit_condition(
        &mut self,
        body: &mut FunctionBodyBuilder,
        condition: ConditionCode,
    ) -> Result<()> {
        let flag = |body: &mut FunctionBodyBuilder, which| {
            self.state.read_flag(body, which);
        };
        match condition {
            ConditionCode::o => flag(body, Flag::Overflow),
            ConditionCode::no => {
                flag(body, Flag::Overflow);
                body.i32_eqz();
            }
            ConditionCode::b => flag(body, Flag::Carry),
            ConditionCode::ae => {
                flag(body, Flag::Carry);
                body.i32_eqz();
            }
            ConditionCode::e => flag(body, Flag::Zero),
            ConditionCode::ne => {
                flag(body, Flag::Zero);
                body.i32_eqz();
            }
            ConditionCode::s => flag(body, Flag::Sign),
            ConditionCode::ns => {
                flag(body, Flag::Sign);
                body.i32_eqz();
            }
            ConditionCode::be => {
                flag(body, Flag::Carry);
                flag(body, Flag::Zero);
                body.i32_or();
            }
            ConditionCode::a => {
                flag(body, Flag::Carry);
                flag(body, Flag::Zero);
                body.i32_or();
                body.i32_eqz();
            }
            ConditionCode::l => {
                flag(body, Flag::Sign);
                flag(body, Flag::Overflow);
                body.i32_ne();
            }
            ConditionCode::ge => {
                flag(body, Flag::Sign);
                flag(body, Flag::Overflow);
                body.i32_eq();
            }
            ConditionCode::le => {
                flag(body, Flag::Zero);
                flag(body, Flag::Sign);
                flag(body, Flag::Overflow);
                body.i32_ne();
                body.i32_or();
            }
            ConditionCode::g => {
                flag(body, Flag::Zero);
                body.i32_eqz();
                flag(body, Flag::Sign);
                flag(body, Flag::Overflow);
                body.i32_eq();
                body.i32_and();
            }
            ConditionCode::p => flag(body, Flag::Parity),
            ConditionCode::np => {
                flag(body, Flag::Parity);
                body.i32_eqz();
            }
            ConditionCode::None => bail!("instruction has no condition code"),
        }
        Ok(())
    }

    fn translate_set_condition(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        self.emit_condition(body, lifted.instruction.condition_code())?;
        let value = self.temporaries.take(body, ValueType::I32);
        body.local_set(value);
        self.write_operand(body, lifted, 0, OperandWidth::Byte, value)
    }

    fn translate_conditional_move(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let width = self.destination_width(&lifted.instruction)?;
        // Reading and writing the destination unconditionally reproduces
        // x86's quirk that a 32-bit `cmov` zeroes the upper half of its
        // destination whether or not the condition holds.
        self.read_operand(body, lifted, 1, width)?;
        self.read_operand(body, lifted, 0, width)?;
        self.emit_condition(body, lifted.instruction.condition_code())?;
        body.select();

        let value = self.temporaries.take(body, width.value_type());
        body.local_set(value);
        self.write_operand(body, lifted, 0, width, value)
    }

    // ---- unary and shift operations --------------------------------------

    fn translate_negate(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let width = self.destination_width(&lifted.instruction)?;
        let value_type = width.value_type();
        let zero = self.temporaries.take(body, value_type);
        let operand = self.temporaries.take(body, value_type);
        let result = self.temporaries.take(body, value_type);

        emit_constant(body, width, 0);
        body.local_set(zero);
        self.read_operand(body, lifted, 0, width)?;
        body.local_set(operand);

        body.local_get(zero);
        body.local_get(operand);
        emit_binary(body, width, BinaryOperation::Subtract);
        body.local_set(result);

        self.write_operand(body, lifted, 0, width, result)?;
        self.emit_flags(
            body,
            FlagRule::Subtraction,
            width,
            OperationValues {
                left: zero,
                right: operand,
                result,
            },
            true,
        );
        Ok(())
    }

    fn translate_complement(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let width = self.destination_width(&lifted.instruction)?;
        self.read_operand(body, lifted, 0, width)?;
        emit_constant(body, width, width_all_ones(width));
        emit_binary(body, width, BinaryOperation::Xor);

        let result = self.temporaries.take(body, width.value_type());
        body.local_set(result);
        // `not` leaves every flag alone.
        self.write_operand(body, lifted, 0, width, result)
    }

    /// `inc` and `dec`: like `add`/`sub` of one, except that the carry flag
    /// keeps its previous value.
    fn translate_step(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        rule: FlagRule,
    ) -> Result<()> {
        let width = self.destination_width(&lifted.instruction)?;
        let value_type = width.value_type();
        let left = self.temporaries.take(body, value_type);
        let right = self.temporaries.take(body, value_type);
        let result = self.temporaries.take(body, value_type);

        self.read_operand(body, lifted, 0, width)?;
        body.local_set(left);
        emit_constant(body, width, 1);
        body.local_set(right);

        body.local_get(left);
        body.local_get(right);
        emit_binary(
            body,
            width,
            match rule {
                FlagRule::Addition => BinaryOperation::Add,
                _ => BinaryOperation::Subtract,
            },
        );
        body.local_set(result);

        self.write_operand(body, lifted, 0, width, result)?;
        self.emit_flags(
            body,
            rule,
            width,
            OperationValues {
                left,
                right,
                result,
            },
            false,
        );
        Ok(())
    }

    fn translate_shift(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        kind: ShiftKind,
    ) -> Result<()> {
        let width = self.destination_width(&lifted.instruction)?;
        let value_type = width.value_type();

        let value = self.temporaries.take(body, value_type);
        let count = self.temporaries.take(body, ValueType::I32);
        let result = self.temporaries.take(body, value_type);

        self.read_operand(body, lifted, 0, width)?;
        body.local_set(value);

        // x86 masks the shift count to five bits, or six for a 64-bit
        // operand, before doing anything with it.
        self.read_operand(body, lifted, 1, OperandWidth::Byte)?;
        body.i32_const(if width == OperandWidth::QuadWord {
            0x3f
        } else {
            0x1f
        });
        body.i32_and();
        body.local_set(count);

        body.local_get(value);
        if kind == ShiftKind::RightArithmetic {
            emit_sign_extend_within_carrier(body, width);
        }
        emit_shift_count(body, width, count);
        match (width.carrier(), kind) {
            (Carrier::I32, ShiftKind::Left) => body.i32_shl(),
            (Carrier::I32, ShiftKind::RightLogical) => body.i32_shr_unsigned(),
            (Carrier::I32, ShiftKind::RightArithmetic) => body.i32_shr_signed(),
            (Carrier::I64, ShiftKind::Left) => body.i64_shl(),
            (Carrier::I64, ShiftKind::RightLogical) => body.i64_shr_unsigned(),
            (Carrier::I64, ShiftKind::RightArithmetic) => body.i64_shr_signed(),
        }
        emit_mask_to_width(body, width);
        body.local_set(result);

        self.write_operand(body, lifted, 0, width, result)?;

        // A shift by zero leaves every flag untouched, so the whole flag
        // update sits behind that test.
        body.local_get(count);
        body.i32_eqz();
        body.i32_eqz();
        body.if_();
        self.emit_shift_flags(body, kind, width, value, count, result);
        body.end();
        Ok(())
    }

    /// `rol`/`ror`: the bits that leave one end arrive at the other.
    ///
    /// Unlike a shift, a rotate writes only two flags. The sign, zero,
    /// parity and adjust flags are *unaffected* — a rotate does not define
    /// them, and writing them would diverge from hardware on the next
    /// instruction that reads one.
    fn translate_rotate(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        left: bool,
    ) -> Result<()> {
        let width = self.destination_width(&lifted.instruction)?;
        let value_type = width.value_type();
        let bits = width.bits() as i32;

        let value = self.temporaries.take(body, value_type);
        let count = self.temporaries.take(body, ValueType::I32);
        let amount = self.temporaries.take(body, ValueType::I32);
        let result = self.temporaries.take(body, value_type);

        self.read_operand(body, lifted, 0, width)?;
        body.local_set(value);

        // Masked exactly as a shift's count is, five bits or six, *before*
        // being reduced modulo the operand's own width. The two are
        // different: a 16-bit rotate by 16 has a masked count of 16, which
        // is not zero — so it writes flags — and a rotate amount of zero, so
        // it moves nothing.
        self.read_operand(body, lifted, 1, OperandWidth::Byte)?;
        body.i32_const(if width == OperandWidth::QuadWord {
            0x3f
        } else {
            0x1f
        });
        body.i32_and();
        body.local_set(count);
        body.local_get(count);
        body.i32_const(bits);
        body.i32_rem_unsigned();
        body.local_set(amount);

        match width {
            // Wasm rotates the whole carrier, which is the operand exactly
            // at these two widths.
            OperandWidth::QuadWord => {
                body.local_get(value);
                body.local_get(amount);
                body.i64_extend_i32_unsigned();
                if left {
                    body.i64_rotate_left();
                } else {
                    body.i64_rotate_right();
                }
                body.local_set(result);
            }
            OperandWidth::DoubleWord => {
                body.local_get(value);
                body.local_get(amount);
                if left {
                    body.i32_rotate_left();
                } else {
                    body.i32_rotate_right();
                }
                body.local_set(result);
            }
            // Narrower than the carrier, so the wasm rotate would carry bits
            // in from the padding above the operand. Two shifts and an or,
            // with the zero case taken out: shifting a carrier by its own
            // width is masked back to a shift by zero, which would leave the
            // value where a rotate by nothing has to leave it anyway — but
            // by the wrong route, and the other half would then duplicate it.
            _ => {
                // Rotating by nothing is the identity, and it has to be
                // spelled that way rather than fall out of the arithmetic:
                // the other half would shift by the operand's whole width,
                // which wasm masks back to a shift by zero and would
                // duplicate the value instead of contributing nothing.
                body.local_get(value);
                body.local_set(result);
                body.local_get(amount);
                body.i32_eqz();
                body.i32_eqz();
                body.if_();
                body.local_get(value);
                body.local_get(amount);
                if left {
                    body.i32_shl();
                } else {
                    body.i32_shr_unsigned();
                }
                body.local_get(value);
                body.i32_const(bits);
                body.local_get(amount);
                body.i32_sub();
                if left {
                    body.i32_shr_unsigned();
                } else {
                    body.i32_shl();
                }
                body.i32_or();
                body.local_set(result);
                body.end();
            }
        }
        body.local_get(result);
        emit_mask_to_width(body, width);
        body.local_set(result);
        self.write_operand(body, lifted, 0, width, result)?;

        // A rotate by a masked count of zero writes no flag at all.
        body.local_get(count);
        body.i32_eqz();
        body.i32_eqz();
        body.if_();
        // Carry takes the bit that came round: the lowest for a left
        // rotate, the highest for a right one.
        body.local_get(result);
        if left {
            emit_low_bit(body, width);
        } else {
            emit_sign_bit(body, width);
        }
        self.state.write_flag(body, Flag::Carry);
        // Overflow is architecturally defined for a count of one only. The
        // formula is emitted for every count because that is what hardware
        // does, and because a value nobody may read is better matching than
        // invented.
        body.local_get(result);
        emit_sign_bit(body, width);
        body.local_get(result);
        if left {
            emit_low_bit(body, width);
        } else {
            emit_second_bit(body, width);
        }
        body.i32_xor();
        self.state.write_flag(body, Flag::Overflow);
        body.end();
        Ok(())
    }

    /// `bt`, `bts`, `btr`, `btc`: the selected bit goes to the carry flag,
    /// and three of the four then write it back changed.
    ///
    /// Only the register-destination form is here. With a memory
    /// destination and a *register* offset, x86 reads a bit string — the
    /// offset is signed and may reach far outside the operand named — which
    /// is a different instruction wearing the same mnemonic, and guessing at
    /// it would address the wrong byte silently.
    fn translate_bit_test(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        action: BitAction,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        if instruction.op_kind(0) != OpKind::Register && instruction.op_kind(1) == OpKind::Register
        {
            bail!(
                "`{}` addresses a bit string in memory, whose offset is not \
                 bounded by the operand; only the register form is translated",
                render(instruction)
            );
        }
        let width = self.destination_width(instruction)?;
        let value_type = width.value_type();

        let value = self.temporaries.take(body, value_type);
        let offset = self.temporaries.take(body, ValueType::I32);
        let selected = self.temporaries.take(body, value_type);

        self.read_operand(body, lifted, 0, width)?;
        body.local_set(value);
        // At the destination's width, not a byte: a shift takes its count
        // from `%cl` and this takes its offset from a full register, so
        // asking for a byte gets the register as it is rather than narrowed.
        // Then reduced modulo the operand's width, which is what makes this
        // the bounded form.
        self.read_operand(body, lifted, 1, width)?;
        match width.carrier() {
            Carrier::I32 => {
                body.i32_const(width.bits() as i32 - 1);
                body.i32_and();
            }
            Carrier::I64 => {
                body.i64_const(width.bits() as i64 - 1);
                body.i64_and();
                body.i32_wrap_i64();
            }
        }
        body.local_set(offset);

        // A one in the selected position, as the operand's own type.
        emit_constant(body, width, 1);
        emit_shift_count(body, width, offset);
        match width.carrier() {
            Carrier::I32 => body.i32_shl(),
            Carrier::I64 => body.i64_shl(),
        }
        body.local_set(selected);

        // Carry takes the bit as it was, before anything writes it back.
        body.local_get(value);
        body.local_get(selected);
        match width.carrier() {
            Carrier::I32 => body.i32_and(),
            Carrier::I64 => body.i64_and(),
        }
        emit_is_zero(body, width);
        body.i32_eqz();
        self.state.write_flag(body, Flag::Carry);

        // Every other flag is architecturally undefined here, and hardware
        // leaves them alone — so this does too.
        if action == BitAction::Read {
            return Ok(());
        }
        let result = self.temporaries.take(body, value_type);
        body.local_get(value);
        body.local_get(selected);
        match (width.carrier(), action) {
            (Carrier::I32, BitAction::Set) => body.i32_or(),
            (Carrier::I64, BitAction::Set) => body.i64_or(),
            (Carrier::I32, BitAction::Complement) => body.i32_xor(),
            (Carrier::I64, BitAction::Complement) => body.i64_xor(),
            (Carrier::I32, _) => {
                // Clear: and with the complement of the selected bit.
                body.i32_const(-1);
                body.i32_xor();
                body.i32_and();
            }
            (Carrier::I64, _) => {
                body.i64_const(-1);
                body.i64_xor();
                body.i64_and();
            }
        }
        emit_mask_to_width(body, width);
        body.local_set(result);
        self.write_operand(body, lifted, 0, width, result)
    }

    fn emit_shift_flags(
        &mut self,
        body: &mut FunctionBodyBuilder,
        kind: ShiftKind,
        width: OperandWidth,
        value: u32,
        count: u32,
        result: u32,
    ) {
        body.local_get(result);
        emit_is_zero(body, width);
        self.state.write_flag(body, Flag::Zero);

        body.local_get(result);
        emit_sign_bit(body, width);
        self.state.write_flag(body, Flag::Sign);

        body.local_get(result);
        emit_parity(body, width);
        self.state.write_flag(body, Flag::Parity);

        // The carry flag holds the last bit shifted out.
        body.local_get(value);
        match kind {
            ShiftKind::Left => {
                body.i32_const(width.bits() as i32);
                body.local_get(count);
                body.i32_sub();
            }
            _ => {
                body.local_get(count);
                body.i32_const(1);
                body.i32_sub();
            }
        }
        emit_shift_amount_to_carrier(body, width);
        match width.carrier() {
            Carrier::I32 => {
                body.i32_shr_unsigned();
                body.i32_const(1);
                body.i32_and();
            }
            Carrier::I64 => {
                body.i64_shr_unsigned();
                body.i32_wrap_i64();
                body.i32_const(1);
                body.i32_and();
            }
        }
        self.state.write_flag(body, Flag::Carry);

        // Overflow is architecturally defined only for a shift of one; the
        // rule below is what hardware computes there.
        match kind {
            ShiftKind::Left => {
                body.local_get(result);
                emit_sign_bit(body, width);
                self.state.read_flag(body, Flag::Carry);
                body.i32_xor();
            }
            ShiftKind::RightLogical => {
                body.local_get(value);
                emit_sign_bit(body, width);
            }
            ShiftKind::RightArithmetic => body.i32_const(0),
        }
        self.state.write_flag(body, Flag::Overflow);
    }

    /// The two- and three-operand forms of `imul`, which keep only the low
    /// half of the product.
    /// `cbw`/`cwde`/`cdqe`: widen the accumulator in place, signed.
    /// `xchg`: the two operands swap, and no flag moves.
    ///
    /// With a memory operand this is atomic whether or not it carries a
    /// `lock` prefix, which is why every libc uses it to release a mutex.
    /// Nothing here makes it atomic and nothing has to: the scheduler
    /// switches threads only at syscalls, so no other actor can observe the
    /// two halves apart. If that ever stops being true — preemption at
    /// arbitrary points — this is one of the places that has to change, and
    /// it is written here so the search finds it.
    fn translate_exchange(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let width = self.destination_width(&lifted.instruction)?;
        let value_type = width.value_type();
        let first = self.temporaries.take(body, value_type);
        let second = self.temporaries.take(body, value_type);

        // Both read before either is written: the operands can name the same
        // register, and a swap that reads the second one afterwards would
        // read what it had just written.
        self.read_operand(body, lifted, 0, width)?;
        body.local_set(first);
        self.read_operand(body, lifted, 1, width)?;
        body.local_set(second);

        self.write_operand(body, lifted, 0, width, second)?;
        self.write_operand(body, lifted, 1, width, first)
    }

    /// `xadd`: the destination gets the sum, the source gets the
    /// destination's old value, and the flags are an ordinary addition's.
    fn translate_exchange_and_add(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let width = self.destination_width(&lifted.instruction)?;
        let value_type = width.value_type();
        let destination = self.temporaries.take(body, value_type);
        let source = self.temporaries.take(body, value_type);
        let sum = self.temporaries.take(body, value_type);

        self.read_operand(body, lifted, 0, width)?;
        body.local_set(destination);
        self.read_operand(body, lifted, 1, width)?;
        body.local_set(source);

        body.local_get(destination);
        body.local_get(source);
        emit_binary(body, width, BinaryOperation::Add);
        body.local_set(sum);

        // The source first: it takes the destination's *old* value, and the
        // two can name the same register only if they are the same, in which
        // case x86 leaves the sum there.
        self.write_operand(body, lifted, 1, width, destination)?;
        self.write_operand(body, lifted, 0, width, sum)?;
        self.emit_flags(
            body,
            FlagRule::Addition,
            width,
            OperationValues {
                left: destination,
                right: source,
                result: sum,
            },
            true,
        );
        Ok(())
    }

    /// `cmpxchg`: compare the accumulator with the destination, and write
    /// one of them.
    ///
    /// Equal, and the source replaces the destination; unequal, and the
    /// destination replaces the accumulator. Either way the flags are those
    /// of `cmp accumulator, destination` — which is what makes `ZF` the
    /// answer to "did it take", and what every compare-and-swap loop
    /// branches on.
    fn translate_compare_and_exchange(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let width = self.destination_width(&lifted.instruction)?;
        let value_type = width.value_type();
        let destination = self.temporaries.take(body, value_type);
        let source = self.temporaries.take(body, value_type);
        let expected = self.temporaries.take(body, value_type);
        let difference = self.temporaries.take(body, value_type);

        self.read_operand(body, lifted, 0, width)?;
        body.local_set(destination);
        self.read_operand(body, lifted, 1, width)?;
        body.local_set(source);
        self.state.read_register(body, accumulator(width));
        body.local_set(expected);

        body.local_get(expected);
        body.local_get(destination);
        emit_binary(body, width, BinaryOperation::Subtract);
        body.local_set(difference);
        self.emit_flags(
            body,
            FlagRule::Subtraction,
            width,
            OperationValues {
                left: expected,
                right: destination,
                result: difference,
            },
            true,
        );

        body.local_get(expected);
        body.local_get(destination);
        emit_equal(body, width);
        body.if_();
        self.write_operand(body, lifted, 0, width, source)?;
        body.else_();
        body.local_get(destination);
        self.state.write_register(body, accumulator(width));
        body.end();
        Ok(())
    }

    fn translate_widen_accumulator(
        &mut self,
        body: &mut FunctionBodyBuilder,
        from: OperandWidth,
    ) -> Result<()> {
        let to = OperandWidth::from_bytes(from.bytes() * 2)?;
        self.state.read_register(body, accumulator(from));
        emit_widen(body, from, to, true);
        let value = self.temporaries.take(body, to.value_type());
        body.local_set(value);
        body.local_get(value);
        self.state.write_register(body, accumulator(to));
        Ok(())
    }

    /// `cwd`/`cdq`/`cqo`: fill the data register with the accumulator's sign,
    /// producing the double-width dividend a following `idiv` consumes.
    fn translate_sign_into_data_register(
        &mut self,
        body: &mut FunctionBodyBuilder,
        width: OperandWidth,
    ) -> Result<()> {
        self.state.read_register(body, accumulator(width));
        emit_sign_extend_within_carrier(body, width);
        match width.carrier() {
            Carrier::I32 => {
                body.i32_const(31);
                body.i32_shr_signed();
                emit_mask_to_width(body, width);
            }
            Carrier::I64 => {
                body.i64_const(63);
                body.i64_shr_signed();
            }
        }
        let value = self.temporaries.take(body, width.value_type());
        body.local_set(value);
        body.local_get(value);
        self.state.write_register(body, data_register(width));
        Ok(())
    }

    /// `div` and `idiv`: divide the double-width value held in the data and
    /// accumulator registers, leaving the quotient in the accumulator and the
    /// remainder in the data register.
    ///
    /// Widths up to four bytes are computed exactly in `i64`. The eight-byte
    /// form's dividend is 128 bits wide, which `i64` cannot hold; the
    /// translation handles the case compilers actually emit — a dividend
    /// produced by `cqo` or by zeroing `rdx`, so that it fits in 64 bits — and
    /// traps otherwise, where hardware would have divided successfully. That
    /// is the one place division diverges from the machine, and it is why
    /// full-width division fidelity is on the list of work after the MVP.
    fn translate_divide(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        signed: bool,
    ) -> Result<()> {
        let width = self.operand_width(&lifted.instruction, 0)?;
        let dividend = self.temporaries.take(body, ValueType::I64);
        let divisor = self.temporaries.take(body, ValueType::I64);
        let quotient = self.temporaries.take(body, ValueType::I64);

        self.emit_dividend(body, width, signed);
        body.local_set(dividend);

        self.read_operand(body, lifted, 0, width)?;
        emit_promote_to_i64(body, width, signed);
        body.local_set(divisor);

        // A zero divisor traps here exactly as it raises a divide error on
        // the machine.
        body.local_get(dividend);
        body.local_get(divisor);
        if signed {
            body.i64_div_signed();
        } else {
            body.i64_div_unsigned();
        }
        body.local_set(quotient);

        // A quotient too wide for the accumulator is a divide error too.
        if width != OperandWidth::QuadWord {
            emit_quotient_overflows(body, width, signed, quotient);
            body.if_();
            body.unreachable();
            body.end();
        }

        let remainder = self.temporaries.take(body, ValueType::I64);
        body.local_get(dividend);
        body.local_get(divisor);
        if signed {
            body.i64_rem_signed();
        } else {
            body.i64_rem_unsigned();
        }
        body.local_set(remainder);

        body.local_get(quotient);
        emit_narrow_from_i64(body, width);
        self.state.write_register(body, accumulator(width));

        body.local_get(remainder);
        emit_narrow_from_i64(body, width);
        self.state.write_register(body, remainder_register(width));

        // Every flag is architecturally undefined after a division; leaving
        // them as they were is as good as any other answer and cheaper than
        // inventing one.
        Ok(())
    }

    /// Pushes the double-width dividend as an `i64`.
    fn emit_dividend(&mut self, body: &mut FunctionBodyBuilder, width: OperandWidth, signed: bool) {
        if width == OperandWidth::QuadWord {
            // The dividend is `rdx:rax`. Only a dividend that fits in 64 bits
            // can be handled; check that `rdx` holds nothing but the sign.
            self.state.read_register(body, data_register(width));
            if signed {
                self.state.read_register(body, accumulator(width));
                body.i64_const(63);
                body.i64_shr_signed();
                body.i64_ne();
            } else {
                body.i64_eqz();
                body.i32_eqz();
            }
            body.if_();
            body.unreachable();
            body.end();
            self.state.read_register(body, accumulator(width));
            return;
        }

        // A byte-wide divide takes its whole dividend from `ax`; the wider
        // ones pair the data register with the accumulator.
        if width == OperandWidth::Byte {
            self.state
                .read_register(body, accumulator(OperandWidth::Word));
            emit_promote_to_i64(body, OperandWidth::Word, false);
        } else {
            self.state.read_register(body, data_register(width));
            emit_promote_to_i64(body, width, false);
            body.i64_const(width.bits() as i64);
            body.i64_shl();
            self.state.read_register(body, accumulator(width));
            emit_promote_to_i64(body, width, false);
            body.i64_or();
        }

        // The pair is a signed number twice the operand's width.
        if signed {
            match width {
                OperandWidth::Byte => body.i64_extend16_signed(),
                OperandWidth::Word => body.i64_extend32_signed(),
                // A 64-bit pair is already the value it denotes.
                _ => {}
            }
        }
    }

    fn translate_signed_multiply(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
    ) -> Result<()> {
        let instruction = &lifted.instruction;
        let (left_operand, right_operand) = match instruction.op_count() {
            2 => (0, 1),
            3 => (1, 2),
            count => bail!(
                "the {count}-operand form of `imul` (a widening multiply into \
                 rdx:rax) is not implemented"
            ),
        };

        let width = self.destination_width(instruction)?;
        let value_type = width.value_type();
        let left = self.temporaries.take(body, value_type);
        let right = self.temporaries.take(body, value_type);
        let result = self.temporaries.take(body, value_type);

        self.read_operand(body, lifted, left_operand, width)?;
        body.local_set(left);
        self.read_operand(body, lifted, right_operand, width)?;
        body.local_set(right);

        body.local_get(left);
        body.local_get(right);
        match width.carrier() {
            Carrier::I32 => body.i32_mul(),
            Carrier::I64 => body.i64_mul(),
        }
        emit_mask_to_width(body, width);
        body.local_set(result);

        self.write_operand(body, lifted, 0, width, result)?;
        self.emit_multiply_flags(body, width, left, right, result);
        Ok(())
    }

    /// `imul` sets carry and overflow together, both meaning "the full
    /// product did not fit". Sign, zero and parity are architecturally
    /// undefined; the translator gives them the obvious values rather than
    /// leaving them at whatever they held.
    fn emit_multiply_flags(
        &mut self,
        body: &mut FunctionBodyBuilder,
        width: OperandWidth,
        left: u32,
        right: u32,
        result: u32,
    ) {
        body.local_get(result);
        emit_is_zero(body, width);
        self.state.write_flag(body, Flag::Zero);

        body.local_get(result);
        emit_sign_bit(body, width);
        self.state.write_flag(body, Flag::Sign);

        body.local_get(result);
        emit_parity(body, width);
        self.state.write_flag(body, Flag::Parity);

        let overflowed = self.temporaries.take(body, ValueType::I32);
        match width {
            // Below 64 bits the exact product fits in an `i64`, so the test
            // is a direct comparison against the sign-extended result.
            OperandWidth::QuadWord => {
                self.emit_quad_word_multiply_overflow(body, left, right, result);
            }
            _ => {
                body.local_get(left);
                emit_sign_extend_within_carrier(body, width);
                body.i64_extend_i32_signed();
                body.local_get(right);
                emit_sign_extend_within_carrier(body, width);
                body.i64_extend_i32_signed();
                body.i64_mul();

                body.local_get(result);
                emit_sign_extend_within_carrier(body, width);
                body.i64_extend_i32_signed();
                body.i64_ne();
            }
        }
        body.local_tee(overflowed);
        self.state.write_flag(body, Flag::Carry);
        body.local_get(overflowed);
        self.state.write_flag(body, Flag::Overflow);
    }

    /// Pushes `1` when a 64-bit signed product overflowed.
    ///
    /// The product fits exactly when its high half is the sign extension of
    /// its low half.
    fn emit_quad_word_multiply_overflow(
        &mut self,
        body: &mut FunctionBodyBuilder,
        left: u32,
        right: u32,
        result: u32,
    ) {
        self.emit_quad_word_product_high(body, left, right, true);
        body.local_get(result);
        body.i64_const(63);
        body.i64_shr_signed();
        body.i64_ne();
    }

    /// Pushes the high 64 bits of the 128-bit product of two `i64` locals.
    ///
    /// Computed from four 32-bit partial products, which keeps everything
    /// inside `i64` arithmetic — no division, and nothing that can trap. The
    /// unsigned high half is the natural result; the signed one is that minus
    /// each operand masked by the other's sign, which is the standard
    /// correction and cheaper than branching on signs.
    fn emit_quad_word_product_high(
        &mut self,
        body: &mut FunctionBodyBuilder,
        left: u32,
        right: u32,
        signed: bool,
    ) {
        const LOW_MASK: i64 = 0xffff_ffff;
        let left_low = self.temporaries.take(body, ValueType::I64);
        let left_high = self.temporaries.take(body, ValueType::I64);
        let right_low = self.temporaries.take(body, ValueType::I64);
        let right_high = self.temporaries.take(body, ValueType::I64);
        let cross_one = self.temporaries.take(body, ValueType::I64);
        let cross_two = self.temporaries.take(body, ValueType::I64);
        let high = self.temporaries.take(body, ValueType::I64);

        let split = |body: &mut FunctionBodyBuilder, source: u32, low: u32, high: u32| {
            body.local_get(source);
            body.i64_const(LOW_MASK);
            body.i64_and();
            body.local_set(low);
            body.local_get(source);
            body.i64_const(32);
            body.i64_shr_unsigned();
            body.local_set(high);
        };
        split(body, left, left_low, left_high);
        split(body, right, right_low, right_high);

        // cross_one = left_high * right_low, cross_two = left_low * right_high
        body.local_get(left_high);
        body.local_get(right_low);
        body.i64_mul();
        body.local_set(cross_one);
        body.local_get(left_low);
        body.local_get(right_high);
        body.i64_mul();
        body.local_set(cross_two);

        // The unsigned high half: the top product, the two crossed carries,
        // and the carry out of the low 64 bits.
        body.local_get(left_high);
        body.local_get(right_high);
        body.i64_mul();
        body.local_get(cross_one);
        body.i64_const(32);
        body.i64_shr_unsigned();
        body.i64_add();
        body.local_get(cross_two);
        body.i64_const(32);
        body.i64_shr_unsigned();
        body.i64_add();

        body.local_get(left_low);
        body.local_get(right_low);
        body.i64_mul();
        body.i64_const(32);
        body.i64_shr_unsigned();
        body.local_get(cross_one);
        body.i64_const(LOW_MASK);
        body.i64_and();
        body.i64_add();
        body.local_get(cross_two);
        body.i64_const(LOW_MASK);
        body.i64_and();
        body.i64_add();
        body.i64_const(32);
        body.i64_shr_unsigned();
        body.i64_add();
        body.local_set(high);

        body.local_get(high);
        if signed {
            // Correct the unsigned high half into the signed one.
            body.local_get(left);
            body.i64_const(63);
            body.i64_shr_signed();
            body.local_get(right);
            body.i64_and();
            body.i64_sub();
            body.local_get(right);
            body.i64_const(63);
            body.i64_shr_signed();
            body.local_get(left);
            body.i64_and();
            body.i64_sub();
        }
    }

    /// The one-operand `mul` and `imul`: multiply the accumulator by the
    /// operand and write a result twice as wide, high half in the data
    /// register and low half in the accumulator.
    ///
    /// Below 64 bits the exact product fits in an `i64` and the two halves
    /// are just shifts of it. The 64-bit form needs the real 128-bit product,
    /// whose high half comes from the same partial-product arithmetic the
    /// overflow test uses.
    ///
    /// A byte-wide multiply is the exception to the register pairing: the
    /// whole 16-bit product lands in `ax`, with no data register involved.
    fn translate_wide_multiply(
        &mut self,
        body: &mut FunctionBodyBuilder,
        lifted: &LiftedInstruction,
        signed: bool,
    ) -> Result<()> {
        let width = self.operand_width(&lifted.instruction, 0)?;
        let quad = width == OperandWidth::QuadWord;
        let left = self.temporaries.take(body, ValueType::I64);
        let right = self.temporaries.take(body, ValueType::I64);
        // Below 64 bits this holds the *whole* product, which fits; at 64 bits
        // it holds only the low half, with `high` carrying the rest.
        let product = self.temporaries.take(body, ValueType::I64);
        let high = self.temporaries.take(body, ValueType::I64);

        self.state.read_register(body, accumulator(width));
        emit_promote_to_i64(body, width, signed);
        body.local_set(left);
        self.read_operand(body, lifted, 0, width)?;
        emit_promote_to_i64(body, width, signed);
        body.local_set(right);

        body.local_get(left);
        body.local_get(right);
        body.i64_mul();
        body.local_set(product);

        if quad {
            self.emit_quad_word_product_high(body, left, right, signed);
        } else {
            body.local_get(product);
            body.i64_const(width.bits() as i64);
            body.i64_shr_unsigned();
        }
        body.local_set(high);

        if width == OperandWidth::Byte {
            // `ax` takes the whole product, so it is written as one 16-bit
            // value rather than split across a register pair this width does
            // not use.
            body.local_get(product);
            emit_narrow_from_i64(body, OperandWidth::Word);
            self.state
                .write_register(body, accumulator(OperandWidth::Word));
        } else {
            body.local_get(product);
            emit_narrow_from_i64(body, width);
            self.state.write_register(body, accumulator(width));
            body.local_get(high);
            emit_narrow_from_i64(body, width);
            self.state.write_register(body, data_register(width));
        }

        // Carry and overflow both mean "the result needed the high half".
        // Sign, zero, parity and adjust are architecturally undefined here;
        // as with division, leaving them alone is as good an answer as
        // inventing one, and it keeps the translation from claiming to know
        // something the machine does not define.
        let overflowed = self.temporaries.take(body, ValueType::I32);
        match (signed, quad) {
            // Fits when the high half carries nothing.
            (false, _) => {
                body.local_get(high);
                body.i64_eqz();
                body.i32_eqz();
            }
            // Fits when the high half is only the low half's sign.
            (true, true) => {
                body.local_get(high);
                body.local_get(product);
                body.i64_const(63);
                body.i64_shr_signed();
                body.i64_ne();
            }
            // The exact product is in hand, so the question is directly
            // whether it survives a round trip through the narrower width.
            (true, false) => {
                let spare = 64 - width.bits() as i64;
                body.local_get(product);
                body.local_get(product);
                body.i64_const(spare);
                body.i64_shl();
                body.i64_const(spare);
                body.i64_shr_signed();
                body.i64_ne();
            }
        }
        body.local_tee(overflowed);
        self.state.write_flag(body, Flag::Carry);
        body.local_get(overflowed);
        self.state.write_flag(body, Flag::Overflow);
        Ok(())
    }

    /// Pushes `rsp` narrowed to a linear-memory address.
    fn push_stack_pointer_address(&self, body: &mut FunctionBodyBuilder) {
        self.state
            .read_register(body, RegisterSlice::quad(STACK_POINTER_REGISTER));
        body.i32_wrap_i64();
    }

    fn adjust_stack_pointer(&self, body: &mut FunctionBodyBuilder, delta: i64) {
        self.state
            .read_register(body, RegisterSlice::quad(STACK_POINTER_REGISTER));
        body.i64_const(delta);
        body.i64_add();
        self.state
            .write_register(body, RegisterSlice::quad(STACK_POINTER_REGISTER));
    }

    /// The width an instruction's destination operand works in.
    fn destination_width(&self, instruction: &Instruction) -> Result<OperandWidth> {
        self.operand_width(instruction, 0)
    }

    // ---- flags -----------------------------------------------------------

    /// Computes the four modelled flags eagerly from an operation's inputs
    /// and result, all of which are already parked in locals.
    fn emit_flags(
        &mut self,
        body: &mut FunctionBodyBuilder,
        rule: FlagRule,
        width: OperandWidth,
        values: OperationValues,
        update_carry: bool,
    ) {
        let OperationValues {
            left,
            right,
            result,
        } = values;
        // Zero flag.
        body.local_get(result);
        emit_is_zero(body, width);
        self.state.write_flag(body, Flag::Zero);

        // Sign flag.
        body.local_get(result);
        emit_sign_bit(body, width);
        self.state.write_flag(body, Flag::Sign);

        // Parity flag.
        body.local_get(result);
        emit_parity(body, width);
        self.state.write_flag(body, Flag::Parity);

        match rule {
            FlagRule::Logical => {
                body.i32_const(0);
                self.state.write_flag(body, Flag::Carry);
                body.i32_const(0);
                self.state.write_flag(body, Flag::Overflow);
            }
            FlagRule::Addition => {
                // Carry out of the top bit: the sum wrapped below its left
                // operand.
                if update_carry {
                    body.local_get(result);
                    body.local_get(left);
                    emit_less_than_unsigned(body, width);
                    self.state.write_flag(body, Flag::Carry);
                }

                // Signed overflow: both operands differ in sign from the
                // result.
                body.local_get(left);
                body.local_get(result);
                emit_xor(body, width);
                body.local_get(right);
                body.local_get(result);
                emit_xor(body, width);
                emit_and(body, width);
                emit_sign_bit(body, width);
                self.state.write_flag(body, Flag::Overflow);
            }
            FlagRule::Subtraction => {
                // Borrow: the left operand was the smaller one.
                if update_carry {
                    body.local_get(left);
                    body.local_get(right);
                    emit_less_than_unsigned(body, width);
                    self.state.write_flag(body, Flag::Carry);
                }

                // Signed overflow: the operands differ in sign and the result
                // takes the sign of the right one.
                body.local_get(left);
                body.local_get(right);
                emit_xor(body, width);
                body.local_get(left);
                body.local_get(result);
                emit_xor(body, width);
                emit_and(body, width);
                emit_sign_bit(body, width);
                self.state.write_flag(body, Flag::Overflow);
            }
        }
    }
}

/// Whether a transfer names its callee or computes it.
enum CallTarget {
    Direct(FunctionReference),
    Indirect,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShiftKind {
    Left,
    RightLogical,
    RightArithmetic,
}

/// `jcc` in any of its sixteen spellings. `loop` and `jrcxz` also branch
/// conditionally but carry no condition code, and are not implemented.
fn is_conditional_jump(instruction: &Instruction) -> bool {
    instruction.flow_control() == FlowControl::ConditionalBranch
        && instruction.condition_code() != ConditionCode::None
}

fn is_set_condition(mnemonic: Mnemonic) -> bool {
    matches!(
        mnemonic,
        Mnemonic::Seta
            | Mnemonic::Setae
            | Mnemonic::Setb
            | Mnemonic::Setbe
            | Mnemonic::Sete
            | Mnemonic::Setg
            | Mnemonic::Setge
            | Mnemonic::Setl
            | Mnemonic::Setle
            | Mnemonic::Setne
            | Mnemonic::Setno
            | Mnemonic::Setnp
            | Mnemonic::Setns
            | Mnemonic::Seto
            | Mnemonic::Setp
            | Mnemonic::Sets
    )
}

fn is_conditional_move(mnemonic: Mnemonic) -> bool {
    matches!(
        mnemonic,
        Mnemonic::Cmova
            | Mnemonic::Cmovae
            | Mnemonic::Cmovb
            | Mnemonic::Cmovbe
            | Mnemonic::Cmove
            | Mnemonic::Cmovg
            | Mnemonic::Cmovge
            | Mnemonic::Cmovl
            | Mnemonic::Cmovle
            | Mnemonic::Cmovne
            | Mnemonic::Cmovno
            | Mnemonic::Cmovnp
            | Mnemonic::Cmovns
            | Mnemonic::Cmovo
            | Mnemonic::Cmovp
            | Mnemonic::Cmovs
    )
}

/// `al`/`ax`/`eax`/`rax`, the register a division's quotient lands in.
fn accumulator(width: OperandWidth) -> RegisterSlice {
    RegisterSlice {
        number: 0,
        width,
        high_byte: false,
    }
}

/// `dl`/`dx`/`edx`/`rdx`, which holds the dividend's high half.
fn data_register(width: OperandWidth) -> RegisterSlice {
    RegisterSlice {
        number: 2,
        width,
        high_byte: false,
    }
}

/// Where a division's remainder goes: `ah` for a byte-wide divide, and the
/// data register at every other width.
fn remainder_register(width: OperandWidth) -> RegisterSlice {
    match width {
        OperandWidth::Byte => RegisterSlice {
            number: 0,
            width: OperandWidth::Byte,
            high_byte: true,
        },
        other => data_register(other),
    }
}

/// Widens a value from its carrier to `i64`.
fn emit_promote_to_i64(body: &mut FunctionBodyBuilder, width: OperandWidth, signed: bool) {
    if width == OperandWidth::QuadWord {
        return;
    }
    if signed {
        emit_sign_extend_within_carrier(body, width);
        body.i64_extend_i32_signed();
    } else {
        body.i64_extend_i32_unsigned();
    }
}

/// Narrows an `i64` back into the carrier a width travels in.
fn emit_narrow_from_i64(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    if width == OperandWidth::QuadWord {
        return;
    }
    body.i32_wrap_i64();
    emit_mask_to_width(body, width);
}

/// Pushes `1` when the `i64` quotient held in a local does not fit in
/// `width` bits — which is a divide error on the machine.
fn emit_quotient_overflows(
    body: &mut FunctionBodyBuilder,
    width: OperandWidth,
    signed: bool,
    quotient: u32,
) {
    if signed {
        // Round-tripping through the narrower width loses nothing exactly
        // when the value fits in it.
        let spare_bits = 64 - width.bits() as i64;
        body.local_get(quotient);
        body.i64_const(spare_bits);
        body.i64_shl();
        body.i64_const(spare_bits);
        body.i64_shr_signed();
        body.local_get(quotient);
        body.i64_ne();
    } else {
        body.local_get(quotient);
        body.i64_const(width.bits() as i64);
        body.i64_shr_unsigned();
        body.i64_eqz();
        body.i32_eqz();
    }
}

/// All bits of a width set, as the carrier type holds it.
fn width_all_ones(width: OperandWidth) -> i64 {
    match width {
        OperandWidth::Byte => 0xff,
        OperandWidth::Word => 0xffff,
        OperandWidth::DoubleWord => 0xffff_ffff,
        OperandWidth::QuadWord => -1,
    }
}

fn emit_constant(body: &mut FunctionBodyBuilder, width: OperandWidth, value: i64) {
    match width.carrier() {
        Carrier::I32 => body.i32_const(value as i32),
        Carrier::I64 => body.i64_const(value),
    }
}

/// Sign-extends a narrow value so that its carrier holds the same number in
/// full width — what an arithmetic operation on it needs.
fn emit_sign_extend_within_carrier(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width {
        OperandWidth::Byte => body.i32_extend8_signed(),
        OperandWidth::Word => body.i32_extend16_signed(),
        OperandWidth::DoubleWord | OperandWidth::QuadWord => {}
    }
}

/// Pushes a shift count held in an `i32` local, converted to the carrier the
/// value being shifted uses.
fn emit_shift_count(body: &mut FunctionBodyBuilder, width: OperandWidth, count: u32) {
    body.local_get(count);
    emit_shift_amount_to_carrier(body, width);
}

/// Converts an `i32` shift amount on top of the stack to the value's carrier.
fn emit_shift_amount_to_carrier(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    if width.value_type() == ValueType::I64 {
        body.i64_extend_i32_unsigned();
    }
}

#[derive(Clone, Copy)]
enum BinaryOperation {
    Add,
    Subtract,
    And,
    Or,
    Xor,
}

fn emit_binary(body: &mut FunctionBodyBuilder, width: OperandWidth, operation: BinaryOperation) {
    match (width.carrier(), operation) {
        (Carrier::I32, BinaryOperation::Add) => body.i32_add(),
        (Carrier::I32, BinaryOperation::Subtract) => body.i32_sub(),
        (Carrier::I32, BinaryOperation::And) => body.i32_and(),
        (Carrier::I32, BinaryOperation::Or) => body.i32_or(),
        (Carrier::I32, BinaryOperation::Xor) => body.i32_xor(),
        (Carrier::I64, BinaryOperation::Add) => body.i64_add(),
        (Carrier::I64, BinaryOperation::Subtract) => body.i64_sub(),
        (Carrier::I64, BinaryOperation::And) => body.i64_and(),
        (Carrier::I64, BinaryOperation::Or) => body.i64_or(),
        (Carrier::I64, BinaryOperation::Xor) => body.i64_xor(),
    }
    // Addition and subtraction can carry out of a narrow width; re-establish
    // the zero-extension invariant.
    if matches!(operation, BinaryOperation::Add | BinaryOperation::Subtract) {
        emit_mask_to_width(body, width);
    }
}

fn emit_and(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width.carrier() {
        Carrier::I32 => body.i32_and(),
        Carrier::I64 => body.i64_and(),
    }
}

fn emit_xor(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width.carrier() {
        Carrier::I32 => body.i32_xor(),
        Carrier::I64 => body.i64_xor(),
    }
}

fn emit_mask_to_width(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width {
        OperandWidth::Byte | OperandWidth::Word => {
            body.i32_const(width.mask_i32());
            body.i32_and();
        }
        _ => {}
    }
}

/// Replaces the value on top of the stack with `1` if it is zero, else `0`.
fn emit_is_zero(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width.carrier() {
        Carrier::I32 => body.i32_eqz(),
        Carrier::I64 => body.i64_eqz(),
    }
}

/// Bit zero of the value on the stack, as an `i32` of `0` or `1`.
fn emit_low_bit(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width.carrier() {
        Carrier::I32 => {
            body.i32_const(1);
            body.i32_and();
        }
        Carrier::I64 => {
            body.i32_wrap_i64();
            body.i32_const(1);
            body.i32_and();
        }
    }
}

/// The bit below the sign bit, which a right rotate's overflow rule needs.
fn emit_second_bit(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width.carrier() {
        Carrier::I32 => {
            body.i32_const(width.bits() as i32 - 2);
            body.i32_shr_unsigned();
            body.i32_const(1);
            body.i32_and();
        }
        Carrier::I64 => {
            body.i64_const(width.bits() as i64 - 2);
            body.i64_shr_unsigned();
            body.i32_wrap_i64();
            body.i32_const(1);
            body.i32_and();
        }
    }
}

/// Replaces the value on top of the stack with its sign bit, as `0` or `1`.
fn emit_sign_bit(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width.carrier() {
        Carrier::I32 => {
            body.i32_const(width.bits() as i32 - 1);
            body.i32_shr_unsigned();
            body.i32_const(1);
            body.i32_and();
        }
        Carrier::I64 => {
            body.i64_const(width.bits() as i64 - 1);
            body.i64_shr_unsigned();
            body.i32_wrap_i64();
            body.i32_const(1);
            body.i32_and();
        }
    }
}

/// Replaces the value on top of the stack with its parity flag: `1` when its
/// **low byte** has an even number of set bits.
///
/// The low byte, not the whole result, whatever the operand's width — that is
/// what the architecture says, and a compare of two wide values reports the
/// parity of only the bottom eight bits of the difference.
fn emit_parity(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    if width.carrier() == Carrier::I64 {
        body.i32_wrap_i64();
    }
    body.i32_const(0xff);
    body.i32_and();
    body.i32_popcnt();
    body.i32_const(1);
    body.i32_and();
    // Even parity sets the flag, so the low bit of the population count is
    // inverted rather than used directly.
    body.i32_eqz();
}

/// Unsigned comparison of the two values on the stack, leaving `0` or `1`.
/// Narrow values are zero-extended, so a plain unsigned compare is right.
fn emit_equal(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width.carrier() {
        Carrier::I32 => body.i32_eq(),
        Carrier::I64 => body.i64_eq(),
    }
}

fn emit_less_than_unsigned(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width.carrier() {
        Carrier::I32 => body.i32_lt_unsigned(),
        Carrier::I64 => body.i64_lt_unsigned(),
    }
}

fn emit_load(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width {
        OperandWidth::Byte => body.i32_load8_unsigned(0),
        OperandWidth::Word => body.i32_load16_unsigned(0),
        OperandWidth::DoubleWord => body.i32_load(width.alignment_log2(), 0),
        OperandWidth::QuadWord => body.i64_load(width.alignment_log2(), 0),
    }
}

fn emit_store(body: &mut FunctionBodyBuilder, width: OperandWidth) {
    match width {
        OperandWidth::Byte => body.i32_store8(0),
        OperandWidth::Word => body.i32_store16(0),
        OperandWidth::DoubleWord => body.i32_store(width.alignment_log2(), 0),
        OperandWidth::QuadWord => body.i64_store(width.alignment_log2(), 0),
    }
}

/// Widens the value on top of the stack from one width to another.
fn emit_widen(body: &mut FunctionBodyBuilder, from: OperandWidth, to: OperandWidth, signed: bool) {
    if !signed {
        // Values already travel zero-extended, so widening within the `i32`
        // carrier is a no-op; only the jump to `i64` needs an instruction.
        if to == OperandWidth::QuadWord && from != OperandWidth::QuadWord {
            body.i64_extend_i32_unsigned();
        }
        return;
    }

    match (from, to.carrier()) {
        (OperandWidth::Byte, Carrier::I32) => body.i32_extend8_signed(),
        (OperandWidth::Word, Carrier::I32) => body.i32_extend16_signed(),
        (OperandWidth::DoubleWord, Carrier::I32) => {}
        (OperandWidth::Byte, Carrier::I64) => {
            body.i64_extend_i32_unsigned();
            body.i64_extend8_signed();
        }
        (OperandWidth::Word, Carrier::I64) => {
            body.i64_extend_i32_unsigned();
            body.i64_extend16_signed();
        }
        (OperandWidth::DoubleWord, Carrier::I64) => body.i64_extend_i32_signed(),
        (OperandWidth::QuadWord, _) => {}
    }
    // A signed widening that stops short of the carrier's full width leaves
    // set bits above the destination width; clear them.
    if to != OperandWidth::QuadWord {
        emit_mask_to_width(body, to);
    }
}

fn is_immediate(kind: OpKind) -> bool {
    matches!(
        kind,
        OpKind::Immediate8
            | OpKind::Immediate8_2nd
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64
    )
}

fn mask_to_width(value: u64, width: OperandWidth) -> u64 {
    match width {
        OperandWidth::QuadWord => value,
        other => value & ((1u64 << other.bits()) - 1),
    }
}

/// Renders an instruction for diagnostics.
pub fn render(instruction: &Instruction) -> String {
    let mut formatter = IntelFormatter::new();
    formatter.options_mut().set_rip_relative_addresses(true);
    let mut text = String::new();
    formatter.format(instruction, &mut text);
    text
}
