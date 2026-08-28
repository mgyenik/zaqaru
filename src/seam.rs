//! The kernel seam: where a translated `syscall` stops being x86 and becomes
//! an ordinary typed wasm call.
//!
//! This is the sibling of [`crate::thunks`]. That module bridges *outwards*
//! to functions somebody else compiled; this one bridges *downwards* to the
//! kernel, and the difference that matters is where the signature comes from.
//! An interop thunk's signature is discovered — declared, read out of a wasm
//! object, or inferred from call sites. The seam's is fixed by Linux: `rax`
//! carries the number, `rdi, rsi, rdx, r10, r8, r9` the arguments, and `rax`
//! the result. There is nothing to infer, so there is nothing to get wrong at
//! run time: a kernel whose `kisal_syscall` disagrees with this shape fails
//! at link.
//!
//! Everything the seam object contains is machinery the *whole* container
//! runtime shares, which is why it is one object rather than a piece of each:
//!
//! - `x86_syscall`, the seam proper;
//! - the `x86_yield` tag and the throw/catch pair the scheduler is built
//!   from — a thread blocks by throwing away its redundant wasm frames, and
//!   `x86_run_thread` is where the unwind lands;
//! - `x86_save_machine`/`x86_load_machine`, which move the whole register
//!   file between the globals and a thread control block in linear memory.
//!
//! None of the scheduling machinery is *used* yet. It is emitted, linked and
//! exercised now because its risk is toolchain risk — whether `wasm-ld` and
//! the engine accept a tag at all — and toolchain risk is worth retiring at
//! the first opportunity rather than in the middle of building a scheduler.

use anyhow::Result;

use crate::abi::ARGUMENT_REGISTERS;
use crate::emitter::code::{
    DataReference, FunctionBody, FunctionBodyBuilder, FunctionReference, TagReference,
};
use crate::emitter::data::DataSegment;
use crate::emitter::linking::{DataSymbolLocation, Symbol, SymbolTarget, symbol_flags};
use crate::emitter::{
    DefinedFunction, DefinedTag, ENVIRONMENT_MODULE, FIRST_TABLE_INDEX, FunctionType,
    ImportedFunction, ValueType, WasmObject,
};
use crate::machine::{
    Flag, MachineState, OperandWidth, REGISTER_NAMES, STACK_POINTER_REGISTER,
    VECTOR_REGISTER_COUNT, VectorHalf,
};
use crate::transpile::SYSCALL_ENTRY;

/// The kernel's syscall handler, as the linker sees it: the number and six
/// arguments in, the result out. Undefined here; the kisal staticlib defines
/// it. A disagreement about this signature is a link error, which is the
/// whole reason the seam is a typed call rather than a convention.
pub const KERNEL_DISPATCH: &str = "kisal_syscall";

/// The tag a blocking thread throws. It carries nothing: everything the
/// scheduler needs — which thread, why, and what to do on wake — is already
/// in kisal's own state by the time the throw happens, and a payload would
/// be a second place for that to be recorded.
pub const YIELD_TAG: &str = "x86_yield";

/// The function that throws [`YIELD_TAG`].
///
/// Called from generated code — never from Rust. Wasm exceptions unwind wasm
/// frames without running Rust's drops, so a throw raised inside a kisal
/// frame would leak whatever that frame owned. The rule the scheduler is
/// built on is therefore that a Rust handler *returns* its decision to
/// block, and the generated seam is what turns that return into a throw.
pub const YIELD_THROW: &str = "kisal_yield";

/// What the kernel returns instead of a syscall result to say the thread
/// must *leave* rather than resume — blocked, or finished.
///
/// The kernel cannot throw. Wasm exceptions unwind wasm frames without
/// running Rust's drops, so a throw raised inside a kisal frame would leak
/// whatever that frame owned; the rule is that the kernel returns its
/// decision and the seam turns the decision into the throw. This is that
/// decision, and it is a return value rather than a second call because the
/// check costs one comparison on a path every syscall takes.
///
/// It is safe as a sentinel because the kernel is the only thing that
/// produces syscall results, and it never produces this one for a call that
/// completed — asserted on the kernel's side, where the two meet.
pub const LEAVE: i64 = 0x7a61_7161_7275_0001u64 as i64;

