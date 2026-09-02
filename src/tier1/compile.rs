//! One block to one wasm function.
//!
//! The contract is `targum::tier1`'s, and the semantics are `Cpu::quick`'s
//! in `targum/src/exec.rs`, over the same pre-decoded [`Quick`] the
//! interpreter reads — so an op means here exactly what it means there,
//! and an op the lowering declines is not compiled at all but handed to
//! the interpreter through a helper, one instruction at a time.
//!
//! What the function holds in locals: the guest registers it touches, the
//! `%fs` base if it needs it, and the lazy-flags record. What it stores
//! back, and when: every register it wrote and the record if it wrote one,
//! at every exit and before every helper call, so that the control block
//! is complete whenever anyone else looks at it. What it never does: model
//! a fault. An access it cannot make it declines, by handing the
//! instruction back to the interpreter unexecuted; the interpreter's own
//! rules then put the fault where it belongs.
//!
//! Addresses are `entry` plus a delta throughout — the address the block
//! is running at this time is a parameter, and the address it was compiled
//! at is only used to turn the constants iced resolved into deltas. That is
//! what lets one function serve the same bytes wherever they are mapped.

use iced_x86::Instruction;
use targum::flags::{Condition, Rule};
use targum::quick::{Address, Op, Quick, Source};
use targum::state::{Slice, Width, layout};
use targum::tier1::{
    KIND_CONTINUE, KIND_INTERPRET, STEP_SYSCALL, STEP_TRAPPED,
};

use crate::emitter::ValueType;
use crate::emitter::code::{FunctionBody, FunctionBodyBuilder, FunctionReference};
use super::sweep::{Candidate, decode_instructions};

/// The functions compiled code calls: the engine's three helpers, as the
/// object imported them, and the two permission checks the object defines
/// once and every block shares.
#[derive(Clone, Copy)]
pub struct Helpers {
    /// `targum_step(tcb: i32, position: i32) -> i32`
    pub step: FunctionReference,
    /// `targum_condition(tcb: i32, condition: i32) -> i32`
    pub condition: FunctionReference,
    /// `targum_code_write(address: i64, length: i32)`
    pub code_write: FunctionReference,
    /// `check_read(vitals: i32, address: i64, width: i32) -> i32`: one if
    /// every byte of the access is readable.
    pub check_read: FunctionReference,
    /// `check_write(vitals: i32, address: i64, width: i32) -> i32`: bit
    /// zero if every byte is writable, bit one if any page holds code.
    pub check_write: FunctionReference,
}

/// The body of one permission check, shared by every block in the object.
///
/// Inline, the same test was over a hundred bytes of wasm per memory
/// operand and most instructions have one; as a call it is a dozen. What
/// it tests is `Space::permitted`: the page of the first byte and, when
/// different, the page of the last, each against the bitmap's length at
/// full width and then its bit. An address above four gigabytes wrapped
/// first would alias a small page, which is why the length compare is
/// done in 64 bits.
pub fn check_body(write: bool) -> FunctionBody {
    const VITALS_P: u32 = 0;
    const ADDRESS: u32 = 1;
    const WIDTH: u32 = 2;
    let mut body = FunctionBodyBuilder::new(3);
    let page = body.declare_local(ValueType::I64);
    let last = body.declare_local(ValueType::I64);
    let answer = body.declare_local(ValueType::I32);
    let (map, words) = match write {
        false => (VITALS_READABLE, VITALS_READABLE_WORDS),
        true => (VITALS_WRITABLE, VITALS_WRITABLE_WORDS),
    };
    // One page's permission bit, or zero past the bitmap's end.
    let permitted = |body: &mut FunctionBodyBuilder, page: u32, map: u32, words: u32| {
        // (page >> 6 < words) && (map[page >> 6] >> (page & 63)) & 1
        body.local_get(page);
        body.i64_const(6);
        body.i64_shr_unsigned();
        body.local_get(VITALS_P);
        body.i32_load(2, words);
        body.i64_extend_i32_unsigned();
        body.i64_lt_unsigned();
        body.if_();
        body.local_get(VITALS_P);
        body.i32_load(2, map);
        body.local_get(page);
        body.i64_const(6);
        body.i64_shr_unsigned();
        body.i32_wrap_i64();
        body.i32_const(3);
        body.i32_shl();
        body.i32_add();
        body.i64_load(3, 0);
        body.local_get(page);
        body.i64_const(63);
        body.i64_and();
        body.i64_shr_unsigned();
        body.i64_const(1);
        body.i64_and();
        body.i32_wrap_i64();
        body.local_set(answer);
        body.else_();
        body.i32_const(0);
        body.local_set(answer);
        body.end();
    };
    body.local_get(ADDRESS);
    body.i64_const(12);
    body.i64_shr_unsigned();
    body.local_set(page);
    body.local_get(ADDRESS);
    body.local_get(WIDTH);
    body.i64_extend_i32_unsigned();
    body.i64_add();
    body.i64_const(1);
    body.i64_sub();
    body.i64_const(12);
    body.i64_shr_unsigned();
    body.local_set(last);
    // Not permitted on the first page: answer zero.
    permitted(&mut body, page, map, words);
    body.local_get(answer);
    body.i32_eqz();
    body.if_();
    body.i32_const(0);
    body.return_();
    body.end();
    // A different last page, not permitted: answer zero.
    body.local_get(last);
    body.local_get(page);
    body.i64_ne();
    body.if_();
    permitted(&mut body, last, map, words);
    body.local_get(answer);
    body.i32_eqz();
    body.if_();
    body.i32_const(0);
    body.return_();
    body.end();
    body.end();
    if !write {
        body.i32_const(1);
        body.return_();
        body.finish()
    } else {
        // Permitted; now whether either page holds code, in bit one.
        permitted(&mut body, page, VITALS_CODE, VITALS_CODE_WORDS);
        body.local_get(answer);
        body.i32_const(1);
        body.i32_shl();
        body.i32_const(1);
        body.i32_or();
        body.local_set(answer);
        body.local_get(last);
        body.local_get(page);
        body.i64_ne();
        body.if_();
        body.local_get(answer);
        body.local_set(WIDTH); // kept aside; `answer` is reused below
        permitted(&mut body, last, VITALS_CODE, VITALS_CODE_WORDS);
        body.local_get(answer);
        body.i32_const(1);
        body.i32_shl();
        body.local_get(WIDTH);
        body.i32_or();
        body.local_set(answer);
        body.end();
        body.local_get(answer);
        body.return_();
        body.finish()
    }
}

