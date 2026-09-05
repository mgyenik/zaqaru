//! Instructions as text, for a debugger reading the machine.
//!
//! Behind the `disassembly` feature: the engine decodes every instruction it
//! runs and never needs to print one, so the formatter's tables are bytes a
//! module ships only when something outside wants to read code. What is
//! served is the run of instructions from an address — a thread's `rip` —
//! as far as the executable mapping goes; going backwards from an address
//! is not well defined on x86, and is not attempted.

use iced_x86::{Decoder, DecoderOptions, FastFormatter};

use crate::space::Space;

/// One instruction: where it is, its bytes, and its text.
pub struct Line {
    pub address: u64,
    pub bytes: Vec<u8>,
    pub text: String,
}

/// Up to `count` instructions starting at `address`, stopping where the
/// executable mapping ends or the bytes stop decoding. Empty when `address`
/// is not executable.
pub fn disassemble(space: &Space, address: u64, count: usize) -> Vec<Line> {
    const LONGEST: u64 = 15;
    let Ok(bytes) = space.fetch(address, count as u64 * LONGEST) else {
        return Vec::new();
    };
    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut formatter = FastFormatter::new();
    formatter.options_mut().set_space_after_operand_separator(true);
    formatter.options_mut().set_uppercase_hex(false);
    formatter.options_mut().set_use_hex_prefix(true);
    let mut lines = Vec::with_capacity(count);
    let mut text = String::new();
    while lines.len() < count && decoder.can_decode() {
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            break;
        }
        text.clear();
        formatter.format(&instruction, &mut text);
        let start = (instruction.ip() - address) as usize;
        lines.push(Line {
            address: instruction.ip(),
            bytes: bytes[start..start + instruction.len()].to_vec(),
            text: text.clone(),
        });
    }
    lines
}
