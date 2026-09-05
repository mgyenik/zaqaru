//! The image as a relocatable wasm object.
//!
//! One module *is* the container, so the image is not a file the runtime
//! opens — it is two data segments `wasm-ld` places like any other data, and
//! the kernel finds them through link-time symbols rather than through addresses
//! anybody had to agree on. That is what makes the whole filesystem torrent
//! free: a `read(2)` of an image file is a copy inside linear memory, and no
//! part of it reaches the host.
//!
//! It also buys two things on the native host for nothing. wasmtime
//! instantiates memories copy-on-write from a prepared image, so the blob is
//! physically shared across every process instance and instantiation is an
//! mmap rather than a copy; and fork can skip the region entirely, because
//! nothing writes it.

use anyhow::Result;

use crate::wasm::{DataObject, DataSegment, DataSymbol, symbol_flags};

/// The symbols the kernel resolves the image through.
pub const BLOB_SYMBOL: &str = "__image_blob";
pub const INDEX_SYMBOL: &str = "__image_index";

/// The blob is page-aligned so that a file the image marked `MMAP_ALIGNED`
/// keeps that property in linear memory. Alignment inside a segment placed on
/// an arbitrary boundary would be alignment relative to nothing.
const BLOB_ALIGNMENT_LOG2: u32 = 12;
/// The index has eight-byte fields.
const INDEX_ALIGNMENT_LOG2: u32 = 3;

/// A region has to fit the 32-bit size a data symbol records.
///
/// The packager refuses to build an image with a region this large, so this is
/// the second of two guards rather than the only one — but it is the one
/// standing where the narrowing actually happens, and a cast that is only
/// safe because of a check in another crate is a cast that stops being safe
/// when that crate changes.
pub fn refuse_unaddressable(name: &str, length: usize) -> Result<()> {
    if length > u32::MAX as usize {
        anyhow::bail!(
            "{name} is {length} bytes, past the 32-bit size a wasm data \
             symbol records"
        );
    }
    Ok(())
}

/// Builds the object carrying an image.
pub fn emit(image: &image::Image) -> Result<Vec<u8>> {
    let mut wasm = DataObject::new();

    for (name, bytes, alignment) in [
        (BLOB_SYMBOL, &image.blob, BLOB_ALIGNMENT_LOG2),
        (INDEX_SYMBOL, &image.index, INDEX_ALIGNMENT_LOG2),
    ] {
        refuse_unaddressable(name, bytes.len())?;
        let segment_index = wasm.segments.len() as u32;
        wasm.segments.push(DataSegment {
            // Not `.bss`: these bytes are the image, and a zero-filled
            // segment would be an empty filesystem.
            name: format!(".rodata.{name}"),
            alignment_log2: alignment,
            bytes: bytes.to_vec(),
        });
        wasm.symbols.push(DataSymbol {
            name: name.to_string(),
            segment_index,
            offset: 0,
            size: bytes.len() as u32,
            // Visible to the kernel across the link, and hidden from the
            // module's public face: the image is an implementation detail of
            // the container, not something a host reaches into.
            flags: symbol_flags::HIDDEN,
        });
    }

    Ok(wasm.serialize())
}

/// The object for an image with nothing in it but a root directory.
///
/// Every container links an image, because a container without a filesystem
/// is not one — and because the kernel references the symbols unconditionally, so
/// a link that omitted the image would fail with an undefined symbol rather
/// than quietly producing a module with no files. A test that does not care
/// about the filesystem links this.
pub fn empty() -> Result<Vec<u8>> {
    emit(&image::bake_empty())
}
