//! The AOT build ABI: compile CIL to a target's bytes -- a flashable Cortex-M chip image, or a
//! `.wasm` widget -- in the browser. A thin linear-memory binding around `lamella_aot::build::build`,
//! the same one-call pipeline the native `deploy-microbit`/`wasm-program` examples drive. So the
//! in-page IDE turns the user's C# (compiled to CIL by `lamella_compile`) into chip-flashable bytes
//! client-side, with no server round trip; the browser's WebHID/WebUSB then flashes them.
//!
//! `lamella_aot_build_linked` is the class-library tier, the one a board's program reaches: the
//! program linked with corlib, the libraries it references and the board's runtime-support
//! archive, with the board's bus table laid in, through the entry point the command line calls
//! (`lamella_aot::build::build_linked_cortex_m_with_board`).

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
    envelope(&json, &image)
}

/// Builds the payload the class-library ABI returns: the same envelope, its JSON
/// `{ error, busTable }`. `error` is `null`, or the build's own sentence (what the command line
/// prints for the same failure); `busTable` says whether a bus table was laid into the image.
///
/// A build that names no board lays no table, and neither does a board this build has no table
/// for: one whose family has none, or an id this build does not know. Every bus then reads as
/// absent, so `busTable` is how a caller sees that its board id found nothing.
///
/// An `archive` that is not an archive is refused before anything is lowered, naming the archive:
/// the build itself would report it as an object it had produced and could not read back.
fn build_linked_payload(
    cil: &[u8],
    corlib: &[u8],
    libraries: &[&[u8]],
    archive: &[u8],
    target: &str,
    board: &str,
) -> Vec<u8> {
    let board_buses = if board.is_empty() {
        None
    } else {
        lamella_catalog::bus_table(board)
    };
    let built = match lamella_elf::read_archive(archive) {
        Err(error) => Err(format!(
            "the runtime-support archive given is not an archive: {error:?}"
        )),
        Ok(_) => lamella_aot::build::build_linked_cortex_m_with_board(
            cil,
            corlib,
            libraries,
            archive,
            target,
            board_buses,
        )
        .map_err(|e| format!("{e}")),
    };
    let (image, error) = match built {
        Ok(image) => (image, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let json = serde_json::to_vec(&serde_json::json!({
        "error": error,
        "busTable": board_buses.is_some(),
    }))
    .unwrap_or_default();
    envelope(&json, &image)
}

/// `[u32 json_len][json][u32 image_len][image]`, the layout both builds return.
fn envelope(json: &[u8], image: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8 + json.len() + image.len());
    payload.extend_from_slice(&(json.len() as u32).to_le_bytes());
    payload.extend_from_slice(json);
    payload.extend_from_slice(&(image.len() as u32).to_le_bytes());
    payload.extend_from_slice(image);
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

/// Compiles the CIL assembly at `cil_ptr..cil_ptr + cil_len` to a board's class-library image: the
/// program lowered and linked with the corlib, the libraries it references and the board's
/// runtime-support archive, then wrapped in the part's boot image -- the image the command line
/// writes for a board on the class-library tier.
///
/// - `libs` is `[u32 count]` then `count` x `[u32 len][.dll bytes]`, the packing the compile ABI
///   uses, in the order the resolver searches them. Only the libraries the program references,
///   directly or through one another, are linked, so a host may pass every library it compiled
///   against. Zero-length passes none.
/// - `archive` is the runtime-support archive built for the board (`liblamella_runtime_support.a`).
/// - `target` is the AOT target, e.g. `"rp2350"`.
/// - `board` is the board's id; its bus table is laid into the image for `System.Device`'s
///   factories. Zero-length names no board.
///
/// Returns `[u32 json_len][JSON][u32 image_len][image bytes]` (free the whole buffer with
/// `lamella_dealloc(result, 4 + json_len + 4 + image_len)`). The JSON is `{ error, busTable }`:
/// `image_len == 0` means the build failed and `error` says why; `busTable` says whether the board's
/// bus table was laid.
///
/// # Safety
/// Each pointer/length pair must be a buffer the host filled via a prior `lamella_alloc`
/// (zero-length `libs` and `board` are allowed).
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_aot_build_linked(
    cil_ptr: *const u8,
    cil_len: usize,
    corlib_ptr: *const u8,
    corlib_len: usize,
    libs_ptr: *const u8,
    libs_len: usize,
    archive_ptr: *const u8,
    archive_len: usize,
    target_ptr: *const u8,
    target_len: usize,
    board_ptr: *const u8,
    board_len: usize,
) -> *mut u8 {
    let read = |ptr: *const u8, len: usize| -> &[u8] {
        if len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(ptr, len) }
        }
    };
    let libraries = crate::abi::split_refs(read(libs_ptr, libs_len));
    let target = core::str::from_utf8(read(target_ptr, target_len)).unwrap_or("");
    let board = core::str::from_utf8(read(board_ptr, board_len)).unwrap_or("");
    result_buffer(build_linked_payload(
        read(cil_ptr, cil_len),
        read(corlib_ptr, corlib_len),
        &libraries,
        read(archive_ptr, archive_len),
        target,
        board,
    ))
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
    fn a_linked_build_for_an_unsupported_target_reports_why() {
        let (json, image_len) = parse(&build_linked_payload(
            b"not a managed assembly",
            b"",
            &[],
            EMPTY_ARCHIVE,
            "totally-not-a-chip",
            "",
        ));
        assert_eq!(
            json["error"],
            "the target named is not one this build supports"
        );
        assert_eq!(
            json["busTable"], false,
            "a build naming no board lays no table"
        );
        assert_eq!(image_len, 0);
    }

    #[test]
    fn a_linked_build_says_whether_the_boards_bus_table_was_laid() {
        let laid = |board: &str| {
            let (json, _) = parse(&build_linked_payload(
                b"not a managed assembly",
                b"",
                &[],
                EMPTY_ARCHIVE,
                "rp2350",
                board,
            ));
            json["busTable"].clone()
        };
        assert_eq!(laid("rpi-pico2"), true);
        assert_eq!(laid("pimoroni-pico-plus-2-w"), true);
        assert_eq!(laid("bbc-micro-bit-v2"), false, "a family with no table");
        assert_eq!(
            laid("rpi-pico2-typo"),
            false,
            "an id this build does not know"
        );
        assert_eq!(laid(""), false, "no board");
    }

    const EMPTY_ARCHIVE: &[u8] = b"!<arch>\n";

    #[test]
    fn a_linked_build_refuses_an_archive_that_is_not_one() {
        let (json, image_len) = parse(&build_linked_payload(
            b"not a managed assembly",
            b"",
            &[],
            b"<!doctype html>",
            "rp2350",
            "rpi-pico2",
        ));
        assert_eq!(image_len, 0);
        let error = json["error"].as_str().unwrap();
        assert!(
            error.starts_with("the runtime-support archive given is not an archive"),
            "got {error}"
        );
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
