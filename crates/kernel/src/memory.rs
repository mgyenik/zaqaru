//! Reaching into guest memory, through the page table and nowhere else.
//!
//! A guest address is a linear-memory offset and a Rust pointer inside the
//! module is the same number, which is what makes a syscall cost a call
//! rather than a copy. It is *not* a licence to dereference whatever the
//! guest passed, and under the VM it is not even a licence the kernel holds:
//! **every kernel access to guest memory goes through
//! [`cpu::space::Space`]**, which is where the permission bits and the
//! code-page invalidation hook live.
//!
//! That is a correctness property, not a style rule. The set of writers to
//! guest memory is closed — the interpreter's stores, these helpers, and the
//! mapping rows — and a writer that reaches around the page table leaves a
//! decoded block executing bytes that no longer exist, or writes a page the
//! guest mapped read-only. Both are silent. So this module is an *adapter*
//! rather than an implementation: it turns the address space's answers into
//! the errnos a syscall row returns, and it holds no way to reach memory
//! that the address space does not.
//!
//! Two failures are worth keeping apart, and Linux keeps them apart too:
//!
//! - **Truncation.** `usize` is 32 bits inside the module, so validating a
//!   `u64` address and then casting it discards the top half. A `write` of
//!   `0x1_0000_0005` bytes would report four gigabytes written and deliver
//!   five, and a libc retry loop believes it.
//! - **Wrapping.** A slice that wraps the address space is undefined, which
//!   a guest can arrange with a high address and a long count.
//!
//! Both are the address space's business now, and both are checked there.

use cpu::space::{Access, Fault, Space, Unterminated};

use crate::errno::Errno;

/// What a refused access means to a syscall row.
///
/// The same [`Fault`] serves two consumers, and this is the seam between
/// them: at a syscall row a refused access is `EFAULT`, one call failing;
/// at the interpreter's loop the same fault is a `SIGSEGV` delivered to the
/// guest. One check, two meanings, decided by who asked.
impl From<Fault> for Errno {
    fn from(_: Fault) -> Self {
        Errno::Fault
    }
}

/// The kernel's view of guest memory, for the rows that only read it.
///
/// Borrowed rather than copied, which is the whole point: there is one
/// address space, the kernel does not get a private handle on it, and a new
/// call site cannot acquire one without saying so in its signature.
///
/// Split from [`GuestMemory`] along the line that matters: a *write* to
/// guest memory can invalidate a decoded block and so needs the address
/// space mutably, and a read cannot and does not. The split is what lets a
/// row that reads a path keep working on `&self` — and, more usefully, it
/// means a reviewer can find every kernel write to guest memory by looking
/// for one method name.
pub struct GuestReader<'a> {
    space: &'a Space,
}

impl<'a> GuestReader<'a> {
    pub fn new(space: &'a Space) -> Self {
        Self { space }
    }

    /// Whether `length` bytes at `address` are reachable at all.
    ///
    /// Reachable, not readable or writable: this is `access_ok`, the check
    /// Linux makes *before* an access rather than instead of one. A range
    /// that passes here can still fail the access that follows, because the
    /// access tests the permission it actually needs — which is how a write
    /// to a read-only mapping becomes `EFAULT` rather than a silent success.
    ///
    /// A zero-length access touches nothing and is allowed anywhere, which
    /// is what lets a zero-length `write` name a null buffer — and callers
    /// do. A null pointer with a real length is `EFAULT`, as on Linux,
    /// because page zero is never mapped.
    pub fn check(&self, address: u64, length: u64) -> Result<(), Errno> {
        if length == 0 {
            return Ok(());
        }
        // Either permission will do; the access decides which it needs.
        match self.space.check(address, length, Access::Read) {
            Ok(()) => Ok(()),
            Err(_) => Ok(self.space.check(address, length, Access::Write)?),
        }
    }

    /// # Safety
    /// The reference outlives this borrow, and the guest can write the
    /// range while it is live. Callers must not hold one across anything
    /// that lets the guest run.
    pub unsafe fn slice<'b>(&self, address: u64, length: u64) -> Result<&'b [u8], Errno> {
        // SAFETY: the caller's obligation, forwarded.
        Ok(unsafe { self.space.slice(address, length) }?)
    }

