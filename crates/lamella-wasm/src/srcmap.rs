//! In-page SOURCE MAP (feature `bake`): given the assemblies + the app's Portable PDB, return the
//! `method_id -> source line` table the LAMELLA LINK DEBUGGER needs, keyed by the SAME `method_id` the deployed
//! baked image reports on the wire. It loads + trims EXACTLY as `bake` does (so the numbering matches the
//! image the device runs) and reads the PDB via `lamella_metadata::PortablePdb` through the loader's own
//! token binding (`Module::resolve`) -- the mapping `lamella-dap`'s `InterpreterBackend::with_pdb` builds.
//! So the JS debugger REUSES the Rust PDB reader + loader binding (no JS PDB parser, no drift, and the
//! `MethodId <-> method_rid` question is answered by the code, not guessed).

#![allow(unsafe_code)]

use crate::abi::result_buffer;
use lamella_load::{load_with_corlib_and_libraries_unfrozen, load_with_corlib_unfrozen};
use lamella_metadata::{Assembly, PortablePdb};
use lamella_token::Token;

/// The `MethodDef` metadata table tag (matches lamella-dap's InterpreterBackend).
const METHOD_DEF: u8 = 0x06;

/// Wrap a JSON body as `[u32 json_len][JSON]` (the payload `result_buffer` then length-prefixes again).
fn wrap(json: Vec<u8>) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + json.len());
    payload.extend_from_slice(&(json.len() as u32).to_le_bytes());
    payload.extend_from_slice(&json);
    payload
}

fn error_payload(message: &str) -> Vec<u8> {
    wrap(
        serde_json::to_vec(&serde_json::json!({
            "methods": {}, "entryPoint": serde_json::Value::Null, "error": message,
        }))
        .unwrap_or_default(),
    )
}

fn srcmap(corlib: &[u8], library: &[u8], app: &[u8], pdb: &[u8], trim: bool) -> Vec<u8> {
    if library.is_empty() {
        srcmap_libs(corlib, &[], app, pdb, trim)
    } else {
        srcmap_libs(corlib, &[library], app, pdb, trim)
    }
}

/// The same source map over ANY number of library assemblies -- the driver-stack shape. Must accept
/// the SAME reference set the bake did, or the method_id numbering it produces will not match the
/// image on the device and every stop would highlight the wrong line.
fn srcmap_libs(corlib: &[u8], libraries: &[&[u8]], app: &[u8], pdb: &[u8], trim: bool) -> Vec<u8> {
    crate::abi::with_static(corlib, |corlib| {
        crate::abi::with_static_all(libraries, |libraries| {
            crate::abi::with_static(app, |app| srcmap_inner(corlib, libraries, app, pdb, trim))
        })
    })
}

