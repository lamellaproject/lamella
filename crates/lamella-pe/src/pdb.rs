//! Encoding Portable PDB debug data (the Portable PDB spec, an extension of
//! ECMA-335 metadata).

use crate::heap::{
    BlobHeapBuilder, GuidHeapBuilder, StringHeapBuilder, compress_i32, compress_u32,
};
use crate::root::metadata_root_from_streams;
use crate::tables::{Column, HeapSizes, TableStream};
use alloc::vec;
use alloc::vec::Vec;
use lamella_token::Token;

/// `MethodDef` table (II.22.26) -- referenced by `LocalScope.Method`.
const METHOD_DEF: u8 = 0x06;
/// `Document` table (II Portable PDB).
const DOCUMENT: u8 = 0x30;
/// `MethodDebugInformation` table -- parallel to `MethodDef`.
const METHOD_DEBUG_INFORMATION: u8 = 0x31;
/// `LocalScope` table (a method's local-variable scope).
const LOCAL_SCOPE: u8 = 0x32;
/// `LocalVariable` table (a named local).
const LOCAL_VARIABLE: u8 = 0x33;
/// `LocalConstant` table (unused; `LocalScope.ConstantList` points into it).
const LOCAL_CONSTANT: u8 = 0x34;
/// `ImportScope` table (the `using` scope a `LocalScope` sits in).
const IMPORT_SCOPE: u8 = 0x35;
/// The metadata-root version string (matches the PE's).
const RUNTIME_VERSION: &str = "v4.0.30319";
/// The C# language GUID, in the .NET `Guid` byte layout (Data1/2/3 little-endian).
const CSHARP_LANGUAGE_GUID: [u8; 16] = [
    0xf8, 0x62, 0x51, 0x3f, 0xc6, 0x07, 0xd3, 0x11, 0x90, 0x53, 0x00, 0xc0, 0x4f, 0xa3, 0x02, 0xa1,
];
/// The SHA-256 hash-algorithm GUID (Portable PDB spec,
/// `8829d00f-11b8-4213-878b-770e8597ac16`), in the same .NET `Guid` byte layout. It
/// names the algorithm behind a `Document`'s source hash.
const SHA256_ALGORITHM_GUID: [u8; 16] = [
    0x0f, 0xd0, 0x29, 0x88, 0xb8, 0x11, 0x13, 0x42, 0x87, 0x8b, 0x77, 0x0e, 0x85, 0x97, 0xac, 0x16,
];

/// One sequence point: the CIL offset (in bytes) where a statement begins and the
/// 1-based source line/column range it covers. A *hidden* point (`is_hidden`) marks
/// compiler-synthesized IL that has no source -- a debugger steps over it rather than
/// stopping; its line/column fields are unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencePoint {
    /// Byte offset into the method body where the statement's IL begins.
    pub il_offset: u32,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based start column.
    pub start_column: u32,
    /// 1-based end line.
    pub end_line: u32,
    /// 1-based end column.
    pub end_column: u32,
    /// A hidden point (`0xFEEFEE`): the IL at this offset has no source position, so a
    /// debugger steps over it (a `using`/`lock` disposal, other synthesized code).
    pub is_hidden: bool,
}

impl SequencePoint {
    /// A hidden sequence point at `il_offset` -- synthesized IL a debugger steps over.
    #[must_use]
    pub fn hidden(il_offset: u32) -> SequencePoint {
        SequencePoint {
            il_offset,
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: 0,
            is_hidden: true,
        }
    }
}

/// Encodes the single-document sequence-points blob for one method (Portable PDB
/// spec, "Sequence points blob"). `local_signature` is the RID of the method's
/// local-variable `StandAloneSig` (0 when it has no locals). Points must be ordered
/// by non-decreasing IL offset; points sharing an offset with the previous one are
/// dropped (a statement that emitted no IL of its own).
///
/// Returns an empty vector when there are no points, signalling "no debug info" --
/// the caller stores that as a null blob.
#[must_use]
pub fn sequence_points_blob(local_signature: u32, points: &[SequencePoint]) -> Vec<u8> {
    if points.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    compress_u32(local_signature, &mut out);

    let mut previous_il: Option<u32> = None;
    let mut previous_pos: Option<(u32, u32)> = None;
    for point in points {
        if !point.is_hidden
            && point.start_line == point.end_line
            && point.start_column == point.end_column
        {
            continue;
        }
        match previous_il {
            Some(previous) => {
                let delta = point.il_offset - previous;
                if delta == 0 {
                    continue;
                }
                compress_u32(delta, &mut out);
            }
            None => compress_u32(point.il_offset, &mut out),
        }
        previous_il = Some(point.il_offset);

        if point.is_hidden {
            compress_u32(0, &mut out);
            compress_u32(0, &mut out);
            continue;
        }

        let delta_lines = point.end_line - point.start_line;
        compress_u32(delta_lines, &mut out);
        if delta_lines == 0 {
            compress_u32(point.end_column - point.start_column, &mut out);
        } else {
            compress_i32(point.end_column as i32 - point.start_column as i32, &mut out);
        }
        match previous_pos {
            None => {
                compress_u32(point.start_line, &mut out);
                compress_u32(point.start_column, &mut out);
            }
            Some((line, column)) => {
                compress_i32(point.start_line as i32 - line as i32, &mut out);
                compress_i32(point.start_column as i32 - column as i32, &mut out);
            }
        }
        previous_pos = Some((point.start_line, point.start_column));
    }
    out
}