    /// A NUL-terminated string at a guest address — a path, an attribute
    /// name, anything the guest hands over as a C string.
    ///
    /// # Safety
    /// As [`GuestMemory::slice`].
    pub unsafe fn c_string<'b>(&self, address: u64, limit: usize) -> Result<&'b [u8], Errno> {
        // SAFETY: the caller's obligation, forwarded.
        match unsafe { self.space.c_string(address, limit) }? {
            Ok(bytes) => Ok(bytes),
            Err(Unterminated::Fault) => Err(Errno::Fault),
            Err(Unterminated::TooLong) => Err(Errno::NameTooLong),
        }
    }

}

/// The kernel's view of guest memory, for the rows that write it.
///
/// Every kernel write to guest memory is a call on this type, and this type
/// can only be obtained from the one address space — which is what makes
/// the closed writer set closed.
pub struct GuestMemory<'a> {
    space: &'a mut Space,
}

impl<'a> GuestMemory<'a> {
    pub fn new(space: &'a mut Space) -> Self {
        Self { space }
    }

    /// As [`GuestReader::check`].
    pub fn check(&self, address: u64, length: u64) -> Result<(), Errno> {
        GuestReader::new(self.space).check(address, length)
    }

    /// # Safety
    /// As [`GuestReader::slice`].
    pub unsafe fn slice<'b>(&self, address: u64, length: u64) -> Result<&'b [u8], Errno> {
        // SAFETY: the caller's obligation, forwarded.
        unsafe { GuestReader::new(self.space).slice(address, length) }
    }

    /// # Safety
    /// As [`GuestReader::slice`].
    pub unsafe fn c_string<'b>(&self, address: u64, limit: usize) -> Result<&'b [u8], Errno> {
        // SAFETY: the caller's obligation, forwarded.
        unsafe { GuestReader::new(self.space).c_string(address, limit) }
    }

    /// Populates memory the kernel is handing over, without asking the
    /// guest's permissions — see [`Space::place`].
    ///
    /// Used by exactly three kinds of row: a fresh mapping being zeroed, a
    /// file's bytes being copied into a mapping of it, and a program's
    /// segments being loaded. Every other write to guest memory is a write
    /// on the guest's behalf and goes through [`GuestMemory::write`], where
    /// a read-only page is `EFAULT` as it should be.
    pub fn place(&mut self, address: u64, bytes: &[u8]) -> Result<(), Errno> {
        Ok(self.space.place(address, bytes)?)
    }

    /// As [`GuestMemory::place`], for a run of one byte.
    pub fn place_fill(&mut self, address: u64, length: u64, byte: u8) -> Result<(), Errno> {
        Ok(self.space.place_fill(address, length, byte)?)
    }

    /// Fills a range of the guest's memory with a byte.
    ///
    /// Separate from [`GuestMemory::write`] because the ranges are a
    /// different size: a mapping is zeroed a megabyte at a time, and doing
    /// that through a slice the caller had to materialise would mean the
    /// kernel allocating a megabyte to write zeros with.
    ///
    /// # Safety
    /// Nothing, any more — the address space checks. The marker is kept so
    /// that call sites do not have to change shape, and so that removing it
    /// is one edit rather than sixty.
    pub unsafe fn fill(&mut self, address: u64, length: u64, byte: u8) -> Result<(), Errno> {
        Ok(self.space.fill(address, length, byte)?)
    }

    /// Copies bytes *into* the guest.
    ///
    /// # Safety
    /// As [`GuestMemory::fill`].
    pub unsafe fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), Errno> {
        Ok(self.space.write(address, bytes)?)
    }

    /// # Safety
    /// As [`GuestMemory::fill`].
    pub unsafe fn store_u64(&mut self, address: u64, value: u64) -> Result<(), Errno> {
        Ok(self
            .space
            .store(address, cpu::state::Width::Qword, value)?)
    }
}
