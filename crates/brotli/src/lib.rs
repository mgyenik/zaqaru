//! A brotli decoder for the debugger page.
//!
//! Browsers inflate gzip natively (`DecompressionStream`) and brotli not at
//! all, while brotli takes a fifth off a booted container's snapshot. So
//! the page carries this: `brotli-decompressor` compiled to a wasm module
//! of a couple of hundred kilobytes, with an interface of three functions
//! and a memory. The caller places the compressed bytes with `alloc`,
//! reserves the output — whose length the snapshot's own prefix states —
//! and calls `decompress`; the bytes are then in this module's memory to
//! copy out. No allocator is exposed beyond `alloc` and `free`, and no
//! state survives a call.

use std::alloc::{Layout, alloc, dealloc};

/// `length` bytes in this module's memory, aligned to eight, or null.
#[unsafe(no_mangle)]
pub extern "C" fn brotli_alloc(length: usize) -> *mut u8 {
    match Layout::from_size_align(length.max(1), 8) {
        // SAFETY: a non-zero, aligned layout.
        Ok(layout) => unsafe { alloc(layout) },
        Err(_) => core::ptr::null_mut(),
    }
}

/// Gives back what `brotli_alloc` gave, with the same length.
#[unsafe(no_mangle)]
pub extern "C" fn brotli_free(pointer: *mut u8, length: usize) {
    if pointer.is_null() {
        return;
    }
    if let Ok(layout) = Layout::from_size_align(length.max(1), 8) {
        // SAFETY: `pointer` came from `brotli_alloc` with this length.
        unsafe { dealloc(pointer, layout) }
    }
}

/// Inflates `input_length` bytes at `input` into `output`, which holds
/// `output_length` bytes — exactly the inflated size, which the caller
/// knows. Answers the bytes written, or a negative number: −1 when the
/// stream is not brotli or is truncated, −2 when it is longer than the
/// output.
#[unsafe(no_mangle)]
pub extern "C" fn brotli_decompress(
    input: *const u8,
    input_length: usize,
    output: *mut u8,
    output_length: usize,
) -> i64 {
    // SAFETY: the caller placed both buffers with `brotli_alloc` and gives
    // their lengths.
    let input = unsafe { core::slice::from_raw_parts(input, input_length) };
    let output = unsafe { core::slice::from_raw_parts_mut(output, output_length) };
    let written = core::cell::Cell::new(0usize);
    let overflowed = core::cell::Cell::new(false);
    let mut decoder = brotli_decompressor::DecompressorWriter::new(
        Sink {
            into: output,
            at: &written,
            overflowed: &overflowed,
        },
        4096,
    );
    let wrote = std::io::Write::write_all(&mut decoder, input).and_then(|()| std::io::Write::flush(&mut decoder));
    drop(decoder);
    if overflowed.get() {
        return -2;
    }
    if wrote.is_err() || (written.get() == 0 && input_length > 0) {
        return -1;
    }
    written.get() as i64
}

/// Where the decoder writes: straight into the caller's buffer.
struct Sink<'a> {
    into: &'a mut [u8],
    at: &'a core::cell::Cell<usize>,
    overflowed: &'a core::cell::Cell<bool>,
}

impl std::io::Write for Sink<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let at = self.at.get();
        if bytes.len() > self.into.len() - at {
            self.overflowed.set(true);
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "the output is full"));
        }
        self.into[at..at + bytes.len()].copy_from_slice(bytes);
        self.at.set(at + bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