/// A named local variable: its slot index in the method's locals and its name.
pub struct LocalVariable {
    /// The local's 0-based index in the method's local signature.
    pub index: u16,
    /// The source name to show in a debugger.
    pub name: alloc::boxed::Box<str>,
}

/// One method's debug info, supplied in `MethodDef` order (the
/// `MethodDebugInformation` table is parallel to `MethodDef`). A method with no
/// sequence points -- a compiler-synthesized one, say -- still occupies a row.
pub struct MethodDebug {
    /// The method's sequence points, ordered by IL offset (empty if none).
    pub sequence_points: Vec<SequencePoint>,
    /// The RID of the method's local-variable `StandAloneSig` (0 if it has none).
    pub local_signature: u32,
    /// The method's named locals (empty if none); emitted as a method-wide scope.
    pub locals: Vec<LocalVariable>,
    /// The method body's IL byte length, for the local scope's range.
    pub scope_length: u32,
    /// The 1-based `Document` row this method's sequence points attribute to (its
    /// source file), or 0 when it has no points. Every point in the method shares it:
    /// a method spanning documents (via `#line`) is not represented in v1.
    pub document: u32,
}

/// A source document for the PDB: its path (the key a debugger resolves breakpoints
/// against) and its text, whose SHA-256 is emitted as the document hash so a debugger
/// can verify the source on disk still matches the one compiled. An empty `source`
/// emits no hash.
///
/// The hash covers the text as the compiler decoded it (its UTF-8 bytes), which equals
/// the file on disk for the common UTF-8-without-BOM source; matching a re-encoded
/// source byte-for-byte (UTF-16, a BOM) is a later refinement over the raw file bytes.
pub struct DebugDocument<'a> {
    /// The document path, recorded verbatim.
    pub path: &'a str,
    /// The document's source text; empty to emit no hash.
    pub source: &'a str,
}

