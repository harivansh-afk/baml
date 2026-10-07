//! Statically callable array operations for generated code. They use the same
//! BEX storage and locks as bytecode, without a runtime trait call per element.

use baml_type::Int63;

use crate::{
    Object, PermitProof, RealizedTy, Value,
    array_index::resolve_index,
    errors::{VmInternalError, VmPanic, VmRustFnError},
    types::Array,
};

/// The caller's permit protects the object, while its generated state roots it.
#[inline]
#[allow(unsafe_code, reason = "raw heap access bounded by the caller's permit")]
unsafe fn array(value: Value, _permit: PermitProof<'_>) -> Result<&Array, VmInternalError> {
    let invalid = || VmInternalError::InvalidCompiledCode {
        message: "expected int[] in compiled code".into(),
    };
    let pointer = value.as_object_ptr().ok_or_else(invalid)?;
    // SAFETY: callers require a current, live value under `_permit`. Only the
    // immutable object header is borrowed; contents use the container lock.
    match unsafe { pointer.get() } {
        Object::Array(array) if *array.element_ty == RealizedTy::Int => Ok(array),
        _ => Err(invalid()),
    }
}

/// Validate an array at a compiled call boundary without copying its elements.
///
/// # Safety
/// `value` must be current and live in the heap protected by `permit`.
#[inline]
#[allow(
    unsafe_code,
    reason = "the runtime roots arguments under the supplied permit"
)]
pub unsafe fn check_int_array(
    value: Value,
    permit: PermitProof<'_>,
) -> Result<Value, VmInternalError> {
    // SAFETY: inherited from this function's caller.
    unsafe { array(value, permit)? };
    Ok(value)
}

/// Read the current length without retaining heap storage across a checkpoint.
///
/// # Safety
/// `value` must be current and live in the heap protected by `permit`. Generated
/// callers keep it in traced state (or newly allocated under the same permit).
#[inline]
#[allow(
    unsafe_code,
    reason = "generated callers root values under the supplied permit"
)]
pub unsafe fn int_array_len(value: Value, permit: PermitProof<'_>) -> Result<Int63, VmRustFnError> {
    // SAFETY: inherited from this function's caller.
    let length = unsafe { array(value, permit)? }.len();
    i64::try_from(length)
        .ok()
        .and_then(Int63::new)
        .ok_or_else(|| {
            VmInternalError::InvalidCompiledCode {
                message: "array length exceeds int range".into(),
            }
            .into()
        })
}

/// Read an integer using BAML's index and error rules.
///
/// # Safety
/// `value` must be current and live in the heap protected by `permit`.
#[inline(always)]
#[allow(
    clippy::inline_always,
    reason = "expose each hot array operation to the generated loop optimizer"
)]
#[allow(
    unsafe_code,
    reason = "generated callers root values under the supplied permit"
)]
pub unsafe fn int_array_get(
    value: Value,
    index: Int63,
    permit: PermitProof<'_>,
) -> Result<Int63, VmRustFnError> {
    // SAFETY: inherited from this function's caller. The guard stays local.
    let guard = unsafe { array(value, permit)? }.lock();
    let offset = resolve_index(index.get(), guard.len()).ok_or(VmPanic::IndexOutOfBounds {
        index: index.get(),
        length: guard.len(),
    })?;
    Ok(super::read_int(guard[offset])?)
}

/// Replace one integer under the array's existing lock. The slice access cannot
/// resize backing storage; integer stores create no GC reference or payload debt.
///
/// # Safety
/// `array_value` must be current and live in the heap protected by `permit`.
#[inline(always)]
#[allow(
    clippy::inline_always,
    reason = "expose each hot array operation to the generated loop optimizer"
)]
#[allow(
    unsafe_code,
    reason = "generated callers root values under the supplied permit"
)]
pub unsafe fn int_array_set(
    array_value: Value,
    index: Int63,
    value: Int63,
    permit: PermitProof<'_>,
) -> Result<(), VmRustFnError> {
    // SAFETY: inherited from this function's caller. Neither the slice nor its
    // lock escapes, including on the bounds-error path.
    unsafe { array(array_value, permit)? }
        .data
        .with_slice_mut(|elements| {
            let offset =
                resolve_index(index.get(), elements.len()).ok_or(VmPanic::IndexOutOfBounds {
                    index: index.get(),
                    length: elements.len(),
                })?;
            // An Int63 cannot contain a heap pointer, so the ordinary reference
            // write barrier would do nothing. Heap-valued stores must use it.
            elements[offset] = Value::int(value.get());
            Ok(())
        })
}
