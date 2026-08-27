//! Moving values between wasm's typed world and the emulated register file.
//!
//! Both boundary directions do the same four things, mirrored: a typed export
//! wrapper *stores* arguments into registers and *loads* a result back out,
//! while an outgoing thunk *loads* arguments out of registers and *stores* a
//! result back in. Writing the pair once is what keeps them from drifting —
//! an incoming `f32` that is unpacked differently from how the outgoing one
//! is packed would be a bug no single-direction test could see.
//!
//! Two conversions carry all the subtlety:
//!
//! - **Narrow integers.** A 32-bit argument occupies the low half of its
//!   register and SysV leaves the upper half undefined, so zero-extension on
//!   the way in is both cheap and faithful — it is what `mov edi, …` already
//!   does on the hardware — and wrapping on the way out simply reads the half
//!   that was ever meaningful.
//! - **`float`.** A `float` lives in the low *32* bits of an XMM register,
//!   not the low 64, so it travels through `i32`: reinterpret to bits, widen
//!   the register slot around it, and reverse that coming back. Converting
//!   between `f32` and `f64` here instead would be lossy in one direction and
//!   wrong in both.

use crate::abi::{
    ARGUMENT_REGISTERS, AbiType, ArgumentLocation, FLOAT_ARGUMENT_REGISTERS,
    FLOAT_RETURN_VALUE_REGISTER, RETURN_VALUE_REGISTER,
};
use crate::emitter::code::FunctionBodyBuilder;
use crate::machine::{MachineState, VectorHalf};

/// Takes a value of `parameter` type off the wasm stack and puts it in the
/// guest register SysV gives it.
pub fn store_argument(
    body: &mut FunctionBodyBuilder,
    machine: &MachineState,
    parameter: AbiType,
    location: ArgumentLocation,
) {
    match location {
        ArgumentLocation::Integer(slot) => {
            widen_to_register(body, parameter);
            body.global_set(machine.register(ARGUMENT_REGISTERS[slot]));
        }
        ArgumentLocation::Float(slot) => {
            widen_to_register(body, parameter);
            body.global_set(
                machine.vector_register(FLOAT_ARGUMENT_REGISTERS[slot], VectorHalf::Low),
            );
        }
    }
}

/// Pushes the guest register holding an argument onto the wasm stack, typed
/// as the signature says.
pub fn load_argument(
    body: &mut FunctionBodyBuilder,
    machine: &MachineState,
    parameter: AbiType,
    location: ArgumentLocation,
) {
    match location {
        ArgumentLocation::Integer(slot) => {
            body.global_get(machine.register(ARGUMENT_REGISTERS[slot]));
        }
        ArgumentLocation::Float(slot) => {
            body.global_get(
                machine.vector_register(FLOAT_ARGUMENT_REGISTERS[slot], VectorHalf::Low),
            );
        }
    }
    narrow_from_register(body, parameter);
}

/// Takes a result off the wasm stack and puts it in the register the guest
/// convention returns values in.
pub fn store_result(body: &mut FunctionBodyBuilder, machine: &MachineState, result: AbiType) {
    widen_to_register(body, result);
    match result {
        AbiType::I32 | AbiType::I64 => body.global_set(machine.register(RETURN_VALUE_REGISTER)),
        AbiType::F32 | AbiType::F64 => {
            body.global_set(machine.vector_register(FLOAT_RETURN_VALUE_REGISTER, VectorHalf::Low))
        }
    }
}

/// Pushes the guest result register onto the wasm stack, typed as the
/// signature says.
pub fn load_result(body: &mut FunctionBodyBuilder, machine: &MachineState, result: AbiType) {
    match result {
        AbiType::I32 | AbiType::I64 => body.global_get(machine.register(RETURN_VALUE_REGISTER)),
        AbiType::F32 | AbiType::F64 => {
            body.global_get(machine.vector_register(FLOAT_RETURN_VALUE_REGISTER, VectorHalf::Low))
        }
    }
    narrow_from_register(body, result);
}

/// Converts a value on the wasm stack into the 64 bits a register holds.
fn widen_to_register(body: &mut FunctionBodyBuilder, value: AbiType) {
    match value {
        AbiType::I64 => {}
        AbiType::I32 => body.i64_extend_i32_unsigned(),
        AbiType::F64 => body.i64_reinterpret_f64(),
        AbiType::F32 => {
            body.i32_reinterpret_f32();
            body.i64_extend_i32_unsigned();
        }
    }
}

/// The reverse: the 64 bits of a register down to the value they carry.
fn narrow_from_register(body: &mut FunctionBodyBuilder, value: AbiType) {
    match value {
        AbiType::I64 => {}
        AbiType::I32 => body.i32_wrap_i64(),
        AbiType::F64 => body.f64_reinterpret_i64(),
        AbiType::F32 => {
            body.i32_wrap_i64();
            body.f32_reinterpret_i32();
        }
    }
}
