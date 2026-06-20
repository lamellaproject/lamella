//! In-page compilation (feature `compile`): a wasm ABI over `compile_source`, so the
//! browser IDE (Studio) compiles the user's own C# client-side -- no server, the
//! "all compilation in the browser" pillar.

#![allow(unsafe_code)]

use lamella_assemble::compile_source;
use lamella_metadata::Assembly;

use crate::abi::result_buffer;

/// The 1-based (line, column) of byte `offset` in `source`.
fn line_col(source: &str, offset: usize) -> (u32, u32) {
    let mut line = 1u32;
    let mut column = 1u32;
    for (index, ch) in source.char_indices() {
        if index >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// Splits the references buffer (`[u32 count]` then `count` x `[u32 len][bytes]`) into
/// the individual assembly byte slices; stops at the first malformed length.
fn split_refs(refs: &[u8]) -> Vec<&[u8]> {
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

/// Compiles `source` against `refs`, returning the payload described in the module doc.
fn compile(source: &[u8], refs: &[u8]) -> Vec<u8> {
    let source = core::str::from_utf8(source).unwrap_or("");
    let blobs = split_refs(refs);
    let assemblies: Vec<Assembly> = blobs
        .iter()
        .filter_map(|blob| Assembly::read(blob).ok())
        .collect();
    let result = compile_source(
        source,
        "Program.cs",
        "Program",
        "Program",
        &assemblies,
        true,
    );

    let diagnostics: Vec<serde_json::Value> = result
        .diagnostics
        .iter()
        .map(|diagnostic| {
            let (line, column) = line_col(source, diagnostic.span.start as usize);
            serde_json::json!({
                "code": diagnostic.code,
                "severity": if diagnostic.is_error() { "error" } else { "warning" },
                "line": line,
                "column": column,
                "message": diagnostic.message,
            })
        })
        .collect();
    let envelope = serde_json::json!({
        "diagnostics": diagnostics,
        "emitError": result.emit_error.map(|error| format!("{error:?}")),
    });
    let json = serde_json::to_vec(&envelope).unwrap_or_default();
    let image = result.image.unwrap_or_default();
    let pdb = result.pdb.unwrap_or_default();

    let mut payload = Vec::with_capacity(12 + json.len() + image.len() + pdb.len());
    payload.extend_from_slice(&(json.len() as u32).to_le_bytes());
    payload.extend_from_slice(&json);
    payload.extend_from_slice(&(image.len() as u32).to_le_bytes());
    payload.extend_from_slice(&image);
    payload.extend_from_slice(&(pdb.len() as u32).to_le_bytes());
    payload.extend_from_slice(&pdb);
    payload
}

/// Compiles the C# at `src_ptr..src_ptr + src_len` against the reference assemblies
/// packed at `refs_ptr..refs_ptr + refs_len`, returning a `[u32 len][payload]` buffer
/// (free with `lamella_dealloc(result, 4 + len)`).
///
/// # Safety
/// Both pointer/length pairs must be buffers the host filled via prior `lamella_alloc`
/// calls (a zero-length `refs` is allowed and means no references).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_compile(
    src_ptr: *const u8,
    src_len: usize,
    refs_ptr: *const u8,
    refs_len: usize,
) -> *mut u8 {
    let source = unsafe { core::slice::from_raw_parts(src_ptr, src_len) };
    let refs: &[u8] = if refs_len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(refs_ptr, refs_len) }
    };
    result_buffer(compile(source, refs))
}

