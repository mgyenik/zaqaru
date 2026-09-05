//! The canonical ABI's wire shapes for the two host imports.
//!
//! Target-independent on purpose: the layouts are a contract with the host,
//! and a contract is checked natively, in the unit tests below, against
//! literals written from the specification rather than against the
//! constants the code itself uses.

/// The canonical ABI numbers a variant's cases in declaration order, and
/// `option` is declared `none | some`.
pub const OPTION_NONE: u32 = 0;
pub const OPTION_SOME: u32 = 1;

/// One `list<u8>`: a pointer into linear memory and a length in bytes.
///
/// The layout is the wire format, so the representation is pinned rather
/// than left to the compiler.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Slice {
    pub pointer: u32,
    pub length: u32,
}

impl Slice {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            pointer: bytes.as_ptr() as usize as u32,
            length: bytes.len() as u32,
        }
    }

    /// # Safety
    /// The slice must name live bytes in this module's linear memory — which
    /// is what the host writes there, and what `Slice::of` produces.
    pub unsafe fn as_bytes<'a>(self) -> &'a [u8] {
        if self.length == 0 {
            return &[];
        }
        unsafe {
            core::slice::from_raw_parts(self.pointer as usize as *const u8, self.length as usize)
        }
    }
}

/// `ll_read`'s return area: sixteen bytes, four-byte aligned.
///
/// The two arms of a `result` occupy the *same* twelve bytes, so this is a
/// union with a discriminant in front and is written as one. Naming three
/// fixed fields would silently encode the `ok` arm's shape and hand back
/// nonsense for the other — the `err` arm's string starts where the `ok`
/// arm's inner discriminant is.
///
/// - `[0]` outer discriminant: `0` = ok, `1` = err.
/// - on ok, `[4]` is the `option`'s discriminant (`0` = none, `1` = some,
///   the canonical ABI's declaration order) and `[8..16]` the bytes'
///   `(pointer, length)`.
/// - on err, `[4..12]` is the message's `(pointer, length)`.
///
/// The one-byte discriminants sit in four-byte slots because that is the
/// canonical ABI's padding rule, not because it is tidier.
#[repr(C, align(4))]
#[derive(Clone, Copy, Default)]
pub struct ReadResult {
    pub discriminant: u32,
    pub arm: [u32; 3],
}

impl ReadResult {
    pub fn is_error(&self) -> bool {
        self.discriminant != 0
    }

    /// The bytes, on `ok(some)`. `None` covers both `ok(none)` and `err`, so
    /// callers that care about the difference must ask [`Self::is_error`]
    /// first.
    pub fn value(&self) -> Option<Slice> {
        if self.is_error() || self.arm[0] != OPTION_SOME {
            return None;
        }
        Some(Slice {
            pointer: self.arm[1],
            length: self.arm[2],
        })
    }

    /// The diagnostic string, on `err`. A diagnostic, never an errno: what a
    /// store failure means to the guest is decided by the syscall row that
    /// provoked it.
    pub fn error(&self) -> Option<Slice> {
        self.is_error().then_some(Slice {
            pointer: self.arm[0],
            length: self.arm[1],
        })
    }
}

/// `ll_write`'s return area: twelve bytes, and a union for the same reason.
///
/// - `[0]` discriminant: `0` = ok, `1` = err.
/// - `[4..12]` a `(pointer, length)`: the result path's element array on ok,
///   the message on err.
#[repr(C, align(4))]
#[derive(Clone, Copy, Default)]
pub struct WriteResult {
    pub discriminant: u32,
    pub payload: Slice,
}

impl WriteResult {
    pub fn is_error(&self) -> bool {
        self.discriminant != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The layouts are pinned against literals written from the specification, not
    // against the constants the code itself uses. Two sides that derive their
    // expectations from one definition cannot detect that the definition is
    // wrong; the host writes these offsets independently, so this is where the
    // two are made to agree with something outside both.

    #[test]
    fn a_list_lowers_to_a_pointer_and_a_length() {
        assert_eq!(core::mem::size_of::<Slice>(), 8);
        assert_eq!(core::mem::align_of::<Slice>(), 4);
    }

    /// `result<option<list<u8>>, string>`: a one-byte discriminant padded to the
    /// payload's four-byte alignment, then a twelve-byte union.
    #[test]
    fn the_read_return_area_is_sixteen_bytes() {
        assert_eq!(core::mem::size_of::<ReadResult>(), 16);
        assert_eq!(core::mem::align_of::<ReadResult>(), 4);
        assert_eq!(core::mem::offset_of!(ReadResult, discriminant), 0);
        assert_eq!(core::mem::offset_of!(ReadResult, arm), 4);
    }

    /// `result<list<list<u8>>, string>`: the same discriminant, then an
    /// eight-byte union — both arms are a `(pointer, length)`.
    #[test]
    fn the_write_return_area_is_twelve_bytes() {
        assert_eq!(core::mem::size_of::<WriteResult>(), 12);
        assert_eq!(core::mem::align_of::<WriteResult>(), 4);
        assert_eq!(core::mem::offset_of!(WriteResult, discriminant), 0);
        assert_eq!(core::mem::offset_of!(WriteResult, payload), 4);
    }

    /// The two arms overlap, and reading the wrong one must not be possible by
    /// accident: on `err` the message starts where `ok`'s inner discriminant is.
    /// The canonical ABI numbers a variant's cases in declaration order, and
    /// `option` is `none | some`. Asserted against the literals rather than the
    /// constants used to build the value, because the two sides of this boundary
    /// agreeing with each other is not the same as either agreeing with the spec.
    #[test]
    fn the_option_discriminant_follows_declaration_order() {
        assert_eq!(OPTION_NONE, 0);
        assert_eq!(OPTION_SOME, 1);
    }

    #[test]
    fn the_two_arms_of_a_read_result_do_not_leak_into_each_other() {
        let ok_some = ReadResult {
            discriminant: 0,
            arm: [OPTION_SOME, 0x1000, 5],
        };
        assert!(!ok_some.is_error());
        assert_eq!(ok_some.error(), None);
        let value = ok_some.value().expect("ok(some) has a value");
        assert_eq!((value.pointer, value.length), (0x1000, 5));

        let ok_none = ReadResult {
            discriminant: 0,
            arm: [OPTION_NONE, 0, 0],
        };
        assert!(!ok_none.is_error());
        assert!(ok_none.value().is_none());

        // The message's (pointer, length) sits at [4..12], one word earlier than
        // the bytes would — which is exactly the mistake the union shape exists
        // to prevent.
        let failed = ReadResult {
            discriminant: 1,
            arm: [0x2000, 11, 0],
        };
        assert!(failed.is_error());
        assert!(failed.value().is_none(), "an error is not a value");
        let message = failed.error().expect("err carries a message");
        assert_eq!((message.pointer, message.length), (0x2000, 11));
    }
}
