//! The WebAssembly linear-memory ABI: the functions a host calls.

#![allow(unsafe_code)]

use crate::{run_bytes, to_json};

/// Reserves `len` bytes in the module's linear memory and returns the offset to
/// write to. Free it later with [`lamella_dealloc`].
#[unsafe(no_mangle)]
pub extern "C" fn lamella_alloc(len: usize) -> *mut u8 {
    let boxed = vec![0u8; len].into_boxed_slice();
    Box::into_raw(boxed) as *mut u8
}

/// Frees a buffer of `len` bytes previously returned by [`lamella_alloc`] or
/// [`lamella_run`].
///
/// # Safety
/// `ptr`/`len` must be a buffer previously returned by [`lamella_alloc`] (passing
/// its `len`) or [`lamella_run`] (passing `4 + length`), not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_dealloc(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    unsafe {
        drop(Box::from_raw(core::ptr::slice_from_raw_parts_mut(ptr, len)));
    }
}

/// Runs the managed assembly whose bytes are at `ptr..ptr + len`. Returns a buffer
/// laid out as `[u32 little-endian length][UTF-8 JSON]`; read the length, then the
/// JSON, then free it with `lamella_dealloc(result, 4 + length)`.
///
/// # Safety
/// `ptr`/`len` must be the buffer the host filled via a prior [`lamella_alloc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_run(ptr: *const u8, len: usize) -> *mut u8 {
    let assembly = unsafe { core::slice::from_raw_parts(ptr, len) };
    let json = to_json(&run_bytes(assembly)).into_bytes();

    let length = u32::try_from(json.len()).unwrap_or(u32::MAX);
    let mut buffer = Vec::with_capacity(4 + json.len());
    buffer.extend_from_slice(&length.to_le_bytes());
    buffer.extend_from_slice(&json);
    Box::into_raw(buffer.into_boxed_slice()) as *mut u8
}

/// Whether the runtime is ready -- always true once the module is instantiated.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_is_ready() -> i32 {
    1
}

/// Runs an embedded fixture and returns its exit code, so a wasm host can validate
/// the whole interpret-in-wasm path with one `--invoke` and no memory marshaling.
/// The fixture `arith.dll` returns 5. Behind the `selftest` feature; not shipped.
#[cfg(feature = "selftest")]
#[unsafe(no_mangle)]
pub extern "C" fn lamella_selftest() -> i32 {
    crate::run_bytes(include_bytes!(
        "../../lamella-load/tests/fixtures/arith.dll"
    ))
    .exit_code
}