/// Builds completion JSON (`{ items: [{ label, kind, detail }] }`) for the caret at byte
/// `offset` in `source`, against `refs`. Parses + builds the model (BCL refs + the unit),
/// then asks the binder's completion engine.
fn complete_json(source: &[u8], offset: usize, refs: &[u8]) -> Vec<u8> {
    let source = core::str::from_utf8(source).unwrap_or("");
    let unit = lamella_syntax::parser::parse_compilation_unit(source).unit;
    let mut model = lamella_binder::Model::new();
    for blob in split_refs(refs) {
        if let Ok(assembly) = Assembly::read(blob) {
            lamella_binder::load_assembly(&mut model, &assembly);
        }
    }
    lamella_binder::collect_into(&mut model, &unit);
    model.link_bases();
    let items: Vec<serde_json::Value> = lamella_binder::complete(source, &unit, &model, offset)
        .into_iter()
        .map(|item| {
            serde_json::json!({
                "label": &*item.label,
                "kind": kind_label(item.kind),
                "detail": &*item.detail,
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({ "items": items })).unwrap_or_default()
}

fn kind_label(kind: lamella_binder::CompletionKind) -> &'static str {
    use lamella_binder::CompletionKind;
    match kind {
        CompletionKind::Field => "field",
        CompletionKind::Property => "property",
        CompletionKind::Method => "method",
        CompletionKind::Type => "type",
        CompletionKind::Local => "local",
        CompletionKind::Parameter => "parameter",
        CompletionKind::Keyword => "keyword",
    }
}

/// Completions (IntelliSense) for the caret at byte `offset` in the C# at
/// `src_ptr..src_ptr + src_len`, against the references packed at `refs_ptr..+refs_len`.
/// Returns a `[u32 len][JSON]` buffer (free with `lamella_dealloc(result, 4 + len)`);
/// the JSON is `{ items: [{ label, kind, detail }] }`.
///
/// # Safety
/// Both pointer/length pairs must be buffers the host filled via prior `lamella_alloc`
/// (a zero-length `refs` is allowed and means no references).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_complete(
    src_ptr: *const u8,
    src_len: usize,
    offset: usize,
    refs_ptr: *const u8,
    refs_len: usize,
) -> *mut u8 {
    let source = unsafe { core::slice::from_raw_parts(src_ptr, src_len) };
    let refs: &[u8] = if refs_len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(refs_ptr, refs_len) }
    };
    result_buffer(complete_json(source, offset, refs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the payload's leading `[u32 json_len][JSON]` and the image length.
    fn parse(payload: &[u8]) -> (serde_json::Value, usize) {
        let json_len = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
        let json: serde_json::Value = serde_json::from_slice(&payload[4..4 + json_len]).unwrap();
        let image_len =
            u32::from_le_bytes(payload[4 + json_len..8 + json_len].try_into().unwrap()) as usize;
        (json, image_len)
    }

    #[test]
    fn compiles_a_no_reference_program_to_an_image() {
        let payload = compile(
            b"class Program { static int Main() { return 0; } }",
            &0u32.to_le_bytes(),
        );
        let (json, image_len) = parse(&payload);
        assert!(image_len > 0, "expected an emitted image");
        assert_eq!(json["diagnostics"].as_array().unwrap().len(), 0);
        assert!(json["emitError"].is_null());
    }

    #[test]
    fn completes_a_locals_members() {
        let source = "class Widget { public int Count; public int Area() { return 0; } } \
                      class P { static int M() { Widget w; return w.Z; } }";
        let offset = source.find("w.").unwrap() + 2;
        let json = complete_json(source.as_bytes(), offset, &0u32.to_le_bytes());
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let labels: Vec<&str> = value["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["label"].as_str().unwrap())
            .collect();
        assert!(labels.contains(&"Count"), "got {labels:?}");
        assert!(labels.contains(&"Area"), "got {labels:?}");
    }

    #[test]
    fn reports_a_diagnostic_with_location() {
        let payload = compile(
            b"class Program { static int Main() { int x; return x; } }",
            &0u32.to_le_bytes(),
        );
        let (json, image_len) = parse(&payload);
        assert_eq!(image_len, 0, "an error blocks emission");
        let diagnostics = json["diagnostics"].as_array().unwrap();
        assert!(diagnostics.iter().any(|d| {
            d["code"] == 165 && d["severity"] == "error" && d["line"] == 1 && d["column"].is_u64()
        }));
    }
}