/// The scheduler's catch: runs one guest continuation to completion or to
/// its next yield.
pub const RUN_THREAD: &str = "x86_run_thread";

/// Where [`YIELD_THROW`] sits in the indirect function table.
///
/// A table slot is not something a linked module can be asked for from the
/// outside, so a seam function that must be reachable *by slot* needs an
/// accessor that the linker fills in. `signal_dispatch` will need exactly
/// this at M10 — the design puts it "at a reserved table slot" — and the
/// mechanism is proved here, by M1's own end-to-end test of the throw.
pub const YIELD_SLOT: &str = "x86_yield_slot";

/// Moving the whole register file between the globals and linear memory: the
/// two halves of a thread control block's save and restore.
pub const SAVE_MACHINE: &str = "x86_save_machine";
pub const LOAD_MACHINE: &str = "x86_load_machine";

/// Reading and writing the `%fs` base one cell at a time.
///
/// The kernel is Rust, and a wasm global is not something Rust can name — so
/// every register the kernel touches individually needs a generated
/// accessor. `arch_prctl` is the first, and it is the reason these exist;
/// the whole-file [`SAVE_MACHINE`]/[`LOAD_MACHINE`] pair is for context
/// switches, where moving one cell at a time would be absurd.
pub const GET_SEGMENT_BASE: &str = "x86_get_fs_base";
pub const SET_SEGMENT_BASE: &str = "x86_set_fs_base";

/// Reading and writing `%rsp` the same way, which boot is what needs.
///
/// Every other register a process starts with is zero, and a wasm global
/// starts at zero, so the stack pointer is the only cell between a fresh
/// instance and the state Linux hands `_start`. Writing it one cell at a
/// time rather than through [`LOAD_MACHINE`] keeps the image's layout stated
/// in one place — the seam — instead of in the kernel as well, where a
/// disagreement would be silent rather than a link error.
/// What the weak exec map answers: there is no linked program in this
/// container, so no address resolves to anything.
///
/// Negative because a slot never is, and a distinct value from the real
/// map's refusal — which traps, because there *is* a program and the address
/// given was not in it.
pub const NO_EXEC_MAP: i32 = -1;

pub const GET_STACK_POINTER: &str = "x86_get_rsp";
pub const SET_STACK_POINTER: &str = "x86_set_rsp";

/// Alignment `wasm-ld` and clang's shadow stack both assume.
const STACK_ALIGNMENT: i32 = 16;

/// The kernel's own stack, and the symbol naming it.
///
/// The kernel does not borrow the guest's stack, and that is a correctness
/// requirement rather than tidiness. A `syscall` is not a `call`: the SysV
/// ABI lets a callee destroy the 128-byte red zone below `%rsp`, and the
/// Linux kernel does not — compilers keep a leaf function's locals there
/// across an inline `syscall` without ever moving `%rsp`. A kernel allocating
/// its frames downward from the guest's `%rsp` walks straight through that,
/// silently, to a depth nobody can bound.
///
/// Giving the kernel a region of its own removes that class — the guest's red
/// zone is no longer anywhere the kernel writes — and it settles a second
/// question the scheduler was going to raise: when a blocking syscall throws,
/// the restore at the end of the seam never runs, so the catch has to put the
/// shadow-stack pointer back, and with a fixed region that is one store to a
/// constant instead of a value that has to have been saved somewhere.
///
/// What it does *not* remove is the region's own bound. A fixed 64 KiB with
/// nothing below it is a margin, and wasm has no faults: an overrun would run
/// into whatever the linker placed next, silently. So the region carries a
/// guard below it, filled with a sentinel before every syscall and checked
/// after — see [`GUARD_SIZE`]. This has a guard page's limitation and not
/// more: a single frame larger than the guard could step over it without
/// touching it. Every frame the kernel actually builds is orders of magnitude
/// smaller, and a frame that large is a compiler-visible fact rather than a
/// depth that creeps up.
///
/// M5's boot carve replaces this with a properly allocated region; until
/// then it is a segment of the seam object, sized for a kernel whose deepest
/// path formats a diagnostic.
const KERNEL_STACK: &str = "x86_kernel_stack";
const KERNEL_STACK_SIZE: u32 = 64 * 1024;

