//! Data segments: the linear-memory contents an object contributes, and the
//! relocations that live inside them.

use super::binary::write_unsigned_leb128;
use super::linking::{Relocation, SegmentInfo};

/// One data segment. The transpiler emits one per input data section, which
/// keeps every intra-section offset — including references past the end of a
/// symbol, such as string pooling or table interiors — exactly as the input
/// had them.
#[derive(Clone, Debug)]
pub struct DataSegment {
    /// Segment name, carried in the linking section's segment info. The
    /// `.bss` prefix is meaningful: `wasm-ld` recognises it and places the
    /// segment in the zero-initialised region instead of emitting its bytes.
    pub name: String,
    pub alignment_log2: u32,
    pub bytes: Vec<u8>,
    /// Relocations at offsets relative to the start of this segment.
    pub relocations: Vec<Relocation>,
}

impl DataSegment {
    pub fn segment_info(&self) -> SegmentInfo {
        SegmentInfo {
            name: self.name.clone(),
            alignment_log2: self.alignment_log2,
            flags: 0,
        }
    }
}

/// Serializes a data section payload and rebases each segment's relocations
/// onto offsets relative to that payload, as `reloc.DATA` requires.
///
/// The segment offset expressions carry each segment's running offset within
/// this object, mirroring what LLVM emits; the linker assigns the real
/// addresses.
pub fn write_data_section_payload(segments: &[DataSegment]) -> (Vec<u8>, Vec<Relocation>) {
    let mut payload = Vec::new();
    let mut relocations = Vec::new();
    write_unsigned_leb128(&mut payload, segments.len() as u64);

    let mut running_offset: i64 = 0;
    for segment in segments {
        payload.push(0x00); // active segment, memory index 0
        payload.push(0x41); // i32.const
        super::binary::write_signed_leb128(&mut payload, running_offset);
        payload.push(0x0b); // end
        write_unsigned_leb128(&mut payload, segment.bytes.len() as u64);

        let segment_start = payload.len() as u32;
        payload.extend_from_slice(&segment.bytes);
        // The linker requires a `reloc.*` section's entries in offset order,
        // and a segment's relocations arrive from several passes.
        let mut within_segment: Vec<Relocation> = segment
            .relocations
            .iter()
            .map(|relocation| Relocation {
                offset: relocation.offset + segment_start,
                ..*relocation
            })
            .collect();
        within_segment.sort_by_key(|relocation| relocation.offset);
        relocations.extend(within_segment);

        running_offset += segment.bytes.len() as i64;
    }

    (payload, relocations)
}
