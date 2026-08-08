//! The AOT build ABI: compile CIL to a target's bytes -- a flashable Cortex-M chip image, or a
//! `.wasm` widget -- in the browser. A thin linear-memory binding around `lamella_aot::build::build`,
//! the same one-call pipeline the native `deploy-microbit`/`wasm-program` examples drive. So the
//! in-page IDE turns the user's C# (compiled to CIL by `lamella_compile`) into chip-flashable bytes
//! client-side, with no server round trip; the browser's WebHID/WebUSB then flashes them.

#![allow(unsafe_code)]

use crate::abi::result_buffer;

/// Builds the payload the ABI returns: `[u32 json_len][JSON {error}][u32 image_len][image bytes]`. Split
/// from the `extern "C"` wrapper so the envelope is testable without raw pointers (as `bake` does).
fn build_payload(cil: &[u8], target: &str) -> Vec<u8> {
    let (image, error) = match lamella_aot::build::build(cil, target) {
        Ok(image) => (image, None),
        Err(e) => (Vec::new(), Some(format!("{e:?}"))),
    };
    let json = serde_json::to_vec(&serde_json::json!({ "error": error })).unwrap_or_default();
    let mut payload = Vec::with_capacity(8 + json.len() + image.len());
    payload.extend_from_slice(&(json.len() as u32).to_le_bytes());
    payload.extend_from_slice(&json);
    payload.extend_from_slice(&(image.len() as u32).to_le_bytes());
    payload.extend_from_slice(&image);
    payload
}

/// Compiles the CIL assembly at `cil_ptr..cil_ptr + cil_len` to native bytes for the target named at
/// `target_ptr..target_ptr + target_len`: `"microbit"`/`"rp2350"` (and other chips) emit a flashable ARM
/// boot image, `"wasm"` emits a WebAssembly widget.
///
/// Returns `[u32 json_len][JSON][u32 image_len][image bytes]` (free the whole buffer with
/// `lamella_dealloc(result, 4 + json_len + 4 + image_len)`). `image_len == 0` means the build failed and
/// the JSON's `error` says why: the CIL was unreadable (`Parse`), a method did not lower
/// (`LowerArm(..)`), or the target is unsupported by this build (`UnsupportedTarget`).
///
/// # Safety
/// Both pointer/length pairs must be buffers the host filled via prior `lamella_alloc` calls (a
/// zero-length `target` is allowed and selects no target, i.e. a failed build reported as
/// `UnsupportedTarget`).
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
    result_buffer(build_payload(cil, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the payload's `[u32 json_len][JSON]` head and the trailing image length.
    fn parse(payload: &[u8]) -> (serde_json::Value, usize) {
        let json_len = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
        let json: serde_json::Value = serde_json::from_slice(&payload[4..4 + json_len]).unwrap();
        let image_len =
            u32::from_le_bytes(payload[4 + json_len..8 + json_len].try_into().unwrap()) as usize;
        (json, image_len)
    }

    #[test]
    fn an_unsupported_target_reports_why() {
        let (json, image_len) = parse(&build_payload(b"not a managed assembly", "totally-not-a-chip"));
        assert_eq!(json["error"], "UnsupportedTarget");
        assert_eq!(image_len, 0, "a failed build carries no image");
    }

    #[test]
    fn unreadable_cil_reports_parse() {
        let (json, image_len) = parse(&build_payload(b"not a managed assembly", "rp2350"));
        assert_eq!(json["error"], "Parse");
        assert_eq!(image_len, 0);
    }

    #[test]
    fn a_real_assembly_reports_the_construct_that_did_not_lower() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wire-host/tests/fixtures/hello.exe"
        );
        let Ok(cil) = std::fs::read(fixture) else {
            return;
        };
        let (json, image_len) = parse(&build_payload(&cil, "rp2350"));
        assert_eq!(image_len, 0);
        assert!(
            json["error"].as_str().unwrap().contains("CallUnsupported"),
            "expected a lowering reason, got {}",
            json["error"]
        );
    }
}