/// The guard below the kernel stack, and the sentinel that fills it.
///
/// Planted on the way in and checked on the way out, rather than once at
/// boot, so that an overrun is attributed to the syscall that caused it
/// instead of to whichever one happens to look next. Sixteen words across
/// four kilobytes: the cost is a few dozen instructions against a dispatch
/// that resolves a path, and the spacing is what decides how small a write
/// into the guard can be and still be seen.
const GUARD_SIZE: u32 = 4096;
const GUARD_STRIDE: u32 = 256;
/// Not zero and not a plausible pointer, so a partial overwrite is as visible
/// as a full one.
const GUARD_SENTINEL: i64 = 0x4b49_5341_4c47_5244; // "KISALGRD"

/// The registers a Linux syscall takes its arguments from, in order.
///
/// `r10` stands where the SysV C convention puts `rcx`, because the `syscall`
/// instruction overwrites `rcx` with the return address before the kernel
/// ever sees it. This is the one place that difference is spelled out; every
/// caller of the seam is ordinary C that never knows about it.
const SYSCALL_ARGUMENT_REGISTERS: [usize; 6] = [
    7,  // rdi
    6,  // rsi
    2,  // rdx
    10, // r10
    8,  // r8
    9,  // r9
];

/// Index of `rax` — the syscall number on the way in, the result on the way
/// out.
const SYSCALL_NUMBER_REGISTER: usize = 0;

/// The two registers the `syscall` instruction destroys. Zeroing them is not
/// emulation of what the hardware leaves behind (the hardware leaves the
/// return address and the flags); it is the deterministic choice among the
/// values a conforming caller is allowed to see, since every libc marks both
/// clobbered.
const SYSCALL_CLOBBERED_REGISTERS: [usize; 2] = [
    1,  // rcx
    11, // r11
];

/// The layout of a saved register file in linear memory.
///
/// Sixteen general-purpose registers, then the `%fs` base as the
/// seventeenth, then both halves of sixteen XMM registers, then the flags.
/// Fixed and dense on purpose: it is a wire format between generated wasm
/// and the kernel's Rust, and the kernel indexes it with the same constants.
pub mod machine_image {
    use super::{Flag, VECTOR_REGISTER_COUNT};

    pub const REGISTER_OFFSET: u32 = 0;
    /// The seventeenth register sits where a seventeenth register would,
    /// which also keeps every eight-byte cell eight-byte aligned.
    pub const SEGMENT_BASE_OFFSET: u32 = 16 * 8;
    /// The timestamp counter, beside the segment base for the same reason:
    /// it is machine state that has to survive a context switch and a
    /// checkpoint, or a resumed run reads a different value than the one it
    /// is replaying. See `crate::machine::TIMESTAMP_STEP`.
    pub const TIMESTAMP_OFFSET: u32 = SEGMENT_BASE_OFFSET + 8;
    pub const VECTOR_OFFSET: u32 = TIMESTAMP_OFFSET + 8;
    pub const FLAG_OFFSET: u32 = VECTOR_OFFSET + (VECTOR_REGISTER_COUNT as u32) * 16;
    pub const SIZE: u32 = FLAG_OFFSET + (Flag::ALL.len() as u32) * 4;
}

