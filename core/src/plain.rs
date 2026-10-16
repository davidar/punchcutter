//! Types that may be reinterpreted as their own bytes, and the three views
//! that do it.
//!
//! Every upload to the GPU and every readback from it passes through here, so
//! this is where the unsafe lives — three functions, each a `from_raw_parts`
//! or a `copy_nonoverlapping` whose soundness rests entirely on the [`Plain`]
//! contract. The ABI structs themselves stay in their own crates
//! (`punchcutter-text`'s `layout` module, `punchcutter-shape`'s `abi` module):
//! the layouts are different contracts and only the trait is shared.

/// Types that may be reinterpreted as their own bytes for GPU upload or
/// filled from a mapped readback.
///
/// # Safety
///
/// An implementor must be `#[repr(C)]`, contain no padding bytes anywhere in
/// its layout, and have no invalid bit patterns (so: only `u32`/`f32`/`i32`
/// leaves and arrays thereof — no `bool`, no enums, no references). The layout tests (`text/tests/layout.rs`, `shape/tests/layout.rs`) pin the
/// "no padding" half of that claim by asserting each field offset and each
/// struct size against slangc's own reflection.
pub unsafe trait Plain: Copy + Sized {}

/// Borrow a value as the bytes an upload would send.
pub fn bytes_of<T: Plain>(value: &T) -> &[u8] {
    // SAFETY: `Plain` promises no padding and no invalid bit patterns, so
    // every byte of `T` is initialized and readable as `u8`; the lifetime is
    // tied to the borrow, and `u8` has alignment 1.
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

/// Borrow a slice of values as one contiguous byte run. Sound because `Plain`
/// types have no padding, hence no inter-element gaps either.
pub fn bytes_of_slice<T: Plain>(values: &[T]) -> &[u8] {
    // SAFETY: as `bytes_of`, extended over `values.len()` contiguous elements.
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of_val(values)) }
}

/// Overwrite `value` from `bytes`, which must be exactly its size.
///
/// Returns `false` (leaving `value` untouched) on a length mismatch, so a
/// readback of the wrong size is the caller's to report rather than a panic.
#[must_use]
pub fn write_from_bytes<T: Plain>(value: &mut T, bytes: &[u8]) -> bool {
    if bytes.len() != size_of::<T>() {
        return false;
    }
    // SAFETY: sizes match exactly; `Plain` says every bit pattern of `T` is
    // valid, so any `size_of::<T>()` source bytes produce a valid `T`. The
    // source is a `&[u8]` (alignment 1, no aliasing with `value`, which is a
    // unique borrow), and an unaligned read is used on the source even though
    // a mapped wgpu buffer is aligned anyway.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), (value as *mut T).cast::<u8>(), bytes.len());
    }
    true
}

#[cfg(test)]
mod tests;
