//! The AOT build ABI: compile CIL to a target's bytes -- a flashable Cortex-M chip image, or a
//! `.wasm` widget -- in the browser. A thin linear-memory binding around `lamella_aot::build::build`,
//! the same one-call pipeline the native `deploy-microbit`/`wasm-program` examples drive. So the
//! in-page IDE turns the user's C# (compiled to CIL by `lamella_compile`) into chip-flashable bytes
//! client-side, with no server round trip; the browser's WebHID/WebUSB then flashes them.

#![allow(unsafe_code)]

use crate::abi::result_buffer;

/// Compiles the CIL assembly at `cil_ptr..cil_ptr + cil_len` to native bytes for the target named at
/// `target_ptr..target_ptr + target_len`: `"microbit"` (and other chips) emit a flashable ARM boot
/// image, `"wasm"` emits a WebAssembly widget. Returns a `[u32 little-endian length][image bytes]`
/// buffer (free with `lamella_dealloc(result, 4 + length)`); a ZERO length means the build failed --
/// the CIL was unreadable, a method did not lower, or the target is unsupported by this build.
///
/// # Safety
/// Both pointer/length pairs must be buffers the host filled via prior `lamella_alloc` calls (a
/// zero-length `target` is allowed and selects no target, i.e. a failed build).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_aot_build(
    cil_ptr: *const u8,
    cil_len: usize,
    target_ptr: *const u8,
    target_len: usize,
) -> *mut u8 {
    let cil = unsafe { core::slice::from_raw_parts(cil_ptr, cil_len) };
    let target_bytes: &[u8] = if target_len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(target_ptr, target_len) }
    };
    let target = core::str::from_utf8(target_bytes).unwrap_or("");
    let image = lamella_aot::build::build(cil, target).unwrap_or_default();
    result_buffer(image)
}