/// Builds the seam object.
pub fn build_seam_object() -> Result<Vec<u8>> {
    let mut wasm = WasmObject::new();
    let machine = MachineState::define(&mut wasm);

    let guest_type = wasm.intern_type(FunctionType {
        parameters: vec![],
        results: vec![],
    });
    let dispatch_type = wasm.intern_type(FunctionType {
        parameters: vec![ValueType::I64; 1 + ARGUMENT_REGISTERS.len()],
        results: vec![ValueType::I64],
    });
    let machine_image_type = wasm.intern_type(FunctionType {
        parameters: vec![ValueType::I32],
        results: vec![],
    });
    let exec_map_type = wasm.intern_type(FunctionType {
        parameters: vec![ValueType::I64],
        results: vec![ValueType::I32],
    });
    let run_thread_type = wasm.intern_type(FunctionType {
        parameters: vec![ValueType::I32],
        results: vec![ValueType::I32],
    });
    let slot_accessor_type = wasm.intern_type(FunctionType {
        parameters: vec![],
        results: vec![ValueType::I32],
    });
    let segment_read_type = wasm.intern_type(FunctionType {
        parameters: vec![],
        results: vec![ValueType::I64],
    });
    let segment_write_type = wasm.intern_type(FunctionType {
        parameters: vec![ValueType::I64],
        results: vec![],
    });
    let tag_type = wasm.intern_type(FunctionType {
        parameters: vec![],
        results: vec![],
    });

    // Imports take the low end of the function index space, so the kernel's
    // dispatcher is declared before anything defined here takes an index.
    let dispatch_index = wasm.imported_functions.len() as u32;
    wasm.imported_functions.push(ImportedFunction {
        module: ENVIRONMENT_MODULE.to_string(),
        field: KERNEL_DISPATCH.to_string(),
        type_index: dispatch_type,
    });
    let dispatch = FunctionReference {
        symbol_index: wasm.add_symbol(Symbol {
            name: KERNEL_DISPATCH.to_string(),
            target: SymbolTarget::Function(dispatch_index),
            flags: symbol_flags::UNDEFINED,
        }),
        function_index: dispatch_index,
    };

    // The kernel stack grows down, so what the seam needs is the address of
    // its *end*. A zero-length symbol there names it without a second
    // relocation or any arithmetic at run time.
    let stack_segment = wasm.data_segments.len() as u32;
    let region_size = GUARD_SIZE + KERNEL_STACK_SIZE;
    wasm.data_segments.push(DataSegment {
        // `.bss` so `wasm-ld` places it in the zero-initialised region
        // instead of carrying 64 KiB of zeros in the module.
        name: format!(".bss.{KERNEL_STACK}"),
        alignment_log2: 4,
        bytes: vec![0; region_size as usize],
        relocations: Vec::new(),
    });
    let stack_symbol = wasm.add_symbol(Symbol {
        name: KERNEL_STACK.to_string(),
        target: SymbolTarget::Data(Some(DataSymbolLocation {
            segment_index: stack_segment,
            offset: 0,
            size: region_size,
        })),
        flags: symbol_flags::LOCAL,
    });
    let stack_top = DataReference {
        symbol_index: stack_symbol,
        addend: region_size as i32,
    };
    // The guard is the low end of the same region, so it needs no second
    // symbol and cannot drift away from the stack it guards.
    let guard_base = DataReference {
        symbol_index: stack_symbol,
        addend: 0,
    };

    let tag_index = wasm.next_defined_tag_index();
    wasm.defined_tags.push(DefinedTag {
        type_index: tag_type,
    });
    let yield_tag = TagReference {
        symbol_index: wasm.add_symbol(Symbol {
            name: YIELD_TAG.to_string(),
            target: SymbolTarget::Tag(tag_index),
            // Weak for the same reason the register globals are: any object
            // may define the tag, and they must collapse onto one.
            flags: symbol_flags::WEAK,
        }),
        tag_index,
    };

    let define_with =
        |wasm: &mut WasmObject, name: &str, type_index: u32, body: FunctionBody, flags: u32| {
            let function_index = wasm.next_defined_function_index();
            wasm.defined_functions
                .push(DefinedFunction { type_index, body });
            let symbol_index = wasm.add_symbol(Symbol {
                name: name.to_string(),
                target: SymbolTarget::Function(function_index),
                flags,
            });
            FunctionReference {
                symbol_index,
                function_index,
            }
        };
    let define = |wasm: &mut WasmObject, name: &str, type_index: u32, body: FunctionBody| {
        define_with(wasm, name, type_index, body, symbol_flags::EXPORTED)
    };

    // The throw is defined before the entry that calls it, because a
    // reference to it is what the entry is built from.
    let yield_throw = define(
        &mut wasm,
        YIELD_THROW,
        guest_type,
        build_yield_throw(yield_tag),
    );
    define(
        &mut wasm,
        SYSCALL_ENTRY,
        guest_type,
        build_syscall_entry(&machine, dispatch, yield_throw, stack_top, guard_base),
    );
    define(
        &mut wasm,
        RUN_THREAD,
        run_thread_type,
        build_run_thread(&machine, yield_tag, guest_type, guard_base),
    );
    let mut body = FunctionBodyBuilder::new(0);
    body.global_get(machine.segment_base());
    define(
        &mut wasm,
        GET_SEGMENT_BASE,
        segment_read_type,
        body.finish(),
    );

    let mut body = FunctionBodyBuilder::new(1);
    body.local_get(0);
    body.global_set(machine.segment_base());
    define(
        &mut wasm,
        SET_SEGMENT_BASE,
        segment_write_type,
        body.finish(),
    );

    let mut body = FunctionBodyBuilder::new(0);
    body.global_get(machine.register(STACK_POINTER_REGISTER));
    define(
        &mut wasm,
        GET_STACK_POINTER,
        segment_read_type,
        body.finish(),
    );

    let mut body = FunctionBodyBuilder::new(1);
    body.local_get(0);
    body.global_set(machine.register(STACK_POINTER_REGISTER));
    define(
        &mut wasm,
        SET_STACK_POINTER,
        segment_write_type,
        body.finish(),
    );

    // The exec map, weakly, for a container that carries no linked program.
    //
    // The kernel's boot path names it unconditionally — it is Rust, compiled
    // once, and cannot know at its own compile time whether the container it
    // ends up in has a program to load. A linked guest defines the real one
    // and the linker prefers it; without one this answers, and the answer
    // says what is missing rather than trapping.
    let mut body = FunctionBodyBuilder::new(1);
    body.i32_const(NO_EXEC_MAP);
    define_with(
        &mut wasm,
        crate::transpile::EXEC_MAP_LOOKUP,
        exec_map_type,
        body.finish(),
        symbol_flags::WEAK,
    );

    define(
        &mut wasm,
        SAVE_MACHINE,
        machine_image_type,
        build_machine_image(&machine, Direction::Save),
    );
    define(
        &mut wasm,
        LOAD_MACHINE,
        machine_image_type,
        build_machine_image(&machine, Direction::Load),
    );

    // The throw needs a slot before the accessor that reports it can be
    // built, and a slot is only a slot once the table has it.
    let yield_slot = crate::emitter::code::TableReference {
        symbol_index: yield_throw.symbol_index,
        table_index: FIRST_TABLE_INDEX,
    };
    wasm.table_functions.push(yield_throw.function_index);
    wasm.uses_function_table = true;

    let mut body = FunctionBodyBuilder::new(0);
    body.i32_const_table_index(yield_slot);
    define(&mut wasm, YIELD_SLOT, slot_accessor_type, body.finish());

    Ok(wasm.serialize())
}

