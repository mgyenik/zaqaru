//! Primitive WebAssembly binary encoding: LEB128 integers and section framing.

/// Width of a relocatable (linker-patchable) LEB128 immediate, in bytes.
///
/// Five bytes is exactly enough for any 32-bit value, which is what every
/// index and address in the wasm relocation format is.
pub fn write_unsigned_leb128(output: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            return;
        }
    }
}

pub fn write_signed_leb128(output: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let sign_bit_set = byte & 0x40 != 0;
        let done = (value == 0 && !sign_bit_set) || (value == -1 && sign_bit_set);
        output.push(if done { byte } else { byte | 0x80 });
        if done {
            return;
        }
    }
}

/// Encodes `value` as a fixed-width 5-byte unsigned LEB128, the form the
/// linker can overwrite in place.
pub fn write_name(output: &mut Vec<u8>, name: &str) {
    write_unsigned_leb128(output, name.len() as u64);
    output.extend_from_slice(name.as_bytes());
}

/// Writes a complete section: id, byte length, payload.
pub fn write_section(output: &mut Vec<u8>, section_id: u8, payload: &[u8]) {
    output.push(section_id);
    write_unsigned_leb128(output, payload.len() as u64);
    output.extend_from_slice(payload);
}

/// Writes a custom section, whose payload begins with its name.
pub fn write_custom_section(output: &mut Vec<u8>, name: &str, payload: &[u8]) {
    let mut framed = Vec::with_capacity(name.len() + payload.len() + 8);
    write_name(&mut framed, name);
    framed.extend_from_slice(payload);
    write_section(output, SECTION_CUSTOM, &framed);
}

pub const SECTION_CUSTOM: u8 = 0;
pub const SECTION_TYPE: u8 = 1;
pub const SECTION_IMPORT: u8 = 2;
pub const SECTION_FUNCTION: u8 = 3;
pub const SECTION_GLOBAL: u8 = 6;
pub const SECTION_EXPORT: u8 = 7;
pub const SECTION_ELEMENT: u8 = 9;
pub const SECTION_CODE: u8 = 10;
pub const SECTION_DATA: u8 = 11;
pub const SECTION_DATA_COUNT: u8 = 12;
/// The exception-handling proposal's tag section. Its ordinal is out of
/// order with its position: the binary format places it between the memory
/// and global sections.
pub const SECTION_TAG: u8 = 13;

pub const WASM_MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6d];
pub const WASM_VERSION: [u8; 4] = [0x01, 0x00, 0x00, 0x00];

#[cfg(test)]
mod tests {
    use super::*;

    fn unsigned(value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_unsigned_leb128(&mut bytes, value);
        bytes
    }

    fn signed(value: i64) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_signed_leb128(&mut bytes, value);
        bytes
    }

    #[test]
    fn canonical_unsigned_encoding() {
        assert_eq!(unsigned(0), [0x00]);
        assert_eq!(unsigned(1), [0x01]);
        assert_eq!(unsigned(127), [0x7f]);
        assert_eq!(unsigned(128), [0x80, 0x01]);
        assert_eq!(unsigned(624485), [0xe5, 0x8e, 0x26]);
        for value in [0u64, 1, 127, 128, 4096, u64::from(u32::MAX), u64::MAX] {
            assert_eq!(decode_unsigned(&unsigned(value)), value);
        }
    }

    #[test]
    fn canonical_signed_encoding() {
        assert_eq!(signed(0), [0x00]);
        assert_eq!(signed(-1), [0x7f]);
        assert_eq!(signed(63), [0x3f]);
        assert_eq!(signed(64), [0xc0, 0x00]);
        assert_eq!(signed(-64), [0x40]);
        assert_eq!(signed(-65), [0xbf, 0x7f]);
        assert_eq!(signed(-123456), [0xc0, 0xbb, 0x78]);
        for value in [0i64, 1, -1, 63, -64, 65535, -65536, i64::from(i32::MIN), i64::MAX] {
            assert_eq!(decode_signed(&signed(value)), value);
        }
    }

    fn decode_unsigned(bytes: &[u8]) -> u64 {
        let mut result = 0u64;
        for (index, byte) in bytes.iter().enumerate() {
            result |= u64::from(byte & 0x7f) << (7 * index);
        }
        result
    }

    fn decode_signed(bytes: &[u8]) -> i64 {
        let mut result = 0i64;
        let mut shift = 0;
        let mut last = 0;
        for byte in bytes {
            result |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            last = *byte;
        }
        if shift < 64 && last & 0x40 != 0 {
            result |= -1i64 << shift;
        }
        result
    }
}