/// A compiled block.
pub struct Compiled {
    pub body: FunctionBody,
    pub instructions: usize,
    /// How many of its instructions went through the helper.
    pub deferred: usize,
}

// The parameters.
const TCB: u32 = 0;
const VITALS: u32 = 1;
const ENTRY: u32 = 2;
const BUDGET: u32 = 3;
// The locals, declared in this order.
const REGISTER_BASE: u32 = 4;
const FS: u32 = 20;
const RULE: u32 = 21;
const WIDTH: u32 = 22;
const LEFT: u32 = 23;
const RIGHT: u32 = 24;
const RESULT: u32 = 25;
const CARRY_IN: u32 = 26;
const RIP: u32 = 27;
const KIND: u32 = 28;
const DONE: u32 = 29;
const A: u32 = 30;
const V: u32 = 31;
const T: u32 = 32;
const T2: u32 = 33;
const Q: u32 = 34;
const Q2: u32 = 35;
/// A branch target held across a stack write, whose checks use `T`.
const TARGET: u32 = 36;
/// An arithmetic op's operands, read before the record is touched: an
/// operand read that declines must leave the flags as they were.
const OP_LEFT: u32 = 37;
const OP_RIGHT: u32 = 38;

/// The vitals' layout — see `targum::space::Vitals`.
const VITALS_READABLE: u32 = 0;
const VITALS_READABLE_WORDS: u32 = 4;
const VITALS_WRITABLE: u32 = 8;
const VITALS_WRITABLE_WORDS: u32 = 12;
const VITALS_CODE: u32 = 16;
const VITALS_CODE_WORDS: u32 = 20;

const STACK_POINTER: u8 = 4;

/// Which register slots a block reads or writes, and whether it needs
/// `%fs`. Everything used is loaded once at entry; everything written is
/// stored at exit; everything used is flushed and reloaded around a helper.
#[derive(Default)]
struct Usage {
    used: [bool; 16],
    written: [bool; 16],
    fs: bool,
    flags: bool,
}

impl Usage {
    fn touch(&mut self, source: Source, writes: bool) {
        if let Source::Register(slice) = source {
            self.used[slice.number as usize] = true;
            if writes {
                self.written[slice.number as usize] = true;
            }
        }
    }

    fn stack(&mut self) {
        self.used[STACK_POINTER as usize] = true;
        self.written[STACK_POINTER as usize] = true;
    }

