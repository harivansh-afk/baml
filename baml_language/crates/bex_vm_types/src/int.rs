//! Catchable integer operations shared by interpreted and compiled BAML.
//!
//! `Int63` owns the value invariant and arithmetic. This module translates
//! failures into the runtime's panic payloads, without allocating on success.

pub use baml_type::Int63;
use baml_type::IntShiftError;

use crate::{Value, errors::VmPanic};

#[inline]
pub fn check(value: i64) -> Result<Int63, VmPanic> {
    Int63::new(value).ok_or_else(|| VmPanic::IntegerOverflow {
        message: format!("{value} overflows int"),
    })
}

#[inline]
pub fn add(left: Int63, right: Int63) -> Result<Int63, VmPanic> {
    finish(left.checked_add(right), left, '+', right)
}

#[inline]
pub fn sub(left: Int63, right: Int63) -> Result<Int63, VmPanic> {
    finish(left.checked_sub(right), left, '-', right)
}

#[inline]
pub fn mul(left: Int63, right: Int63) -> Result<Int63, VmPanic> {
    finish(left.checked_mul(right), left, '*', right)
}

#[inline]
pub fn div(left: Int63, right: Int63) -> Result<Int63, VmPanic> {
    if right == Int63::ZERO {
        return Err(division_by_zero(left, right));
    }
    finish(left.checked_div(right), left, '/', right)
}

#[inline]
pub fn rem(left: Int63, right: Int63) -> Result<Int63, VmPanic> {
    left.checked_rem(right)
        .ok_or_else(|| division_by_zero(left, right))
}

#[inline]
pub fn neg(value: Int63) -> Result<Int63, VmPanic> {
    value.checked_neg().ok_or_else(|| VmPanic::IntegerOverflow {
        message: format!("-({}) overflows int", value.get()),
    })
}

#[inline]
pub fn shl(left: Int63, right: Int63) -> Result<Int63, VmPanic> {
    left.shift_left(right.get()).map_err(shift_error)
}

#[inline]
pub fn shr(left: Int63, right: Int63) -> Result<Int63, VmPanic> {
    left.shift_right(right.get()).map_err(shift_error)
}

#[inline]
fn finish(value: Option<Int63>, left: Int63, op: char, right: Int63) -> Result<Int63, VmPanic> {
    value.ok_or_else(|| overflow(left, op, right))
}

#[cold]
pub fn overflow(left: Int63, op: char, right: Int63) -> VmPanic {
    VmPanic::IntegerOverflow {
        message: format!("{} {op} {} overflows int", left.get(), right.get()),
    }
}

#[cold]
fn division_by_zero(left: Int63, right: Int63) -> VmPanic {
    VmPanic::DivisionByZero {
        left: Value::int(left.get()),
        right: Value::int(right.get()),
    }
}

#[cold]
fn shift_error(error: IntShiftError) -> VmPanic {
    let IntShiftError::NegativeCount(count) = error;
    VmPanic::NegativeBitShift {
        message: format!("bit shift count is negative: {count}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_payloads_are_language_errors() {
        let one = check(1).unwrap();
        assert_eq!(
            add(Int63::MAX, one),
            Err(VmPanic::IntegerOverflow {
                message: "4611686018427387903 + 1 overflows int".into(),
            })
        );
        assert_eq!(
            div(Int63::MIN, check(-1).unwrap()),
            Err(VmPanic::IntegerOverflow {
                message: "-4611686018427387904 / -1 overflows int".into(),
            })
        );
        assert_eq!(rem(Int63::MIN, check(-1).unwrap()), Ok(Int63::ZERO));
        for operation in [div, rem] {
            assert_eq!(
                operation(one, Int63::ZERO),
                Err(VmPanic::DivisionByZero {
                    left: Value::int(1),
                    right: Value::int(0),
                })
            );
        }
        assert_eq!(
            shl(one, check(-1).unwrap()),
            Err(VmPanic::NegativeBitShift {
                message: "bit shift count is negative: -1".into(),
            })
        );
    }
}