/// `x86_syscall`: the emulated convention on the outside, a typed call into
/// the kernel on the inside.
///
/// The kernel is an ordinary wasm callee, so it needs a shadow stack; what it
/// must not be given is the guest's. See [`KERNEL_STACK`] for why, and
/// [`crate::thunks`] for the case where the opposite is correct — a foreign
/// *call* is allowed to eat the red zone, and a `syscall` is not.
fn build_syscall_entry(
    machine: &MachineState,
    dispatch: FunctionReference,
    yield_throw: FunctionReference,
    stack_top: DataReference,
    guard_base: DataReference,
) -> FunctionBody {
    let mut body = FunctionBodyBuilder::new(0);
    let saved_stack_pointer = body.declare_local(ValueType::I32);

    body.global_get(machine.linker_stack_pointer);
    body.local_set(saved_stack_pointer);

    plant_guard(&mut body, guard_base);

    // The kernel runs on its own stack, never on the guest's — see
    // [`KERNEL_STACK`]. The interop thunk does the opposite, correctly, for
    // the opposite reason: a foreign *call* is allowed to eat the red zone.
    body.i32_const_data_address(stack_top);
    body.i32_const(-STACK_ALIGNMENT);
    body.i32_and();
    body.global_set(machine.linker_stack_pointer);

    body.global_get(machine.register(SYSCALL_NUMBER_REGISTER));
    for number in SYSCALL_ARGUMENT_REGISTERS {
        body.global_get(machine.register(number));
    }
    body.call(dispatch);
    // Leaving is decided by the kernel and performed here: the throw exists
    // in one place, and the kernel's frames are gone by the time anything
    // unwinds through them.
    let result = body.declare_local(ValueType::I64);
    body.local_tee(result);
    body.i64_const(LEAVE);
    body.i64_eq();
    body.if_();
    body.call(yield_throw);
    body.end();
    body.local_get(result);
    body.global_set(machine.register(SYSCALL_NUMBER_REGISTER));

    for number in SYSCALL_CLOBBERED_REGISTERS {
        body.i64_const(0);
        body.global_set(machine.register(number));
    }

    body.local_get(saved_stack_pointer);
    body.global_set(machine.linker_stack_pointer);

    check_guard(&mut body, guard_base);

    // Give back everything the translated `syscall` reserved: its
    // return-address slot, and the red zone it had to skip over to place that
    // slot somewhere the guest does not own. See
    // `crate::translate::SYSCALL_RESERVATION`.
    body.global_get(machine.register(STACK_POINTER_REGISTER));
    body.i64_const(crate::translate::SYSCALL_RESERVATION);
    body.i64_add();
    body.global_set(machine.register(STACK_POINTER_REGISTER));

    body.finish()
}