fn srcmap_inner(
    corlib: &'static [u8],
    libraries: &[&'static [u8]],
    app: &'static [u8],
    pdb_bytes: &[u8],
    trim: bool,
) -> Vec<u8> {
    let corlib_asm = match Assembly::read(corlib) {
        Ok(assembly) => assembly,
        Err(error) => return error_payload(&format!("corlib parse: {error:?}")),
    };
    let app_asm = match Assembly::read(app) {
        Ok(assembly) => assembly,
        Err(error) => return error_payload(&format!("app parse: {error:?}")),
    };
    let mut library_asms = Vec::with_capacity(libraries.len());
    for (index, library) in libraries.iter().enumerate() {
        match Assembly::read(library) {
            Ok(assembly) => library_asms.push(assembly),
            Err(error) => return error_payload(&format!("library {index} parse: {error:?}")),
        }
    }

    let app_asm_index = u8::try_from(1 + library_asms.len()).unwrap_or(u8::MAX);
    let loaded = if library_asms.is_empty() {
        load_with_corlib_unfrozen(&corlib_asm, &app_asm)
    } else {
        load_with_corlib_and_libraries_unfrozen(&corlib_asm, &library_asms, &app_asm)
    };
    let mut loaded = match loaded {
        Ok(loaded) => loaded,
        Err(error) => return error_payload(&format!("load: {error:?}")),
    };
    if trim {
        let (methods, types, strings) = loaded.module.reachable_set(Some(loaded.entry));
        loaded.module.retain_reachable(&methods, &types, &strings);
    }

    let pdb = match PortablePdb::read(pdb_bytes) {
        Ok(pdb) => pdb,
        Err(error) => return error_payload(&format!("pdb parse: {error:?}")),
    };

    let mut method_names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    for type_def in app_asm.type_defs() {
        let type_name = type_def.name();
        for method in type_def.methods() {
            let leaf = method.name().unwrap_or_default();
            let qualified = match &type_name {
                Some(name) if !name.namespace.is_empty() => format!("{}.{}.{}", name.namespace, name.name, leaf),
                Some(name) => format!("{}.{}", name.name, leaf),
                None => leaf.to_string(),
            };
            method_names.insert(method.rid(), qualified);
        }
    }

    let mut methods = serde_json::Map::new();
    for rid in 1..=pdb.method_count() {
        let Some(method_id) = loaded.module.resolve(app_asm_index, Token::new(METHOD_DEF, rid)) else {
            continue;
        };
        let points: Vec<serde_json::Value> = pdb
            .sequence_points(rid)
            .into_iter()
            .filter(|point| !point.is_hidden)
            .map(|point| serde_json::json!({ "o": point.il_offset, "l": point.start_line, "c": point.start_column }))
            .collect();
        if points.is_empty() {
            continue;
        }
        let document = pdb.method_document(rid).unwrap_or_default();
        let name = method_names.get(&rid).cloned().unwrap_or_default();
        let locals: Vec<serde_json::Value> = pdb
            .local_variables(rid)
            .into_iter()
            .map(|lv| serde_json::json!({ "index": lv.index, "name": lv.name }))
            .collect();
        methods.insert(method_id.to_string(), serde_json::json!({ "document": document, "name": name, "points": points, "locals": locals }));
    }
    let entry_point = pdb
        .entry_point()
        .and_then(|token| loaded.module.resolve(app_asm_index, token))
        .map_or(serde_json::Value::Null, serde_json::Value::from);

    wrap(
        serde_json::to_vec(&serde_json::json!({
            "methods": methods, "entryPoint": entry_point, "error": serde_json::Value::Null,
        }))
        .unwrap_or_default(),
    )
}

/// See the module doc for the ABI.
///
/// # Safety
/// Each pointer/length pair must be a buffer the host filled via a prior `lamella_alloc` (a zero-length
/// `lib` is allowed and means a corlib+app, 2-assembly, map).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_srcmap(
    corlib_ptr: *const u8,
    corlib_len: usize,
    lib_ptr: *const u8,
    lib_len: usize,
    app_ptr: *const u8,
    app_len: usize,
    pdb_ptr: *const u8,
    pdb_len: usize,
    trim: u32,
) -> *mut u8 {
    let read = |ptr: *const u8, len: usize| -> &[u8] {
        if len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(ptr, len) }
        }
    };
    result_buffer(srcmap(
        read(corlib_ptr, corlib_len),
        read(lib_ptr, lib_len),
        read(app_ptr, app_len),
        read(pdb_ptr, pdb_len),
        trim != 0,
    ))
}

/// The source map over ANY number of library assemblies -- the companion to `lamella_bake_libs`, and
/// it must be given the SAME reference set that bake was, or the `method_id`s will not line up with
/// the image on the device. `libs` is packed as `[u32 count]` then `count` x `[u32 len][.dll bytes]`.
///
/// # Safety
/// Each pointer/length pair must be a buffer the host filled via a prior `lamella_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_srcmap_libs(
    corlib_ptr: *const u8,
    corlib_len: usize,
    libs_ptr: *const u8,
    libs_len: usize,
    app_ptr: *const u8,
    app_len: usize,
    pdb_ptr: *const u8,
    pdb_len: usize,
    trim: u32,
) -> *mut u8 {
    let read = |ptr: *const u8, len: usize| -> &[u8] {
        if len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(ptr, len) }
        }
    };
    let libs = crate::abi::split_refs(read(libs_ptr, libs_len));
    result_buffer(srcmap_libs(
        read(corlib_ptr, corlib_len),
        &libs,
        read(app_ptr, app_len),
        read(pdb_ptr, pdb_len),
        trim != 0,
    ))
}
