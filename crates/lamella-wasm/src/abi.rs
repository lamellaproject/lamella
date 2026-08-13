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

/// Runs the managed assembly at `ptr..ptr + len` WITH the managed corlib at
/// `corlib_ptr..corlib_ptr + corlib_len` loaded alongside it. Same result buffer as [`lamella_run`]:
/// `[u32 little-endian length][UTF-8 JSON]`, freed with `lamella_dealloc(result, 4 + length)`.
///
/// **Prefer this over [`lamella_run`] whenever the host has a corlib.** A corlib-less run resolves only
/// what the loader intrinsic-binds, which covers enough (`Console.WriteLine`, `String.ToUpper`,
/// `Math.Max`) to look complete while any MANAGED corlib method -- `Thread.Sleep` is the one a user hit
/// -- resolves to nothing and traps mid-run. Passing a corlib is strictly more resolving power: the
/// loader falls back to an intrinsic only where the corlib's name index has no match.
///
/// [`lamella_run`] is kept for hosts that have no corlib to hand, and is unchanged.
///
/// # Safety
/// Both pointer/length pairs must be buffers the host filled via prior [`lamella_alloc`] calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_run_with_corlib(
    corlib_ptr: *const u8,
    corlib_len: usize,
    ptr: *const u8,
    len: usize,
) -> *mut u8 {
    let corlib = unsafe { core::slice::from_raw_parts(corlib_ptr, corlib_len) };
    let assembly = unsafe { core::slice::from_raw_parts(ptr, len) };
    result_buffer(to_json(&crate::run_bytes_with_corlib(corlib, assembly)).into_bytes())
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

/// Compiles the Python source at `ptr..ptr + len` (UTF-8) into the DEPLOYABLE BUNDLE BYTES -- the
/// versioned `LPYC` container a device loads through the wire's `RUN_BUNDLE` / `DEPLOY_BUNDLE` ops --
/// returning `[u32 little-endian length][bundle bytes]`; free it with
/// `lamella_dealloc(result, 4 + length)`. Behind the `py` feature.
///
/// **A ZERO length means the program did not compile**, and that is the only failure this call has. The
/// caller gets the REASON by calling [`lamella_py_check`] on the same source -- both go through the same
/// private `compile`, so they cannot disagree about why. Keeping the binary result binary avoids
/// base64-inflating a payload whose whole purpose is to cross a wire.
///
/// This is the seam that lets a browser hand Python to a BOARD rather than only run it in the page:
/// it is the only way a caller out here obtains a `Bundle` for the encoder and the chunked deploy.
///
/// # Safety
/// `ptr`/`len` must be the UTF-8 buffer the host filled via a prior [`lamella_alloc`].
#[cfg(feature = "py")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_py_bundle(ptr: *const u8, len: usize) -> *mut u8 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    let source = core::str::from_utf8(bytes).unwrap_or("");
    result_buffer(crate::py::bundle_py_str(source))
}

/// Completions (IntelliSense) for the caret at byte `offset` in the Python at
/// `ptr..ptr + len` (UTF-8), returning `[u32 length][UTF-8 JSON]`
/// (`{items:[{label, kind, detail, insertText}]}`); free it with
/// `lamella_dealloc(result, 4 + length)`. Behind the `py` feature.
///
/// The C# twin [`crate::compile::lamella_complete`] additionally takes a packed reference-assembly
/// buffer; Python needs none, because the only modules a program here can import are the ones this
/// blob already carries. **An empty item list is the normal answer for a caret with nothing to
/// suggest, not an error** -- this call has no failure mode.
///
/// # Safety
/// `ptr`/`len` must be the UTF-8 buffer the host filled via a prior [`lamella_alloc`].
#[cfg(feature = "py")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_py_complete(
    ptr: *const u8,
    len: usize,
    offset: usize,
) -> *mut u8 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    let source = core::str::from_utf8(bytes).unwrap_or("");
    result_buffer(crate::py::complete_py_str(source, offset).into_bytes())
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

/// Runs `f` over `bytes` staged in a leaked `'static` buffer, then reclaims the buffer. The loader's
/// `code-in-place` seam pins loads to `Assembly<'static>` even though a run/bake confines the borrow
/// to `f` (which returns an OWNED value) -- this bridges a caller's borrowed bytes to that
/// requirement without a permanent per-call leak. `pub(crate)`: its callers here return an owned
/// `RunResult`/`Vec`, so nothing borrows the staged buffer past `f`.
pub(crate) fn with_static<T>(bytes: &[u8], f: impl FnOnce(&'static [u8]) -> T) -> T {
    let staged: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
    let result = f(staged);
    unsafe {
        drop(Box::from_raw(core::ptr::from_ref::<[u8]>(staged).cast_mut()));
    }
    result
}

/// Splits the references buffer (`[u32 count]` then `count` x `[u32 len][bytes]`) into the
/// individual assembly byte slices; stops at the first malformed length. Shared by the compile ABI,
/// the REPL ABI (which packs its compile references the same way) and the multi-library bake -- so
/// it lives here, ungated, rather than behind any one feature.
pub(crate) fn split_refs(refs: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let Some(count) = refs
        .get(0..4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    else {
        return out;
    };
    let mut offset = 4usize;
    for _ in 0..count {
        let Some(len) = refs
            .get(offset..offset + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
        else {
            break;
        };
        offset += 4;
        let Some(blob) = refs.get(offset..offset + len) else {
            break;
        };
        out.push(blob);
        offset += len;
    }
    out
}

/// [`with_static`] for a LIST of buffers -- stages every one, runs `f`, then reclaims them all.
/// A driver-stack bake loads several library assemblies at once, and nesting `with_static` cannot
/// express a count known only at run time. Same contract and same safety argument as the singular
/// form: `f` returns an OWNED value, so nothing borrows the staged buffers once it has returned.
pub(crate) fn with_static_all<T>(buffers: &[&[u8]], f: impl FnOnce(&[&'static [u8]]) -> T) -> T {
    let staged: Vec<&'static [u8]> = buffers
        .iter()
        .map(|bytes| &*Box::leak(bytes.to_vec().into_boxed_slice()))
        .collect();
    let result = f(&staged);
    for bytes in staged {
        unsafe {
            drop(Box::from_raw(core::ptr::from_ref::<[u8]>(bytes).cast_mut()));
        }
    }
    result
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