    fn of(quicks: &[Quick]) -> Self {
        let mut usage = Usage::default();
        for quick in quicks {
            match quick.op {
                Op::General => {}
                Op::Nop | Op::Jcc => {}
                Op::Mov | Op::Lea | Op::Widen | Op::WidenSigned => {
                    usage.touch(quick.source, false);
                    usage.touch(quick.destination, true);
                }
                Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor => {
                    usage.touch(quick.source, false);
                    usage.touch(quick.destination, true);
                    usage.flags = true;
                }
                Op::Cmp | Op::Test => {
                    usage.touch(quick.source, false);
                    usage.touch(quick.destination, false);
                    usage.flags = true;
                }
                Op::Push => {
                    usage.touch(quick.source, false);
                    usage.stack();
                }
                Op::Pop => {
                    usage.touch(quick.destination, true);
                    usage.stack();
                }
                Op::Jmp => usage.touch(quick.source, false),
                Op::Call | Op::Ret => {
                    usage.touch(quick.source, false);
                    usage.stack();
                }
            }
            if let Address::Computed { base, index, .. } = quick.address {
                for slice in [base, index].into_iter().flatten() {
                    usage.used[slice.number as usize] = true;
                }
            }
            if quick.segmented {
                usage.fs = true;
            }
        }
        usage
    }
}

/// What the compiler knows about the flags at a point in the block: the
/// operation that wrote them last, if it was one of ours.
type Known = Option<(Rule, Width)>;

struct Emitter<'a> {
    body: FunctionBodyBuilder,
    helpers: Helpers,
    /// The address the block was compiled at, for turning constants into
    /// deltas from `entry`.
    base: u64,
    usage: &'a Usage,
    /// How many `if`s are open, which is the branch depth to the exit block.
    depth: u32,
    /// Compiled instructions retired since the last helper flush.
    done: i64,
    known: Known,
}

/// Compiles one candidate.
pub fn compile(candidate: &Candidate, helpers: Helpers) -> Option<Compiled> {
    let instructions = decode_instructions(candidate);
    if instructions.is_empty() {
        return None;
    }
    let quicks: Vec<Quick> = instructions.iter().map(Quick::lower).collect();
    let usage = Usage::of(&quicks);
    let mut body = FunctionBodyBuilder::new(4);
    for _ in 0..16 {
        body.declare_local(ValueType::I64); // the registers
    }
    body.declare_local(ValueType::I64); // FS
    body.declare_local(ValueType::I32); // RULE
    body.declare_local(ValueType::I32); // WIDTH
    for _ in 0..4 {
        body.declare_local(ValueType::I64); // LEFT RIGHT RESULT CARRY_IN
    }
    for _ in 0..7 {
        body.declare_local(ValueType::I64); // RIP KIND DONE A V T T2
    }
    body.declare_local(ValueType::I32); // Q
    body.declare_local(ValueType::I32); // Q2
    body.declare_local(ValueType::I64); // TARGET
    body.declare_local(ValueType::I64); // OP_LEFT
    body.declare_local(ValueType::I64); // OP_RIGHT

    let mut emitter = Emitter {
        body,
        helpers,
        base: candidate.address,
        usage: &usage,
        depth: 0,
        done: 0,
        known: None,
    };
    let deferred = quicks.iter().filter(|quick| quick.op == Op::General).count();
    emitter.function(&instructions, &quicks);
    Some(Compiled {
        body: emitter.body.finish(),
        instructions: instructions.len(),
        deferred,
    })
}

