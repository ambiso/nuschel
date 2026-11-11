// SPDX-License-Identifier: EUPL-1.2
//! Copies out of memory Windows returned: the only pointer reads in this
//! crate that are not a Win32 call. Each takes a pointer and a length (or
//! reads to a NUL) and returns owned data, so nothing borrows OS memory
//! beyond the call. Tested under Miri on buffers built in the tests and on
//! the mock's allocations.
#![allow(unsafe_code)]

use std::ffi::c_void;

/// # Safety
/// `source` is valid for reads of `length` bytes, initialised, and not
/// written during the call. It needs no alignment.
pub(crate) unsafe fn copy_bytes(source: *const c_void, length: usize) -> Vec<u8> {
    if length == 0 {
        return Vec::new();
    }
    // SAFETY: the caller guarantees `length` readable, initialised bytes at
    // `source`; u8 has alignment 1; the slice lives only for this statement,
    // during which nothing writes the memory (caller), and `to_vec` copies it.
    unsafe { std::slice::from_raw_parts(source.cast::<u8>(), length) }.to_vec()
}

/// A UTF-16 string up to (not including) its NUL.
///
/// # Safety
/// `text` is non-null, aligned for `u16`, and every unit up to and including
/// the first NUL is readable and initialised and not written during the call.
pub(crate) unsafe fn copy_wide_until_nul(text: *const u16) -> Vec<u16> {
    let mut units = Vec::new();
    let mut next = text;
    loop {
        // SAFETY: `next` is `text` plus the number of non-NUL units read so
        // far; every unit up to and including the NUL is readable and
        // aligned (caller), and the loop stops at the NUL, so this never
        // reads past it.
        let unit = unsafe { next.read() };
        if unit == 0 {
            return units;
        }
        units.push(unit);
        // Pointer arithmetic only; dereferenced in the next iteration under
        // the reasoning above.
        next = next.wrapping_add(1);
    }
}

/// The `count` pointers of an array.
///
/// # Safety
/// `array` is valid for reads of `count` initialised, aligned `T`s (or
/// `count` is 0), not written during the call.
pub(crate) unsafe fn copy_array<T: Copy>(array: *const T, count: usize) -> Vec<T> {
    if count == 0 {
        return Vec::new();
    }
    // SAFETY: the caller guarantees `count` readable, initialised, aligned
    // elements; the slice lives only while they are copied.
    unsafe { std::slice::from_raw_parts(array, count) }.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_bytes_strings_and_arrays() {
        let bytes = [1_u8, 2, 3, 4, 5];
        // SAFETY: `bytes` is 5 readable bytes for the whole call.
        let copied = unsafe { copy_bytes(bytes.as_ptr().cast(), 3) };
        assert_eq!(copied, [1, 2, 3]);
        // SAFETY: a zero length reads nothing, whatever the pointer.
        assert!(unsafe { copy_bytes(std::ptr::null(), 0) }.is_empty());

        let text: Vec<u16> = "AC\\Temp\0garbage".encode_utf16().collect();
        // SAFETY: `text` holds a NUL after "AC\Temp"; the rest is never read.
        let read = unsafe { copy_wide_until_nul(text.as_ptr()) };
        assert_eq!(String::from_utf16(&read).unwrap(), "AC\\Temp");
        let empty = [0_u16];
        // SAFETY: a lone NUL.
        assert!(unsafe { copy_wide_until_nul(empty.as_ptr()) }.is_empty());

        let pointers = [1_usize, 2, 3];
        // SAFETY: three readable elements.
        assert_eq!(unsafe { copy_array(pointers.as_ptr(), 2) }, [1, 2]);
        // SAFETY: count 0 reads nothing.
        assert!(unsafe { copy_array::<usize>(std::ptr::null(), 0) }.is_empty());
    }
}
