//! The 80-bit extended format: representation, packing, classification.
//!
//! The significand keeps its explicit integer bit, exactly as the hardware
//! stores it — which is also what makes the eventual MMX aliasing field
//! access rather than bit surgery.

/// Exponent bias of the extended format.
pub const BIAS: i32 = 16383;

/// The all-ones exponent field: infinities, NaNs, and their pseudo forms.
pub const EXPONENT_MAX: u16 = 0x7FFF;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct F80 {
    /// Bit 63 is the explicit integer bit.
    pub significand: u64,
    /// Bit 15 is the sign; bits 0–14 the biased exponent.
    pub sign_exponent: u16,
}

/// What a bit pattern is. `Subnormal` covers both true denormals and
/// pseudo-denormals (exponent field zero with the integer bit set) — both
/// are accepted operands with the same effective exponent, and the unpack
/// path treats them uniformly. `Unsupported` is the 387+ judgement on the
/// 8087's leftovers: unnormals, pseudo-infinities and pseudo-NaNs, all of
/// which are invalid operands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Zero,
    Subnormal,
    Normal,
    Infinity,
    QuietNan,
    SignallingNan,
    Unsupported,
}

/// A finite value in working form: `sig` normalized with bit 63 set, `exp`
/// unbiased, so that the value is `sig × 2^(exp − 63)`.
#[derive(Clone, Copy, Debug)]
pub struct Unpacked {
    pub sign: bool,
    pub exp: i32,
    pub sig: u64,
}

impl F80 {
    pub const fn new(sign: bool, exponent: u16, significand: u64) -> Self {
        Self {
            significand,
            sign_exponent: ((sign as u16) << 15) | (exponent & EXPONENT_MAX),
        }
    }

    pub const ZERO: Self = Self::new(false, 0, 0);
    pub const ONE: Self = Self::new(false, BIAS as u16, 1 << 63);

    /// The real indefinite QNaN: the masked response to an invalid
    /// operation, and what an empty register reads as.
    pub const INDEFINITE: Self = Self::new(true, EXPONENT_MAX, 0xC000_0000_0000_0000);

    pub const fn sign(self) -> bool {
        self.sign_exponent & 0x8000 != 0
    }

    pub const fn exponent(self) -> u16 {
        self.sign_exponent & EXPONENT_MAX
    }

    pub const fn negate(self) -> Self {
        Self {
            significand: self.significand,
            sign_exponent: self.sign_exponent ^ 0x8000,
        }
    }

    pub const fn abs(self) -> Self {
        Self {
            significand: self.significand,
            sign_exponent: self.sign_exponent & 0x7FFF,
        }
    }

    pub fn classify(self) -> Class {
        let integer_bit = self.significand & (1 << 63) != 0;
        match self.exponent() {
            0 => {
                if self.significand == 0 {
                    Class::Zero
                } else {
                    Class::Subnormal
                }
            }
            EXPONENT_MAX => {
                if !integer_bit {
                    Class::Unsupported
                } else if self.significand == 1 << 63 {
                    Class::Infinity
                } else if self.significand & (1 << 62) != 0 {
                    Class::QuietNan
                } else {
                    Class::SignallingNan
                }
            }
            _ => {
                if integer_bit {
                    Class::Normal
                } else {
                    Class::Unsupported
                }
            }
        }
    }

    pub fn is_nan(self) -> bool {
        matches!(self.classify(), Class::QuietNan | Class::SignallingNan)
    }

    /// The quieted form of a NaN: the top fraction bit set.
    pub fn quieted(self) -> Self {
        Self {
            significand: self.significand | (1 << 62),
            sign_exponent: self.sign_exponent,
        }
    }

