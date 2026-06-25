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
    result_buffer(to_json(&run_bytes(assembly)).into_bytes())
}

/// Compiles the Python source at `ptr..ptr + len` (UTF-8) and runs its `main()`,
/// returning `[u32 little-endian length][UTF-8 JSON]` (`{stdout, exitCode, diagnostics}`)
/// like [`lamella_run`]; free it with `lamella_dealloc(result, 4 + length)`. Behind the
/// `py` feature.
///
/// # Safety
/// `ptr`/`len` must be the UTF-8 buffer the host filled via a prior [`lamella_alloc`].
#[cfg(feature = "py")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_py_run(ptr: *const u8, len: usize) -> *mut u8 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    let source = core::str::from_utf8(bytes).unwrap_or("");
    result_buffer(to_json(&crate::py::run_py_str(source)).into_bytes())
}

/// Compile-CHECKS the Python source at `ptr..ptr + len` (UTF-8) WITHOUT running it, returning
/// `[u32 length][UTF-8 JSON]` (`{stdout:"", exitCode, diagnostics}`) like [`lamella_run`] -- the
/// editor / LSP diagnostics path. Free with `lamella_dealloc(result, 4 + length)`. Behind `py`.
///
/// # Safety
/// `ptr`/`len` must be the UTF-8 buffer the host filled via a prior [`lamella_alloc`].
#[cfg(feature = "py")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_py_check(ptr: *const u8, len: usize) -> *mut u8 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    let source = core::str::from_utf8(bytes).unwrap_or("");
    result_buffer(to_json(&crate::py::check_py_str(source)).into_bytes())
}

/// Packages `bytes` into a freshly allocated `[u32 little-endian length][bytes]`
/// buffer and returns a pointer to it; the host reads the length, then the bytes,
/// then frees it with `lamella_dealloc(result, 4 + length)`. Shared by the run and
/// DAP results.
pub(crate) fn result_buffer(bytes: Vec<u8>) -> *mut u8 {
    let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    let mut buffer = Vec::with_capacity(4 + bytes.len());
    buffer.extend_from_slice(&length.to_le_bytes());
    buffer.extend_from_slice(&bytes);
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
