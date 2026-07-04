//! In-page BAKING (feature `bake`): a wasm ABI over the loader's `write_baked`, so the browser IDE
//! (Studio) turns a compiled app plus its assemblies into a self-contained `.lmli` flash image
//! client-side -- the deploy half of the browser hardware story (compile in-page -> bake in-page ->
//! deploy over WebSerial). It mirrors the host `bake-library-image` example: load (unfrozen) ->
//! optionally trim to the reachable set -> validate the capability profile -> `write_baked`.

#![allow(unsafe_code)]

use lamella_load::{load_with_corlib_and_library_unfrozen, load_with_corlib_unfrozen};
use lamella_metadata::Assembly;

use crate::abi::result_buffer;

/// A `[u32 json_len][JSON][u32 0]` payload carrying an error and no image.
fn error_payload(message: &str) -> Vec<u8> {
    let json = serde_json::to_vec(&serde_json::json!({
        "violations": [],
        "error": message,
        "kept": serde_json::Value::Null,
    }))
    .unwrap_or_default();
    let mut payload = Vec::with_capacity(8 + json.len());
    payload.extend_from_slice(&(json.len() as u32).to_le_bytes());
    payload.extend_from_slice(&json);
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload
}

/// Loads corlib (+ optional library) + app, trims + validates, and bakes -- returning the payload
/// described in the module doc. `code-in-place` pins the loader to `Assembly<'static>`, so the
/// inputs are staged via [`crate::abi::with_static`] (leaked, then reclaimed once the OWNED image is
/// produced) -- a repeated in-page bake does not leak.
fn bake(corlib: &[u8], library: &[u8], app: &[u8], trim: bool) -> Vec<u8> {
    crate::abi::with_static(corlib, |corlib| {
        crate::abi::with_static(library, |library| {
            crate::abi::with_static(app, |app| bake_inner(corlib, library, app, trim))
        })
    })
}

/// The bake itself, over `'static` assembly bytes (the `code-in-place` load requires it). Every
/// borrow of the inputs is confined here, so the caller can reclaim them once this returns.
fn bake_inner(corlib: &'static [u8], library: &'static [u8], app: &'static [u8], trim: bool) -> Vec<u8> {
    let corlib_asm = match Assembly::read(corlib) {
        Ok(assembly) => assembly,
        Err(error) => return error_payload(&format!("corlib parse: {error:?}")),
    };
    let app_asm = match Assembly::read(app) {
        Ok(assembly) => assembly,
        Err(error) => return error_payload(&format!("app parse: {error:?}")),
    };
    let library_asm = if library.is_empty() {
        None
    } else {
        match Assembly::read(library) {
            Ok(assembly) => Some(assembly),
            Err(error) => return error_payload(&format!("library parse: {error:?}")),
        }
    };

    let loaded = match &library_asm {
        Some(library_asm) => {
            load_with_corlib_and_library_unfrozen(&corlib_asm, library_asm, &app_asm)
        }
        None => load_with_corlib_unfrozen(&corlib_asm, &app_asm),
    };
    let mut loaded = match loaded {
        Ok(loaded) => loaded,
        Err(error) => return error_payload(&format!("load: {error:?}")),
    };

    let (violations, kept) = if trim {
        let (methods, types, strings) = loaded.module.reachable_set(Some(loaded.entry));
        let kept_methods = methods.iter().filter(|keep| **keep).count();
        let kept_types = types.iter().filter(|keep| **keep).count();
        let violations = loaded.module.validate_profile(Some(&methods));
        loaded.module.retain_reachable(&methods, &types, &strings);
        (violations, Some((kept_methods, methods.len(), kept_types, types.len())))
    } else {
        (loaded.module.validate_profile(None), None)
    };

    let image = if violations.is_empty() {
        match loaded.module.write_baked(Some(loaded.entry)) {
            Ok(image) => image,
            Err(error) => return error_payload(&format!("bake: {error:?}")),
        }
    } else {
        Vec::new()
    };

    let violation_strings: Vec<String> = violations.iter().map(|v| format!("{v}")).collect();
    let kept_json = kept.map(|(methods, methods_total, types, types_total)| {
        serde_json::json!({
            "methods": methods,
            "methodsTotal": methods_total,
            "types": types,
            "typesTotal": types_total,
        })
    });
    let json = serde_json::to_vec(&serde_json::json!({
        "violations": violation_strings,
        "error": serde_json::Value::Null,
        "kept": kept_json,
    }))
    .unwrap_or_default();

    let mut payload = Vec::with_capacity(8 + json.len() + image.len());
    payload.extend_from_slice(&(json.len() as u32).to_le_bytes());
    payload.extend_from_slice(&json);
    payload.extend_from_slice(&(image.len() as u32).to_le_bytes());
    payload.extend_from_slice(&image);
    payload
}

/// Bakes a 2- or 3-assembly program into a `.lmli` image. See the module doc for the ABI.
///
/// # Safety
/// The three pointer/length pairs must each be a buffer the host filled via a prior `lamella_alloc`
/// (a zero-length `lib` is allowed and means a corlib+app bake).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_bake(
    corlib_ptr: *const u8,
    corlib_len: usize,
    lib_ptr: *const u8,
    lib_len: usize,
    app_ptr: *const u8,
    app_len: usize,
    trim: u32,
) -> *mut u8 {
    let read = |ptr: *const u8, len: usize| -> &[u8] {
        if len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(ptr, len) }
        }
    };
    result_buffer(bake(
        read(corlib_ptr, corlib_len),
        read(lib_ptr, lib_len),
        read(app_ptr, app_len),
        trim != 0,
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
    fn bakes_a_corlib_and_app_into_an_image() {
        let corlib = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-load/tests/fixtures/corlib.dll"
        );
        let app = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wireline/tests/fixtures/hello.exe"
        );
        let (Ok(corlib), Ok(app)) = (std::fs::read(corlib), std::fs::read(app)) else {
            return;
        };
        let payload = bake(&corlib, &[], &app, true);
        let (json, image_len) = parse(&payload);
        assert!(json["error"].is_null(), "unexpected error: {}", json["error"]);
        assert_eq!(json["violations"].as_array().unwrap().len(), 0, "no violations");
        assert!(image_len > 0, "expected a baked image");
    }
}