/// `kisal_yield`: one instruction, and the reason it is a function of its own
/// is that the throw then exists in exactly one place in the whole system.
fn build_yield_throw(tag: TagReference) -> FunctionBody {
    let mut body = FunctionBodyBuilder::new(0);
    body.throw(tag);
    body.finish()
}

/// `x86_run_thread`: enter a guest continuation by table slot under a catch,
/// and report which way it left.
///
/// Zero means the continuation ran off the end of its chain — the thread
/// exited. One means it yielded, and by the time the unwind lands here the
/// thread's control block already holds everything needed to resume it: the
/// flush discipline put the register file in the globals before the kernel
/// was called at all, and the destroyed wasm frames held nothing the guest
/// stack's chain of resume IDs does not.
///
/// A slot rather than a fixed callee, because the two ways a thread starts
/// running are different functions: a fresh thread enters its ELF entry
/// directly, and a suspended one enters the resume driver.
fn build_run_thread(
    machine: &MachineState,
    tag: TagReference,
    guest_type: u32,
    guard_base: DataReference,
) -> FunctionBody {
    let mut body = FunctionBodyBuilder::new(1);
    // The seam moves the shadow-stack pointer to the kernel's own region
    // and puts it back when the syscall returns — and a syscall that leaves
    // never returns, so that restore never runs. This is the only frame
    // still standing that knows what it was, so it is what puts it back.
    // Without this, every leave would strand the pointer inside the kernel's
    // fixed region and the next thing to use a shadow stack would write over
    // the kernel's frames.
    let saved_stack_pointer = body.declare_local(ValueType::I32);
    body.global_get(machine.linker_stack_pointer);
    body.local_set(saved_stack_pointer);

    // Planted here as well as in the seam, so the guard is filled before any
    // guest code runs at all. Without it the catch below would be checking
    // a region nothing had written the sentinel into, and a thread that
    // yielded without ever entering the seam — which is what a direct
    // `kisal_yield` is — would trap on a stack nobody had overrun.
    plant_guard(&mut body, guard_base);
    body.block();
    body.try_table_catch(tag, 0);
    body.local_get(0);
    body.call_indirect(guest_type);
    body.i32_const(0);
    body.return_();
    body.end(); // try_table
    body.end(); // block
    body.local_get(saved_stack_pointer);
    body.global_set(machine.linker_stack_pointer);
    // A yielding syscall leaves through the throw, so the seam's own check
    // never runs for it. The unwind lands here, and this is the first place
    // that can still see the guard the kernel was running above.
    check_guard(&mut body, guard_base);
    body.i32_const(1);
    body.finish()
}