/// Assembles a standalone Portable PDB for a compilation's source documents: one
/// `Document` row per file in `documents` (each with a SHA-256 source hash), a
/// `MethodDebugInformation` row per method (parallel to `MethodDef`, each attributed
/// to its own document), and the `#Pdb` stream carrying the id and entry point. The id
/// must match the PE's debug-directory entry so a debugger pairs the two.
#[must_use]
pub fn build_portable_pdb(
    documents: &[DebugDocument],
    methods: &[MethodDebug],
    entry_point: Token,
    pdb_id: [u8; 20],
) -> Vec<u8> {
    let mut strings = StringHeapBuilder::new();
    let mut blobs = BlobHeapBuilder::new();
    let mut guids = GuidHeapBuilder::new();
    let mut tables = TableStream::new();

    let language = guids.add(CSHARP_LANGUAGE_GUID);
    let sha_algorithm = if documents.iter().any(|document| !document.source.is_empty()) {
        guids.add(SHA256_ALGORITHM_GUID)
    } else {
        0
    };
    for document in documents {
        let path_part = blobs.intern(document.path.as_bytes());
        let mut name_blob = vec![0u8];
        compress_u32(path_part, &mut name_blob);
        let name = blobs.intern(&name_blob);
        let (hash_algorithm, hash) = if document.source.is_empty() {
            (0, 0)
        } else {
            let digest = crate::sha256::sha256(document.source.as_bytes());
            (sha_algorithm, blobs.intern(&digest))
        };
        tables.add_row(
            DOCUMENT,
            vec![
                Column::BlobRef(name),
                Column::GuidRef(hash_algorithm),
                Column::BlobRef(hash),
                Column::GuidRef(language),
            ],
        );
    }

    for method in methods {
        let (document_index, sequence_points) = if method.sequence_points.is_empty() {
            (0, 0)
        } else {
            let blob = sequence_points_blob(method.local_signature, &method.sequence_points);
            (method.document, blobs.intern(&blob))
        };
        tables.add_row(
            METHOD_DEBUG_INFORMATION,
            vec![
                Column::Index(DOCUMENT, document_index),
                Column::BlobRef(sequence_points),
            ],
        );
    }

    let has_locals = methods.iter().any(|method| !method.locals.is_empty());
    if has_locals {
        tables.mark_sorted(LOCAL_SCOPE);
        tables.add_row(
            IMPORT_SCOPE,
            vec![Column::Index(IMPORT_SCOPE, 0), Column::BlobRef(0)],
        );
        for (index, method) in methods.iter().enumerate() {
            if method.locals.is_empty() {
                continue;
            }
            let first_variable = tables.row_count(LOCAL_VARIABLE) + 1;
            tables.add_row(
                LOCAL_SCOPE,
                vec![
                    Column::Index(METHOD_DEF, index as u32 + 1),
                    Column::Index(IMPORT_SCOPE, 1),
                    Column::Index(LOCAL_VARIABLE, first_variable),
                    Column::Index(LOCAL_CONSTANT, 1),
                    Column::U32(0),
                    Column::U32(method.scope_length),
                ],
            );
            for local in &method.locals {
                let name = strings.intern(&local.name);
                tables.add_row(
                    LOCAL_VARIABLE,
                    vec![
                        Column::U16(0),
                        Column::U16(local.index),
                        Column::StringRef(name),
                    ],
                );
            }
        }
    }

    let (referenced_tables, referenced_rows) = if has_locals {
        (1u64 << METHOD_DEF, vec![methods.len() as u32])
    } else {
        (0, Vec::new())
    };

    let table_bytes = tables.serialize(HeapSizes::default());
    let string_bytes = strings.into_bytes();
    let guid_bytes = guids.into_bytes();
    let blob_bytes = blobs.into_bytes();
    let pdb_stream = pdb_stream(pdb_id, entry_point, referenced_tables, &referenced_rows);

    let streams: Vec<(&str, &[u8])> = vec![
        ("#Pdb", pdb_stream.as_slice()),
        ("#~", &table_bytes),
        ("#Strings", &string_bytes),
        ("#GUID", &guid_bytes),
        ("#Blob", &blob_bytes),
    ];
    metadata_root_from_streams(RUNTIME_VERSION, &streams)
}

