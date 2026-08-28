//! Applying the translator's image patches to a program's bytes.
//!
//! A jump table's entries have to be rewritten so the dispatch computes
//! `table + arm` whatever form they were in — that is what makes the
//! translated dispatch a `br_table` over an arm number. For a relocatable
//! object the bytes are in a data segment the module carries, so the
//! translator rewrites them itself. For a linked one they are in the
//! program, and the program reaches the guest through the image, so the
//! rewrite has to reach the image too.
//!
//! Which makes it bake-time work: the bake is already the pass that walks
//! the tree and translates every ELF in it, and this is the other half of
//! translating one. Doing it at load time instead would mean shipping the
//! patches inside the image and having kisal apply them on every boot, to
//! reach exactly the same bytes.

use anyhow::{Result, bail};
use kisal::exec::Program;
use zaqaru::transpile::Patch;

/// Rewrites `bytes` in place, turning virtual addresses back into the file
/// offsets that hold them.
///
/// A patch that does not land in any segment's *file* range is refused
/// rather than dropped: it would mean the translator recovered a table the
/// loader will never place, and a jump table that is not rewritten
/// dispatches to whatever the original entry held.
pub fn apply(bytes: &mut [u8], patches: &[Patch]) -> Result<()> {
    if patches.is_empty() {
        return Ok(());
    }
    let program = Program::parse(bytes).map_err(|error| {
        let mut message = String::new();
        error.message(&mut message);
        anyhow::anyhow!("{message}")
    })?;

    for patch in patches {
        let length = patch.bytes.len() as u64;
        let at = program
            .loads
            .iter()
            .find_map(|load| {
                let end = load.address + load.file_size;
                (patch.address >= load.address && patch.address + length <= end)
                    .then(|| load.offset + (patch.address - load.address))
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "a patch at {:#x} is in no segment's file range, so nothing \
                     the loader places holds it",
                    patch.address
                )
            })?;
        let at = at as usize;
        if at + patch.bytes.len() > bytes.len() {
            bail!(
                "a patch at {:#x} runs past the end of the file",
                patch.address
            );
        }
        bytes[at..at + patch.bytes.len()].copy_from_slice(&patch.bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal executable whose one segment maps its whole file.
    fn program(payload: &[u8]) -> Vec<u8> {
        let mut bytes = std::vec![0u8; 64 + 56];
        bytes[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        bytes[4] = 2; // 64-bit
        bytes[5] = 1; // little-endian
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes()); // x86-64
        bytes[24..32].copy_from_slice(&0x401000u64.to_le_bytes()); // entry
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes()); // phoff
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes()); // phentsize
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes()); // phnum
        let at = 64;
        bytes[at..at + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        bytes[at + 4..at + 8].copy_from_slice(&5u32.to_le_bytes()); // R|X
        bytes[at + 8..at + 16].copy_from_slice(&0u64.to_le_bytes()); // offset
        bytes[at + 16..at + 24].copy_from_slice(&0x400000u64.to_le_bytes()); // vaddr
        let total = (bytes.len() + payload.len()) as u64;
        bytes[at + 32..at + 40].copy_from_slice(&total.to_le_bytes()); // filesz
        bytes[at + 40..at + 48].copy_from_slice(&total.to_le_bytes()); // memsz
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn a_patch_lands_at_the_file_offset_its_address_maps_to() {
        let mut bytes = program(&[0xaa; 32]);
        let payload_at = bytes.len() - 32;
        // The address of the fourth payload byte, through the segment that
        // maps the file from offset zero at `0x400000`.
        let address = 0x400000 + payload_at as u64 + 4;
        apply(
            &mut bytes,
            &[Patch {
                address,
                bytes: std::vec![1, 2, 3, 4],
            }],
        )
        .expect("apply");
        assert_eq!(&bytes[payload_at + 4..payload_at + 8], &[1, 2, 3, 4]);
        // And nothing else moved.
        assert_eq!(bytes[payload_at + 3], 0xaa);
        assert_eq!(bytes[payload_at + 8], 0xaa);
    }

    #[test]
    fn nothing_to_apply_does_not_even_look_at_the_file() {
        // Which matters because a relocatable guest produces no patches and
        // is not an ELF this could parse in the first place.
        let mut nonsense = std::vec![0u8; 4];
        assert!(apply(&mut nonsense, &[]).is_ok());
    }

    #[test]
    fn a_patch_outside_every_segment_is_refused() {
        let mut bytes = program(&[0xaa; 32]);
        let error = apply(
            &mut bytes,
            &[Patch {
                address: 0x900000,
                bytes: std::vec![1, 2, 3, 4],
            }],
        )
        .expect_err("a patch nothing places was accepted");
        assert!(
            format!("{error:#}").contains("no segment"),
            "the refusal does not say why: {error:#}"
        );
    }
}