/// Fills the guard below the kernel stack with the sentinel.
fn plant_guard(body: &mut FunctionBodyBuilder, guard_base: DataReference) {
    let mut offset = 0;
    while offset < GUARD_SIZE {
        body.i32_const_data_address(guard_base);
        body.i64_const(GUARD_SENTINEL);
        body.i64_store(3, offset);
        offset += GUARD_STRIDE;
    }
}

/// Traps if anything wrote into the guard.
///
/// A trap and not a report: the kernel is what overran, so asking it to
/// format a diagnostic would run the same code again on the same broken
/// stack. `unreachable` is the loudest thing available that does not depend
/// on the thing that just failed.
fn check_guard(body: &mut FunctionBodyBuilder, guard_base: DataReference) {
    let mut offset = 0;
    while offset < GUARD_SIZE {
        body.i32_const_data_address(guard_base);
        body.i64_load(3, offset);
        body.i64_const(GUARD_SENTINEL);
        body.i64_ne();
        body.if_();
        body.unreachable();
        body.end();
        offset += GUARD_STRIDE;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Save,
    Load,
}

/// `x86_save_machine` and `x86_load_machine`, which are the same walk over
/// the same layout in opposite directions — written once so the two can
/// never drift apart, which is the only property that makes a round trip
/// meaningful.
fn build_machine_image(machine: &MachineState, direction: Direction) -> FunctionBody {
    let mut body = FunctionBodyBuilder::new(1);
    let quad = OperandWidth::QuadWord.alignment_log2();
    let word = OperandWidth::DoubleWord.alignment_log2();

    let cell = |body: &mut FunctionBodyBuilder,
                offset: u32,
                alignment: u32,
                get: &dyn Fn(&mut FunctionBodyBuilder),
                set: &dyn Fn(&mut FunctionBodyBuilder)| {
        match direction {
            Direction::Save => {
                body.local_get(0);
                get(body);
                if alignment == word {
                    body.i32_store(alignment, offset);
                } else {
                    body.i64_store(alignment, offset);
                }
            }
            Direction::Load => {
                body.local_get(0);
                if alignment == word {
                    body.i32_load(alignment, offset);
                } else {
                    body.i64_load(alignment, offset);
                }
                set(body);
            }
        }
    };

    for number in 0..REGISTER_NAMES.len() {
        let global = machine.register(number);
        cell(
            &mut body,
            machine_image::REGISTER_OFFSET + (number as u32) * 8,
            quad,
            &move |body: &mut FunctionBodyBuilder| body.global_get(global),
            &move |body: &mut FunctionBodyBuilder| body.global_set(global),
        );
    }
    for (offset, global) in [
        (machine_image::SEGMENT_BASE_OFFSET, machine.segment_base()),
        (machine_image::TIMESTAMP_OFFSET, machine.timestamp()),
    ] {
        cell(
            &mut body,
            offset,
            quad,
            &move |body: &mut FunctionBodyBuilder| body.global_get(global),
            &move |body: &mut FunctionBodyBuilder| body.global_set(global),
        );
    }
    for number in 0..VECTOR_REGISTER_COUNT {
        for half in VectorHalf::BOTH {
            let global = machine.vector_register(number, half);
            let offset =
                machine_image::VECTOR_OFFSET + (number as u32) * 16 + (half.index() as u32) * 8;
            cell(
                &mut body,
                offset,
                quad,
                &move |body: &mut FunctionBodyBuilder| body.global_get(global),
                &move |body: &mut FunctionBodyBuilder| body.global_set(global),
            );
        }
    }
    for (index, flag) in Flag::ALL.into_iter().enumerate() {
        let global = machine.flag(flag);
        cell(
            &mut body,
            machine_image::FLAG_OFFSET + (index as u32) * 4,
            word,
            &move |body: &mut FunctionBodyBuilder| body.global_get(global),
            &move |body: &mut FunctionBodyBuilder| body.global_set(global),
        );
    }

    body.finish()
}