/// The `#Pdb` stream: the 20-byte id, the entry-point token, the bit vector of
/// referenced type-system tables, and a row count for each one (ascending bit
/// order), so the reader can size the debug tables' references into the PE.
fn pdb_stream(
    pdb_id: [u8; 20],
    entry_point: Token,
    referenced_tables: u64,
    referenced_rows: &[u32],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + referenced_rows.len() * 4);
    out.extend_from_slice(&pdb_id);
    out.extend_from_slice(&entry_point.0.to_le_bytes());
    out.extend_from_slice(&referenced_tables.to_le_bytes());
    for &rows in referenced_rows {
        out.extend_from_slice(&rows.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(il: u32, sl: u32, sc: u32, el: u32, ec: u32) -> SequencePoint {
        SequencePoint {
            il_offset: il,
            start_line: sl,
            start_column: sc,
            end_line: el,
            end_column: ec,
            is_hidden: false,
        }
    }

    #[test]
    fn no_points_is_an_empty_blob() {
        assert!(sequence_points_blob(0, &[]).is_empty());
    }

    #[test]
    fn a_hidden_point_is_the_marker_and_does_not_move_the_position_base() {
        let blob = sequence_points_blob(
            1,
            &[
                point(0, 5, 9, 5, 25),
                SequencePoint::hidden(4),
                point(8, 6, 9, 6, 20),
            ],
        );
        assert_eq!(
            blob,
            [
                0x01,
                0x00, 0x00, 0x10, 0x05, 0x09,
                0x04, 0x00, 0x00,
                0x04, 0x00, 0x0B, 0x02, 0x00,
            ]
        );
    }

    #[test]
    fn first_point_is_absolute() {
        let blob = sequence_points_blob(0, &[point(0, 3, 5, 3, 12)]);
        assert_eq!(blob, [0x00, 0x00, 0x00, 0x07, 0x03, 0x05]);
    }

    #[test]
    fn later_points_are_deltas_from_the_previous() {
        let blob = sequence_points_blob(1, &[point(0, 3, 5, 3, 6), point(4, 4, 5, 4, 6)]);
        assert_eq!(
            blob,
            [
                0x01, 0x00, 0x00, 0x01, 0x03, 0x05, 0x04, 0x00, 0x01, 0x02, 0x00
            ]
        );
    }

    #[test]
    fn a_point_sharing_an_offset_is_dropped() {
        let blob = sequence_points_blob(0, &[point(2, 1, 1, 1, 2), point(2, 5, 5, 5, 6)]);
        assert_eq!(blob, [0x00, 0x02, 0x00, 0x01, 0x01, 0x01]);
    }

    #[test]
    fn a_zero_width_point_is_dropped() {
        let blob = sequence_points_blob(0, &[point(0, 1, 1, 1, 1), point(4, 2, 5, 2, 9)]);
        assert_eq!(blob, [0x00, 0x04, 0x00, 0x04, 0x02, 0x05]);
    }

    fn u16_at(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }
    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    /// Finds a named stream's body in a metadata root by walking the directory.
    fn find_stream<'a>(root: &'a [u8], name: &str) -> &'a [u8] {
        let version_len = u32_at(root, 12) as usize;
        let mut p = 16 + version_len + 2;
        let count = u16_at(root, p);
        p += 2;
        for _ in 0..count {
            let offset = u32_at(root, p) as usize;
            let size = u32_at(root, p + 4) as usize;
            p += 8;
            let start = p;
            while root[p] != 0 {
                p += 1;
            }
            let entry = core::str::from_utf8(&root[start..p]).unwrap();
            p = start + (((p - start + 1) + 3) & !3);
            if entry == name {
                return &root[offset..offset + size];
            }
        }
        panic!("stream {name} not found");
    }

    #[test]
    fn several_documents_each_get_a_row_and_a_method() {
        let methods = [
            MethodDebug {
                sequence_points: vec![point(0, 1, 1, 1, 2)],
                local_signature: 0,
                locals: Vec::new(),
                scope_length: 2,
                document: 1,
            },
            MethodDebug {
                sequence_points: vec![point(0, 1, 1, 1, 2)],
                local_signature: 0,
                locals: Vec::new(),
                scope_length: 2,
                document: 2,
            },
        ];
        let documents = [
            DebugDocument { path: "A.cs", source: "class A{}" },
            DebugDocument { path: "B.cs", source: "class B{}" },
        ];
        let pdb = build_portable_pdb(&documents, &methods, Token::new(0x06, 1), [0u8; 20]);
        let blobs = find_stream(&pdb, "#Blob");
        assert!(blobs.windows(4).any(|w| w == b"A.cs"), "A.cs missing");
        assert!(blobs.windows(4).any(|w| w == b"B.cs"), "B.cs missing");
        let guids = find_stream(&pdb, "#GUID");
        assert!(
            guids.windows(16).any(|w| w == SHA256_ALGORITHM_GUID),
            "SHA-256 algorithm GUID missing"
        );
        let digest = crate::sha256::sha256(b"class A{}");
        assert!(blobs.windows(32).any(|w| w == digest), "source hash missing");
    }

    #[test]
    fn portable_pdb_has_a_pdb_stream_with_the_id_and_entry_point() {
        let id = [0xABu8; 20];
        let entry = Token::new(0x06, 1);
        let methods = [
            MethodDebug {
                sequence_points: vec![point(0, 3, 5, 3, 12)],
                local_signature: 0,
                locals: alloc::vec![LocalVariable {
                    index: 0,
                    name: "x".into(),
                }],
                scope_length: 8,
                document: 1,
            },
            MethodDebug {
                sequence_points: Vec::new(),
                local_signature: 0,
                locals: Vec::new(),
                scope_length: 0,
                document: 0,
            },
        ];
        let documents = [DebugDocument { path: "C:\\src\\App.cs", source: "" }];
        let pdb = build_portable_pdb(&documents, &methods, entry, id);

        let stream = find_stream(&pdb, "#Pdb");
        assert_eq!(&stream[..20], &id);
        assert_eq!(u32_at(stream, 20), entry.0);
        assert_eq!(
            u64::from_le_bytes(stream[24..32].try_into().unwrap()),
            1 << 6
        );
        assert!(
            find_stream(&pdb, "#Strings")
                .windows(b"x\0".len())
                .any(|window| window == b"x\0")
        );
        assert!(!find_stream(&pdb, "#~").is_empty());
        assert!(
            find_stream(&pdb, "#Blob")
                .windows(b"App.cs".len())
                .any(|w| w == b"App.cs")
        );
    }
}
