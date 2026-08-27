//! Byte-compatible re-implementation of `precise_number::Number` from the
//! Exponent Core workspace.
//!
//! Wire format: `#[repr(C)] struct Number([u64; 4])`, borsh-serialized as four
//! little-endian `u64` words, least-significant word first — 32 bytes total.
//! The value is a fixed-point rational with denominator `ONE = 1e12`.
//!
//! Exponent's `Number` is backed by a U256. This mock only ever uses values
//! that fit in the low 128 bits (words 2 and 3 zero), which covers every
//! exchange rate / emission index a fuzzer can meaningfully produce, and it
//! rejects anything larger rather than silently truncating.

use borsh::{BorshDeserialize, BorshSerialize};

/// Fixed-point number with 1e12 precision, laid out exactly like
/// `precise_number::Number`.
#[derive(
    BorshSerialize, BorshDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord,
)]
#[repr(C)]
pub struct Number(pub [u64; 4]);

impl Number {
    /// Serialized byte size.
    pub const SIZEOF: usize = 32;

    /// The fixed-point denominator (`precise_number::ONE`).
    pub const DENOM: u128 = 1_000_000_000_000;

    pub const ZERO: Self = Self([0, 0, 0, 0]);

    pub const ONE: Self = Self([Self::DENOM as u64, 0, 0, 0]);

    /// The raw scaled value (i.e. `value * 1e12`) as a `u128`.
    ///
    /// Returns `None` if the upper two words are non-zero.
    #[inline]
    pub fn checked_raw_u128(&self) -> Option<u128> {
        if self.0[2] != 0 || self.0[3] != 0 {
            return None;
        }
        Some((self.0[0] as u128) | ((self.0[1] as u128) << 64))
    }

    #[inline]
    pub fn from_raw_u128(raw: u128) -> Self {
        Self([raw as u64, (raw >> 64) as u64, 0, 0])
    }

    /// `Number::from_natural_u64` — scales an integer by `DENOM`.
    #[inline]
    pub fn from_natural_u64(value: u64) -> Self {
        Self::from_raw_u128((value as u128) * Self::DENOM)
    }

    /// True when every word is zero.
    #[inline]
    pub fn is_zero(&self) -> bool {
        self.0 == [0, 0, 0, 0]
    }

    /// True when the value fits in the low 128 bits.
    #[inline]
    pub fn fits_u128(&self) -> bool {
        self.0[2] == 0 && self.0[3] == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_1e12() {
        assert_eq!(Number::ONE.checked_raw_u128(), Some(1_000_000_000_000));
    }

    #[test]
    fn borsh_layout_is_32_le_bytes() {
        let n = Number([1, 2, 3, 4]);
        let bytes = borsh::to_vec(&n).unwrap();
        assert_eq!(bytes.len(), Number::SIZEOF);
        assert_eq!(&bytes[0..8], &1u64.to_le_bytes());
        assert_eq!(&bytes[8..16], &2u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &3u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &4u64.to_le_bytes());
    }

    #[test]
    fn raw_roundtrip() {
        let raw = (7u128 << 64) | 123_456_789;
        assert_eq!(
            Number::from_raw_u128(raw).checked_raw_u128(),
            Some(raw),
            "from_raw_u128 / checked_raw_u128 must round-trip"
        );
    }
}