    /// Unpacks a finite nonzero value to working form, normalizing
    /// subnormals. The caller has already classified; calling this on a
    /// zero, NaN, infinity or unsupported pattern is a bug.
    pub fn unpack(self) -> Unpacked {
        let sign = self.sign();
        if self.exponent() == 0 {
            // Denormal or pseudo-denormal: effective exponent 1 − BIAS
            // either way, significand as it stands.
            let shift = self.significand.leading_zeros() as i32;
            Unpacked {
                sign,
                exp: 1 - BIAS - shift,
                sig: self.significand << shift,
            }
        } else {
            debug_assert!(self.significand & (1 << 63) != 0);
            Unpacked {
                sign,
                exp: self.exponent() as i32 - BIAS,
                sig: self.significand,
            }
        }
    }

    /// Packs working form without rounding — for results known exact and
    /// known in range. `sig` must be normalized or zero.
    pub fn pack(sign: bool, exp: i32, sig: u64) -> Self {
        if sig == 0 {
            return Self::new(sign, 0, 0);
        }
        debug_assert!(sig & (1 << 63) != 0);
        debug_assert!((1 - BIAS..=BIAS + 1).contains(&exp) || exp >= 1 - BIAS - 63);
        if exp < 1 - BIAS {
            // Exactly representable subnormal.
            let shift = (1 - BIAS - exp) as u32;
            debug_assert!(shift <= 63 && (sig << (64 - shift)) == 0 || shift == 0);
            Self::new(sign, 0, sig >> shift)
        } else {
            Self::new(sign, (exp + BIAS) as u16, sig)
        }
    }

    pub fn to_bytes(self) -> [u8; 10] {
        let mut bytes = [0u8; 10];
        bytes[..8].copy_from_slice(&self.significand.to_le_bytes());
        bytes[8..].copy_from_slice(&self.sign_exponent.to_le_bytes());
        bytes
    }

    pub fn from_bytes(bytes: [u8; 10]) -> Self {
        Self {
            significand: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            sign_exponent: u16::from_le_bytes(bytes[8..].try_into().unwrap()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(F80::ZERO.classify(), Class::Zero);
        assert_eq!(F80::ZERO.negate().classify(), Class::Zero);
        assert_eq!(F80::ONE.classify(), Class::Normal);
        assert_eq!(F80::INDEFINITE.classify(), Class::QuietNan);
        let infinity = F80::new(false, EXPONENT_MAX, 1 << 63);
        assert_eq!(infinity.classify(), Class::Infinity);
        let signalling = F80::new(false, EXPONENT_MAX, (1 << 63) | 1);
        assert_eq!(signalling.classify(), Class::SignallingNan);
        assert_eq!(signalling.quieted().classify(), Class::QuietNan);
        // Pseudo-infinity: integer bit clear under an all-ones exponent.
        let pseudo = F80::new(false, EXPONENT_MAX, 0);
        assert_eq!(pseudo.classify(), Class::Unsupported);
        // Unnormal: integer bit clear under a nonzero, non-max exponent.
        let unnormal = F80::new(false, 100, 1 << 62);
        assert_eq!(unnormal.classify(), Class::Unsupported);
        // True denormal and pseudo-denormal both classify subnormal.
        assert_eq!(F80::new(false, 0, 1).classify(), Class::Subnormal);
        assert_eq!(F80::new(false, 0, 1 << 63).classify(), Class::Subnormal);
    }

    #[test]
    fn unpack_normalizes_subnormals() {
        let denormal = F80::new(false, 0, 1);
        let unpacked = denormal.unpack();
        assert_eq!(unpacked.sig, 1 << 63);
        assert_eq!(unpacked.exp, 1 - BIAS - 63);
        // A pseudo-denormal has the same effective exponent as the smallest
        // normal, and unpacks to exactly that.
        let pseudo = F80::new(true, 0, 1 << 63);
        let unpacked = pseudo.unpack();
        assert!(unpacked.sign);
        assert_eq!(unpacked.sig, 1 << 63);
        assert_eq!(unpacked.exp, 1 - BIAS);
    }

    #[test]
    fn byte_round_trip() {
        for value in [F80::ZERO, F80::ONE, F80::INDEFINITE, F80::new(true, 12345, 0xDEAD_BEEF)] {
            assert_eq!(F80::from_bytes(value.to_bytes()), value);
        }
    }
}
