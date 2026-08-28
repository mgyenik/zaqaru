//! Reaching into guest memory, with the bounds check POSIX requires.
//!
//! A guest address is a linear-memory offset and a Rust pointer inside the
//! module is the same number, which is what makes a syscall cost a call
//! rather than a copy. It is *not* a licence to dereference whatever the
//! guest passed. Two things go wrong without a check, and both are silent:
//!
//! - **Truncation.** `usize` is 32 bits inside the module, so validating a
//!   `u64` address and then casting it discards the top half. A `write` of
//!   `0x1_0000_0005` bytes would report four gigabytes written and deliver
//!   five, and a libc retry loop believes it.
//! - **Wrapping.** `slice::from_raw_parts` is undefined when the slice wraps
//!   the address space, which a guest can arrange with a high address and a
//!   long count.
//!
//! And an unchecked out-of-range access does not fail gracefully: the wasm
//! load traps and takes the whole instance with it, where Linux returns
//! `EFAULT` to one thread. The check is what turns an instance-killing trap
//! into a syscall that fails.
//!
//! The arithmetic is a pure function of an address, a length and a limit, so
//! it is tested natively against explicit limits rather than only against
//! whatever the module's memory happens to be.

use crate::errno::Errno;

/// A view of the guest's linear memory, bounded by its current size.
#[derive(Clone, Copy)]
pub struct GuestMemory {
    /// One past the highest addressable byte.
    limit: u64,
}

impl GuestMemory {
    pub fn with_limit(limit: u64) -> Self {
        Self { limit }
    }

    /// Whether `length` bytes at `address` are entirely inside the guest.
    ///
    /// A zero-length access touches nothing and is allowed anywhere, which is
    /// what lets a zero-length `write` name a null buffer — and callers do.
    /// A null pointer with a real length is `EFAULT`, as on Linux.
    pub fn check(&self, address: u64, length: u64) -> Result<(), Errno> {
        if length == 0 {
            return Ok(());
        }
        if address == 0 {
            return Err(Errno::Fault);
        }
        let end = address.checked_add(length).ok_or(Errno::Fault)?;
        if end > self.limit {
            return Err(Errno::Fault);
        }
        // Only now is the cast safe: the range fits the address space, so it
        // fits `usize` on every target this runs on.
        if usize::try_from(end).is_err() {
            return Err(Errno::Fault);
        }
        Ok(())
    }

    /// # Safety
    /// The caller must be inside the guest's address space, which is what
    /// [`Self::check`] establishes and this function re-establishes itself.
    pub unsafe fn slice<'a>(&self, address: u64, length: u64) -> Result<&'a [u8], Errno> {
        self.check(address, length)?;
        if length == 0 {
            return Ok(&[]);
        }
        Ok(unsafe {
            core::slice::from_raw_parts(address as usize as *const u8, length as usize)
        })
    }

    /// A NUL-terminated string at a guest address — a path, an attribute
    /// name, anything the guest hands over as a C string.
    ///
    /// Bounded twice: by the guest's memory, and by `limit`, which callers
    /// set to whatever POSIX maximum applies. A string with no terminator
    /// inside the bound is `ENAMETOOLONG`, which is what Linux answers, and
    /// is emphatically not the same as reading until something happens to be
    /// zero.
    ///
    /// # Safety
    /// As [`Self::slice`].
    pub unsafe fn c_string<'a>(&self, address: u64, limit: usize) -> Result<&'a [u8], Errno> {
        // The reachable span, which may be shorter than the limit near the
        // end of memory — a string may legitimately finish just before it.
        let available = self.limit.saturating_sub(address).min(limit as u64);
        if address == 0 || available == 0 {
            return Err(Errno::Fault);
        }
        let bytes = unsafe { self.slice(address, available)? };
        match bytes.iter().position(|byte| *byte == 0) {
            Some(end) => Ok(&bytes[..end]),
            None if available < limit as u64 => Err(Errno::Fault),
            None => Err(Errno::NameTooLong),
        }
    }

    /// Fills a range of the guest's memory with a byte.
    ///
    /// Separate from [`Self::write`] because the ranges are a different
    /// size: a mapping is zeroed a megabyte at a time, and doing that
    /// through a slice the caller had to materialise would mean the kernel
    /// allocating a megabyte to write zeros with.
    ///
    /// # Safety
    /// As [`Self::slice`].
    pub unsafe fn fill(&self, address: u64, length: u64, byte: u8) -> Result<(), Errno> {
        self.check(address, length)?;
        if length == 0 {
            return Ok(());
        }
        // SAFETY: the range is inside the guest's memory, checked above.
        unsafe {
            core::ptr::write_bytes(address as usize as *mut u8, byte, length as usize);
        }
        Ok(())
    }

    /// Copies bytes *into* the guest.
    ///
    /// # Safety
    /// As [`Self::slice`].
    pub unsafe fn write(&self, address: u64, bytes: &[u8]) -> Result<(), Errno> {
        self.check(address, bytes.len() as u64)?;
        if bytes.is_empty() {
            return Ok(());
        }
        // `copy_from`, not `copy_from_nonoverlapping`: the source can be the
        // image blob, and the image blob is a data segment in the *same*
        // linear memory the destination address names. A guest is free to
        // hand `read(2)` a buffer that overlaps the file it is reading, and
        // the non-overlapping form is undefined there.
        unsafe {
            (address as usize as *mut u8).copy_from(bytes.as_ptr(), bytes.len());
        }
        Ok(())
    }

    /// # Safety
    /// As [`Self::slice`].
    pub unsafe fn store_u64(&self, address: u64, value: u64) -> Result<(), Errno> {
        self.check(address, 8)?;
        unsafe {
            (address as usize as *mut u8).copy_from_nonoverlapping(value.to_le_bytes().as_ptr(), 8);
        }
        Ok(())
    }
}
