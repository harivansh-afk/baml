/// A BAML `int`, represented as a signed 63-bit two's-complement value.
///
/// Keeping the range invariant in the value type lets compile-time evaluation
/// and the VM share integer semantics without duplicating bit manipulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Int63(i64);

/// The only invalid shift count in BAML is a negative one. Non-negative counts
/// at or above the i63 width have defined truncating or saturating behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntShiftError {
    /// The shift count was negative.
    NegativeCount(i64),
}

impl Int63 {
    /// Number of bits in a BAML `int`, including its sign bit.
    pub const BITS: u32 = 63;
    /// Smallest representable BAML `int`.
    pub const MIN: Self = Self(-(1_i64 << 62));
    /// Largest representable BAML `int`.
    pub const MAX: Self = Self((1_i64 << 62) - 1);
    pub const ZERO: Self = Self(0);

    const BIT_MASK: u64 = (1_u64 << Self::BITS) - 1;

    /// Construct a value when `value` is representable as a BAML `int`.
    pub const fn new(value: i64) -> Option<Self> {
        if value >= Self::MIN.0 && value <= Self::MAX.0 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Return the underlying sign-extended `i64`.
    pub const fn get(self) -> i64 {
        self.0
    }

    /// Checked arithmetic in BAML's range, independent of the execution backend.
    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        // The sum of two i63 values always fits in i64.
        Self::new(self.0 + rhs.0)
    }

    pub const fn checked_sub(self, rhs: Self) -> Option<Self> {
        Self::new(self.0 - rhs.0)
    }

    pub const fn checked_mul(self, rhs: Self) -> Option<Self> {
        match self.0.checked_mul(rhs.0) {
            Some(value) => Self::new(value),
            None => None,
        }
    }

    /// Truncates toward zero. Returns `None` for zero or an out-of-range quotient.
    pub const fn checked_div(self, rhs: Self) -> Option<Self> {
        if rhs.0 == 0 {
            None
        } else {
            Self::new(self.0 / rhs.0)
        }
    }

    /// The remainder has the dividend's sign. Returns `None` for a zero divisor.
    pub const fn checked_rem(self, rhs: Self) -> Option<Self> {
        if rhs.0 == 0 {
            None
        } else {
            Some(Self(self.0 % rhs.0))
        }
    }

    pub const fn checked_neg(self) -> Option<Self> {
        Self::new(-self.0)
    }

    pub const fn bit_and(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
    pub const fn bit_or(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
    pub const fn bit_xor(self, rhs: Self) -> Self {
        Self(self.0 ^ rhs.0)
    }

    /// Shift left modulo the i63 width and interpret the retained bits as a
    /// signed two's-complement value.
    pub fn shift_left(self, count: i64) -> Result<Self, IntShiftError> {
        match count {
            ..=-1 => Err(IntShiftError::NegativeCount(count)),
            0..63 => {
                let Ok(shift) = u32::try_from(count) else {
                    unreachable!("shift count in 0..63 always fits in u32")
                };
                let bits = (self.0.cast_unsigned() << shift) & Self::BIT_MASK;
                Ok(Self((bits << 1).cast_signed() >> 1))
            }
            63.. => Ok(Self(0)),
        }
    }

    /// Shift right arithmetically, saturating to the sign bit at or above the
    /// i63 width.
    pub fn shift_right(self, count: i64) -> Result<Self, IntShiftError> {
        match count {
            ..=-1 => Err(IntShiftError::NegativeCount(count)),
            0..63 => {
                let Ok(shift) = u32::try_from(count) else {
                    unreachable!("shift count in 0..63 always fits in u32")
                };
                Ok(Self(self.0 >> shift))
            }
            63.. => Ok(Self(if self.0 < 0 { -1 } else { 0 })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Int63;

    #[test]
    fn arithmetic_matches_wide_integer_reference_at_boundaries() {
        let values = [
            Int63::MIN.get(),
            Int63::MIN.get() + 1,
            -63,
            -1,
            0,
            1,
            63,
            Int63::MAX.get() - 1,
            Int63::MAX.get(),
        ];
        let narrow = |value: i128| i64::try_from(value).ok().and_then(Int63::new);
        for a in values {
            let left = Int63::new(a).unwrap();
            assert_eq!(left.checked_neg(), narrow(-i128::from(a)));
            for b in values {
                let right = Int63::new(b).unwrap();
                assert_eq!(
                    left.checked_add(right),
                    narrow(i128::from(a) + i128::from(b))
                );
                assert_eq!(
                    left.checked_sub(right),
                    narrow(i128::from(a) - i128::from(b))
                );
                assert_eq!(
                    left.checked_mul(right),
                    narrow(i128::from(a) * i128::from(b))
                );
                assert_eq!(
                    left.checked_div(right),
                    i128::from(a).checked_div(i128::from(b)).and_then(narrow)
                );
                assert_eq!(
                    left.checked_rem(right),
                    i128::from(a).checked_rem(i128::from(b)).and_then(narrow)
                );
                assert_eq!(left.bit_and(right), Int63::new(a & b).unwrap());
                assert_eq!(left.bit_or(right), Int63::new(a | b).unwrap());
                assert_eq!(left.bit_xor(right), Int63::new(a ^ b).unwrap());
            }
        }
    }
}
