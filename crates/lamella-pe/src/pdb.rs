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

/// One sequence point: the CIL offset (in bytes) where a statement begins and the
/// 1-based source line/column range it covers.
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

    let mut previous: Option<SequencePoint> = None;
    for point in points {
        if let Some(previous) = previous {
            let delta = point.il_offset - previous.il_offset;
            if delta == 0 {
                continue;
            }
            compress_u32(delta, &mut out);
        } else {
            compress_u32(point.il_offset, &mut out);
        }

        let delta_lines = point.end_line - point.start_line;
        compress_u32(delta_lines, &mut out);
        if delta_lines == 0 {
            compress_u32(point.end_column - point.start_column, &mut out);
        } else {
            compress_i32(
                point.end_column as i32 - point.start_column as i32,
                &mut out,
            );
        }

        match previous {
            None => {
                compress_u32(point.start_line, &mut out);
                compress_u32(point.start_column, &mut out);
            }
            Some(previous) => {
                compress_i32(
                    point.start_line as i32 - previous.start_line as i32,
                    &mut out,
                );
                compress_i32(
                    point.start_column as i32 - previous.start_column as i32,
                    &mut out,
                );
            }
        }
        previous = Some(*point);
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
}

/// Assembles a standalone Portable PDB for one source document: the `Document`
/// table, a `MethodDebugInformation` row per method (parallel to `MethodDef`), and
/// the `#Pdb` stream carrying the id and entry point. The id must match the PE's
/// debug-directory entry so a debugger pairs the two.
#[must_use]
pub fn build_portable_pdb(
    document_path: &str,
    methods: &[MethodDebug],
    entry_point: Token,
    pdb_id: [u8; 20],
) -> Vec<u8> {
    let mut strings = StringHeapBuilder::new();
    let mut blobs = BlobHeapBuilder::new();
    let mut guids = GuidHeapBuilder::new();
    let mut tables = TableStream::new();

    let path_part = blobs.intern(document_path.as_bytes());
    let mut name_blob = vec![0u8];
    compress_u32(path_part, &mut name_blob);
    let name = blobs.intern(&name_blob);
    let language = guids.add(CSHARP_LANGUAGE_GUID);
    let document = tables.add_row(
        DOCUMENT,
        vec![
            Column::BlobRef(name),
            Column::GuidRef(0),
            Column::BlobRef(0),
            Column::GuidRef(language),
        ],
    );

    for method in methods {
        let (document_index, sequence_points) = if method.sequence_points.is_empty() {
            (0, 0)
        } else {
            let blob = sequence_points_blob(method.local_signature, &method.sequence_points);
            (document, blobs.intern(&blob))
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
        }
    }

    #[test]
    fn no_points_is_an_empty_blob() {
        assert!(sequence_points_blob(0, &[]).is_empty());
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
            },
            MethodDebug {
                sequence_points: Vec::new(),
                local_signature: 0,
                locals: Vec::new(),
                scope_length: 0,
            },
        ];
        let pdb = build_portable_pdb("C:\\src\\App.cs", &methods, entry, id);

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
