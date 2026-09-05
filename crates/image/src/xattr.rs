//! Reading extended attributes, byte-faithfully.
//!
//! Two kinds matter for a decision nobody has made yet. `security.capability`
//! is a packed binary struct — the thing that lets `ping` work without being
//! setuid — and `user.*` is whatever the image's author put there. Neither is
//! interpreted here: they are stored as the byte strings they are, so that if
//! the kernel ever honours file capabilities the bits are sitting there unmangled.
//!
//! Not in `std`, so this is the raw pair of calls. Host-side only; nothing
//! about this crosses into the guest.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Result, bail};

/// Every extended attribute of a path, without following a final symlink —
/// a symlink's own attributes are its own.
pub fn read(path: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("{} contains a NUL", path.display()))?;

    let mut names = vec![0u8; 1024];
    let length = loop {
        let length = unsafe {
            libc::llistxattr(
                c_path.as_ptr(),
                names.as_mut_ptr() as *mut libc::c_char,
                names.len(),
            )
        };
        if length >= 0 {
            break length as usize;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            // No attributes, or a filesystem that has never heard of them.
            Some(libc::ENOTSUP) | Some(libc::ENODATA) => return Ok(Vec::new()),
            Some(libc::ERANGE) => {
                names.resize(names.len() * 2, 0);
                continue;
            }
            _ => bail!(
                "listing extended attributes of {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ),
        }
    };

    let mut attributes = Vec::new();
    for name in names[..length].split(|byte| *byte == 0) {
        if name.is_empty() {
            continue;
        }
        // `llistxattr` returns NUL-separated names, so a name cannot contain
        // one; this is a shape guarantee rather than an error path, and
        // spelling it as an error would be an untestable branch pretending
        // to be a check.
        let c_name = CString::new(name).expect("a NUL-separated name has no NUL in it");
        let bytes = value(&c_path, &c_name, path)?;
        refuse_oversize(name, bytes.len(), path)?;
        attributes.push((name.to_vec(), bytes));
    }
    // A stable order, so two bakes of the same tree are the same bytes.
    attributes.sort();
    Ok(attributes)
}

/// The index records a byte string's length in sixteen bits, and Linux caps
/// an attribute value at exactly 65536 — one byte past what fits.
///
/// Split out so it can be tested: no filesystem this runs on will accept a
/// 64 KiB value (ext4 fits a file's whole attribute block in one block), so
/// the input this guard exists for is the `docker save` tarball path, whose
/// PAX records have no such limit. A guard that can only be reached by an
/// input that does not exist yet is still a guard, and it is one whose
/// arithmetic can be checked directly.
pub fn refuse_oversize(name: &[u8], length: usize, path: &Path) -> Result<()> {
    if length > u16::MAX as usize {
        bail!(
            "the extended attribute `{}` of {} is {} bytes, past what an \
             image index can record",
            String::from_utf8_lossy(name),
            path.display(),
            length
        );
    }
    Ok(())
}

fn value(c_path: &CString, c_name: &CString, path: &Path) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; 256];
    loop {
        let length = unsafe {
            libc::lgetxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                bytes.as_mut_ptr() as *mut libc::c_void,
                bytes.len(),
            )
        };
        if length >= 0 {
            bytes.truncate(length as usize);
            return Ok(bytes);
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ERANGE) => {
                bytes.resize(bytes.len() * 2, 0);
            }
            // Raced with a removal; the attribute is simply gone.
            Some(libc::ENODATA) => return Ok(Vec::new()),
            _ => bail!(
                "reading an extended attribute of {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ),
        }
    }
}