impl Emitter<'_> {
    fn function(&mut self, instructions: &[Instruction], quicks: &[Quick]) {
        let count = instructions.len() as i64;
        let end = instructions.last().map_or(self.base, Instruction::next_ip);

        // The exit block: every way out branches to it with RIP, KIND and
        // DONE set, and the epilogue below it writes the world back.
        self.body.block();

        // The budget rule: a block that would overrun the quantum is not
        // run; the interpreter finishes the quantum and stops exactly
        // where it always stops. Straight out, *not* through the exit
        // block: nothing has been loaded yet, and the epilogue would store
        // sixteen zeros over the registers. `KIND_CONTINUE` is zero, so
        // the answer is the entry itself.
        self.body.local_get(BUDGET);
        self.body.i64_const(count);
        self.body.i64_lt_unsigned();
        self.body.if_();
        self.body.local_get(ENTRY);
        self.body.return_();
        self.body.end();

        self.prologue();

        for (position, (instruction, quick)) in instructions.iter().zip(quicks).enumerate() {
            self.instruction(position, instruction, quick, instructions);
        }

        // Fell out of the last instruction.
        self.set_rip_delta(end);
        self.leave(KIND_CONTINUE, self.done);
        self.body.end();

        self.epilogue();
    }

    // ---- the frame ------------------------------------------------------

    fn prologue(&mut self) {
        for number in 0..16u32 {
            if self.usage.used[number as usize] {
                self.body.local_get(TCB);
                self.body.i64_load(3, layout::REGISTERS + number * 8);
                self.body.local_set(REGISTER_BASE + number);
            }
        }
        if self.usage.fs {
            self.body.local_get(TCB);
            self.body.i64_load(3, layout::FS_BASE);
            self.body.local_set(FS);
        }
        if self.usage.flags {
            self.load_flags();
        }
    }

    /// Stores the world back and returns.
    fn epilogue(&mut self) {
        for number in 0..16u32 {
            if self.usage.written[number as usize] {
                self.body.local_get(TCB);
                self.body.local_get(REGISTER_BASE + number);
                self.body.i64_store(3, layout::REGISTERS + number * 8);
            }
        }
        if self.usage.flags {
            self.store_flags();
        }
        // retired += DONE
        self.body.local_get(TCB);
        self.body.local_get(TCB);
        self.body.i64_load(3, layout::RETIRED);
        self.body.local_get(DONE);
        self.body.i64_add();
        self.body.i64_store(3, layout::RETIRED);
        // rip = RIP
        self.body.local_get(TCB);
        self.body.local_get(RIP);
        self.body.i64_store(3, layout::RIP);
        // (KIND << 32) | RIP
        self.body.local_get(KIND);
        self.body.i64_const(32);
        self.body.i64_shl();
        self.body.local_get(RIP);
        self.body.i64_or();
        self.body.return_();
    }

    fn load_flags(&mut self) {
        self.body.local_get(TCB);
        self.body.i32_load8_unsigned(layout::FLAGS_RULE);
        self.body.local_set(RULE);
        self.body.local_get(TCB);
        self.body.i32_load8_unsigned(layout::FLAGS_WIDTH);
        self.body.local_set(WIDTH);
        for (local, offset) in [
            (LEFT, layout::FLAGS_LEFT),
            (RIGHT, layout::FLAGS_RIGHT),
            (RESULT, layout::FLAGS_RESULT),
            (CARRY_IN, layout::FLAGS_CARRY_IN),
        ] {
            self.body.local_get(TCB);
            self.body.i64_load(3, offset);
            self.body.local_set(local);
        }
    }

    fn store_flags(&mut self) {
        self.body.local_get(TCB);
        self.body.local_get(RULE);
        self.body.i32_store8(layout::FLAGS_RULE);
        self.body.local_get(TCB);
        self.body.local_get(WIDTH);
        self.body.i32_store8(layout::FLAGS_WIDTH);
        for (local, offset) in [
            (LEFT, layout::FLAGS_LEFT),
            (RIGHT, layout::FLAGS_RIGHT),
            (RESULT, layout::FLAGS_RESULT),
            (CARRY_IN, layout::FLAGS_CARRY_IN),
        ] {
            self.body.local_get(TCB);
            self.body.local_get(local);
            self.body.i64_store(3, offset);
        }
    }

    /// Branches to the exit block with a kind and a retired count; `RIP`
    /// must already be set.
    fn leave(&mut self, kind: u64, done: i64) {
        self.body.i64_const(kind as i64);
        self.body.local_set(KIND);
        self.body.i64_const(done);
        self.body.local_set(DONE);
        self.body.branch(self.depth);
    }

    /// `RIP = entry + (address - base)`.
    fn set_rip_delta(&mut self, address: u64) {
        self.push_delta(address);
        self.body.local_set(RIP);
    }

    /// Pushes `entry + (address - base)`.
    fn push_delta(&mut self, address: u64) {
        self.body.local_get(ENTRY);
        self.body.i64_const(address.wrapping_sub(self.base) as i64);
        self.body.i64_add();
    }

    /// Hands this instruction back to the interpreter, unexecuted.
    fn decline(&mut self, instruction: &Instruction) {
        self.set_rip_delta(instruction.ip());
        self.leave(KIND_INTERPRET, self.done);
    }

    // ---- registers ------------------------------------------------------

    fn reg_get(&mut self, slice: Slice) {
        self.body.local_get(REGISTER_BASE + u32::from(slice.number));
        if slice.high_byte {
            self.body.i64_const(8);
            self.body.i64_shr_unsigned();
        }
        if slice.width != Width::Qword {
            self.body.i64_const(slice.width.mask() as i64);
            self.body.i64_and();
        }
    }

    /// Writes the value on the stack into a slice, with x86's rule: a
    /// four-byte write zeroes the top, narrower writes leave the rest.
    fn reg_set(&mut self, slice: Slice) {
        let register = REGISTER_BASE + u32::from(slice.number);
        match (slice.width, slice.high_byte) {
            (Width::Qword, _) => self.body.local_set(register),
            (Width::Dword, _) => {
                self.body.i64_const(0xffff_ffff);
                self.body.i64_and();
                self.body.local_set(register);
            }
            (width, high_byte) => {
                let shift = if high_byte { 8 } else { 0 };
                let field = width.mask() << shift;
                self.body.local_set(V);
                self.body.local_get(register);
                self.body.i64_const(!field as i64);
                self.body.i64_and();
                self.body.local_get(V);
                self.body.i64_const(width.mask() as i64);
                self.body.i64_and();
                if shift != 0 {
                    self.body.i64_const(shift);
                    self.body.i64_shl();
                }
                self.body.i64_or();
                self.body.local_set(register);
            }
        }
    }

    // ---- addresses and memory -------------------------------------------

    /// Pushes the address a memory operand names.
    fn address(&mut self, quick: &Quick) {
        match quick.address {
            Address::Fixed(at) => self.push_delta(at),
            Address::Computed {
                displacement,
                base,
                index,
                scale,
                narrow,
            } => {
                self.body.i64_const(displacement as i64);
                if let Some(base) = base {
                    self.reg_get(base);
                    self.body.i64_add();
                }
                if let Some(index) = index {
                    self.reg_get(index);
                    if scale != 1 {
                        self.body.i64_const(i64::from(scale));
                        self.body.i64_mul();
                    }
                    self.body.i64_add();
                }
                if narrow {
                    self.body.i64_const(0xffff_ffff);
                    self.body.i64_and();
                }
            }
        }
        if quick.segmented {
            self.body.local_get(FS);
            self.body.i64_add();
        }
    }

    /// Checks that `width` bytes at the address in `A` may be accessed,
    /// declining the instruction if not. For a write, also leaves in `Q2`
    /// whether any page holds code. One call into the object's shared
    /// check; see [`check_body`].
    fn check(&mut self, write: bool, width: Width, instruction: &Instruction) {
        self.body.local_get(VITALS);
        self.body.local_get(A);
        self.body.i32_const(i32::from(width.bytes() as u8));
        self.body.call(match write {
            false => self.helpers.check_read,
            true => self.helpers.check_write,
        });
        self.body.local_set(Q);
        self.body.local_get(Q);
        self.body.i32_const(1);
        self.body.i32_and();
        self.body.i32_eqz();
        self.body.if_();
        self.depth += 1;
        self.decline(instruction);
        self.depth -= 1;
        self.body.end();
        if write {
            self.body.local_get(Q);
            self.body.i32_const(1);
            self.body.i32_shr_unsigned();
            self.body.local_set(Q2);
        }
    }

    /// Pushes the value at the address in `A`, zero-extended.
    fn load(&mut self, width: Width) {
        self.body.local_get(A);
        self.body.i32_wrap_i64();
        match width {
            Width::Byte => {
                self.body.i32_load8_unsigned(0);
                self.body.i64_extend_i32_unsigned();
            }
            Width::Word => {
                self.body.i32_load16_unsigned(0);
                self.body.i64_extend_i32_unsigned();
            }
            Width::Dword => {
                self.body.i32_load(0, 0);
                self.body.i64_extend_i32_unsigned();
            }
            Width::Qword => self.body.i64_load(0, 0),
        }
    }

    /// Stores the value in `V` at the address in `A`, and — if the page
    /// held code, which `check` left in `Q2` — reports it and leaves so
    /// that the next instruction is fetched from current bytes.
    fn store(&mut self, width: Width, next: u64, done_after: i64) {
        self.body.local_get(A);
        self.body.i32_wrap_i64();
        self.body.local_get(V);
        match width {
            Width::Byte => {
                self.body.i32_wrap_i64();
                self.body.i32_store8(0);
            }
            Width::Word => {
                self.body.i32_wrap_i64();
                self.body.i32_store16(0);
            }
            Width::Dword => {
                self.body.i32_wrap_i64();
                self.body.i32_store(0, 0);
            }
            Width::Qword => self.body.i64_store(0, 0),
        }
        self.body.local_get(Q2);
        self.body.if_();
        self.depth += 1;
        self.body.local_get(A);
        self.body.i32_const(i32::from(width.bytes() as u8));
        self.body.call(self.helpers.code_write);
        self.set_rip_delta(next);
        self.leave(KIND_CONTINUE, done_after);
        self.depth -= 1;
        self.body.end();
    }

    /// Pushes an operand's value at a width.
    fn read(&mut self, quick: &Quick, source: Source, width: Width, instruction: &Instruction) {
        match source {
            Source::Register(slice) => self.reg_get(slice),
            Source::Immediate(value) => self.body.i64_const(value as i64),
            Source::Memory => {
                self.address(quick);
                self.body.local_set(A);
                self.check(false, width, instruction);
                self.load(width);
            }
        }
    }

    /// Writes the value on the stack to an operand at a width.
    fn write(
        &mut self,
        quick: &Quick,
        into: Source,
        width: Width,
        instruction: &Instruction,
        done_after: i64,
    ) {
        match into {
            Source::Register(slice) => self.reg_set(slice),
            Source::Memory => {
                self.body.local_set(V);
                self.address(quick);
                self.body.local_set(A);
                self.check(true, width, instruction);
                self.store(width, instruction.next_ip(), done_after);
            }
            Source::Immediate(_) => unreachable!("an immediate is never a destination"),
        }
    }

    // ---- flags ----------------------------------------------------------

    /// Records a flag-writing operation: `LEFT`, `RIGHT` and `RESULT` are
    /// already set.
    fn record(&mut self, rule: Rule, width: Width) {
        self.body.i32_const(i32::from(rule as u8));
        self.body.local_set(RULE);
        self.body.i32_const(i32::from(width as u8));
        self.body.local_set(WIDTH);
        self.body.i64_const(0);
        self.body.local_set(CARRY_IN);
        self.known = Some((rule, width));
    }

    /// Pushes whether a condition holds, as an `i32`.
    fn condition(&mut self, condition: Condition) {
        let Some((rule, width)) = self.known else {
            // Fed by something before this block, or by a helper: ask the
            // interpreter, with the record it can see.
            if self.usage.flags {
                self.store_flags();
            }
            self.body.local_get(TCB);
            self.body.i32_const(i32::from(condition as u8));
            self.body.call(self.helpers.condition);
            return;
        };
        let sign = width.sign_bit() as i64;
        let zero = |body: &mut FunctionBodyBuilder| {
            body.local_get(RESULT);
            body.i64_eqz();
        };
        let sign_flag = |body: &mut FunctionBodyBuilder| {
            body.local_get(RESULT);
            body.i64_const(sign);
            body.i64_and();
            body.i64_const(0);
            body.i64_ne();
        };
        let carry = |body: &mut FunctionBodyBuilder| match rule {
            Rule::Add => {
                body.local_get(RESULT);
                body.local_get(LEFT);
                body.i64_lt_unsigned();
            }
            Rule::Sub => {
                body.local_get(LEFT);
                body.local_get(RIGHT);
                body.i64_lt_unsigned();
            }
            _ => body.i32_const(0),
        };
        let overflow = |body: &mut FunctionBodyBuilder| match rule {
            Rule::Add => {
                body.local_get(LEFT);
                body.local_get(RESULT);
                body.i64_xor();
                body.local_get(RIGHT);
                body.local_get(RESULT);
                body.i64_xor();
                body.i64_and();
                body.i64_const(sign);
                body.i64_and();
                body.i64_const(0);
                body.i64_ne();
            }
            Rule::Sub => {
                body.local_get(LEFT);
                body.local_get(RIGHT);
                body.i64_xor();
                body.local_get(LEFT);
                body.local_get(RESULT);
                body.i64_xor();
                body.i64_and();
                body.i64_const(sign);
                body.i64_and();
                body.i64_const(0);
                body.i64_ne();
            }
            _ => body.i32_const(0),
        };
        let parity = |body: &mut FunctionBodyBuilder| {
            body.local_get(RESULT);
            body.i64_const(0xff);
            body.i64_and();
            body.i64_popcnt();
            body.i64_const(1);
            body.i64_and();
            body.i64_eqz();
        };
        let body = &mut self.body;
        match condition {
            Condition::Overflow => overflow(body),
            Condition::NoOverflow => {
                overflow(body);
                body.i32_eqz();
            }
            Condition::Below => carry(body),
            Condition::AboveOrEqual => {
                carry(body);
                body.i32_eqz();
            }
            Condition::Equal => zero(body),
            Condition::NotEqual => {
                zero(body);
                body.i32_eqz();
            }
            Condition::BelowOrEqual => {
                carry(body);
                zero(body);
                body.i32_or();
            }
            Condition::Above => {
                carry(body);
                zero(body);
                body.i32_or();
                body.i32_eqz();
            }
            Condition::Sign => sign_flag(body),
            Condition::NoSign => {
                sign_flag(body);
                body.i32_eqz();
            }
            Condition::Parity => parity(body),
            Condition::NoParity => {
                parity(body);
                body.i32_eqz();
            }
            Condition::Less => {
                sign_flag(body);
                overflow(body);
                body.i32_xor();
            }
            Condition::GreaterOrEqual => {
                sign_flag(body);
                overflow(body);
                body.i32_xor();
                body.i32_eqz();
            }
            Condition::LessOrEqual => {
                zero(body);
                sign_flag(body);
                overflow(body);
                body.i32_xor();
                body.i32_or();
            }
            Condition::Greater => {
                zero(body);
                sign_flag(body);
                overflow(body);
                body.i32_xor();
                body.i32_or();
                body.i32_eqz();
            }
        }
    }

    // ---- the helper -----------------------------------------------------

    /// Runs an instruction the lowering declined through the interpreter.
    fn defer(&mut self, position: usize, instruction: &Instruction) {
        // Flush: registers, the record, and what has retired so far.
        for number in 0..16u32 {
            if self.usage.used[number as usize] {
                self.body.local_get(TCB);
                self.body.local_get(REGISTER_BASE + number);
                self.body.i64_store(3, layout::REGISTERS + number * 8);
            }
        }
        if self.usage.flags {
            self.store_flags();
        }
        if self.done != 0 {
            self.body.local_get(TCB);
            self.body.local_get(TCB);
            self.body.i64_load(3, layout::RETIRED);
            self.body.i64_const(self.done);
            self.body.i64_add();
            self.body.i64_store(3, layout::RETIRED);
            self.done = 0;
        }
        // The call.
        self.body.local_get(TCB);
        self.body.i32_const(position as i32);
        self.body.call(self.helpers.step);
        self.body.local_set(Q);
        // Reload: the helper may have changed anything.
        for number in 0..16u32 {
            if self.usage.used[number as usize] {
                self.body.local_get(TCB);
                self.body.i64_load(3, layout::REGISTERS + number * 8);
                self.body.local_set(REGISTER_BASE + number);
            }
        }
        if self.usage.fs {
            self.body.local_get(TCB);
            self.body.i64_load(3, layout::FS_BASE);
            self.body.local_set(FS);
        }
        if self.usage.flags {
            self.load_flags();
        }
        self.known = None;
        // Anything but "fell through" leaves, at wherever the helper put
        // `rip`: a syscall, a branch it took, or a trap it backed out of.
        self.body.local_get(Q);
        self.body.if_();
        self.depth += 1;
        self.body.local_get(TCB);
        self.body.i64_load(3, layout::RIP);
        self.body.local_set(RIP);
        // KIND = (Q == SYSCALL) | ((Q == TRAPPED) << 1)
        self.body.local_get(Q);
        self.body.i32_const(STEP_SYSCALL as i32);
        self.body.i32_eq();
        self.body.i64_extend_i32_unsigned();
        self.body.local_get(Q);
        self.body.i32_const(STEP_TRAPPED as i32);
        self.body.i32_eq();
        self.body.i64_extend_i32_unsigned();
        self.body.i64_const(1);
        self.body.i64_shl();
        self.body.i64_or();
        self.body.local_set(KIND);
        self.body.i64_const(0);
        self.body.local_set(DONE);
        self.body.branch(self.depth);
        self.depth -= 1;
        self.body.end();
        let _ = instruction;
    }

    // ---- one instruction ------------------------------------------------

    fn instruction(
        &mut self,
        position: usize,
        instruction: &Instruction,
        quick: &Quick,
        all: &[Instruction],
    ) {
        let _ = all;
        let width = quick.width;
        let next = instruction.next_ip();
        let after = self.done + 1;
        match quick.op {
            Op::General => {
                self.defer(position, instruction);
                return;
            }
            Op::Nop => {}
            Op::Mov => {
                self.read(quick, quick.source, width, instruction);
                self.write(quick, quick.destination, width, instruction, after);
            }
            Op::Lea => {
                self.address(quick);
                self.write(quick, quick.destination, width, instruction, after);
            }
            Op::Widen | Op::WidenSigned => {
                self.read(quick, quick.source, quick.source_width, instruction);
                if quick.op == Op::WidenSigned && quick.source_width != Width::Qword {
                    let shift = 64 - i64::from(quick.source_width.bits());
                    self.body.i64_const(shift);
                    self.body.i64_shl();
                    self.body.i64_const(shift);
                    self.body.i64_shr_signed();
                }
                self.write(quick, quick.destination, width, instruction, after);
            }
            Op::Add | Op::Sub | Op::Cmp | Op::And | Op::Or | Op::Xor | Op::Test => {
                // Both operands before the record is touched: a read that
                // declines hands the instruction back with the flags as
                // they were, which the record's locals must still be.
                self.read(quick, quick.destination, width, instruction);
                self.body.local_set(OP_LEFT);
                self.read(quick, quick.source, width, instruction);
                self.body.local_set(OP_RIGHT);
                self.body.local_get(OP_LEFT);
                self.body.local_set(LEFT);
                self.body.local_get(OP_RIGHT);
                self.body.local_set(RIGHT);
                self.body.local_get(LEFT);
                self.body.local_get(RIGHT);
                match quick.op {
                    Op::Add => self.body.i64_add(),
                    Op::Sub | Op::Cmp => self.body.i64_sub(),
                    Op::Or => self.body.i64_or(),
                    Op::Xor => self.body.i64_xor(),
                    _ => self.body.i64_and(),
                }
                if width != Width::Qword {
                    self.body.i64_const(width.mask() as i64);
                    self.body.i64_and();
                }
                self.body.local_set(RESULT);
                self.record(quick.rule(), width);
                if quick.writes_back() {
                    self.body.local_get(RESULT);
                    self.write(quick, quick.destination, width, instruction, after);
                }
            }
            Op::Push => {
                self.read(quick, quick.source, width, instruction);
                self.body.local_set(V);
                self.body.local_get(REGISTER_BASE + u32::from(STACK_POINTER));
                self.body.i64_const(i64::from(width.bytes()));
                self.body.i64_sub();
                self.body.local_set(A);
                self.check(true, width, instruction);
                // The stack pointer moves once the store is known to be
                // possible, and before the store's own exit reads it.
                self.body.local_get(A);
                self.body.local_set(REGISTER_BASE + u32::from(STACK_POINTER));
                self.store(width, next, after);
            }
            Op::Pop => {
                self.body.local_get(REGISTER_BASE + u32::from(STACK_POINTER));
                self.body.local_set(A);
                self.check(false, width, instruction);
                self.load(width);
                self.body.local_set(V);
                self.body.local_get(A);
                self.body.i64_const(i64::from(width.bytes()));
                self.body.i64_add();
                self.body.local_set(REGISTER_BASE + u32::from(STACK_POINTER));
                self.body.local_get(V);
                self.write(quick, quick.destination, width, instruction, after);
            }
            Op::Jcc => {
                let Source::Immediate(target) = quick.source else {
                    unreachable!("a conditional branch target is always an immediate")
                };
                self.condition(quick.condition);
                self.body.if_();
                self.depth += 1;
                self.set_rip_delta(target);
                self.leave(KIND_CONTINUE, after);
                self.depth -= 1;
                self.body.end();
            }
            Op::Jmp => {
                match quick.source {
                    Source::Immediate(target) => self.set_rip_delta(target),
                    source => {
                        self.read(quick, source, Width::Qword, instruction);
                        self.body.local_set(RIP);
                    }
                }
                self.leave(KIND_CONTINUE, after);
                return;
            }
            Op::Call => {
                // The target before the push: an indirect call through
                // memory can name the slot the push is about to move.
                match quick.source {
                    Source::Immediate(target) => self.push_delta(target),
                    source => self.read(quick, source, Width::Qword, instruction),
                }
                self.body.local_set(TARGET);
                self.push_delta(next);
                self.body.local_set(V);
                self.body.local_get(REGISTER_BASE + u32::from(STACK_POINTER));
                self.body.i64_const(8);
                self.body.i64_sub();
                self.body.local_set(A);
                self.check(true, Width::Qword, instruction);
                self.body.local_get(A);
                self.body.local_set(REGISTER_BASE + u32::from(STACK_POINTER));
                self.body.local_get(TARGET);
                self.body.local_set(RIP);
                // A return address written onto a code page is a store
                // like any other: report it and go where the call went.
                self.body.local_get(A);
                self.body.i32_wrap_i64();
                self.body.local_get(V);
                self.body.i64_store(0, 0);
                self.body.local_get(Q2);
                self.body.if_();
                self.depth += 1;
                self.body.local_get(A);
                self.body.i32_const(8);
                self.body.call(self.helpers.code_write);
                self.depth -= 1;
                self.body.end();
                self.leave(KIND_CONTINUE, after);
                return;
            }
            Op::Ret => {
                let extra = match quick.source {
                    Source::Immediate(extra) => extra,
                    _ => 0,
                };
                self.body.local_get(REGISTER_BASE + u32::from(STACK_POINTER));
                self.body.local_set(A);
                self.check(false, Width::Qword, instruction);
                self.load(Width::Qword);
                self.body.local_set(RIP);
                self.body.local_get(A);
                self.body.i64_const(8 + extra as i64);
                self.body.i64_add();
                self.body.local_set(REGISTER_BASE + u32::from(STACK_POINTER));
                self.leave(KIND_CONTINUE, after);
                return;
            }
        }
        self.done = after;
    }
}
