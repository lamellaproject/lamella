//! Reading a standalone Portable PDB: sequence points and local-variable names.

use crate::bytes::Reader;
use crate::heaps::{read_compressed_i32, read_compressed_u32};
use crate::image::{MetadataError, MetadataImage};
use crate::rows::Tables;
use crate::tables::{TablesHeader, table};
use alloc::string::String;
use alloc::vec::Vec;
use lamella_token::Token;

/// A sequence point: a CIL byte offset mapped to a 1-based source line/column
/// range. A hidden point marks compiler-generated IL and carries no position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencePoint {
    /// The method-body byte offset where this point begins.
    pub il_offset: u32,
    /// 1-based start line (0 when hidden).
    pub start_line: u32,
    /// 1-based start column (0 when hidden).
    pub start_column: u32,
    /// 1-based end line (0 when hidden).
    pub end_line: u32,
    /// 1-based end column (0 when hidden).
    pub end_column: u32,
    /// Whether this is a hidden point (no source position).
    pub is_hidden: bool,
}

/// A named local variable in scope within a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalVariable<'a> {
    /// The local's 0-based slot in the method's local signature.
    pub index: u16,
    /// The local's source name.
    pub name: &'a str,
}

/// A parsed standalone Portable PDB, navigable by `MethodDef` row.
pub struct PortablePdb<'a> {
    image: MetadataImage<'a>,
    tables: Tables<'a>,
    entry_point: u32,
    pdb_id: Option<&'a [u8]>,
}

impl<'a> PortablePdb<'a> {
    /// Parses a standalone Portable PDB from its bytes.
    ///
    /// # Errors
    /// Returns [`MetadataError`] if the metadata root or `#~` stream is malformed.
    pub fn read(bytes: &'a [u8]) -> Result<PortablePdb<'a>, MetadataError> {
        let image = MetadataImage::parse_metadata_root(bytes)?;
        let mut header = TablesHeader::parse(image.tables())?;
        let pdb_stream = PdbStream::parse(image.pdb());
        if let Some(stream) = &pdb_stream {
            header.apply_external_rows(stream.referenced_tables, &stream.type_system_rows);
        }
        let tables = Tables::new(header)?;
        let (entry_point, pdb_id) = match pdb_stream {
            Some(stream) => (stream.entry_point, Some(stream.id)),
            None => (0, None),
        };
        Ok(PortablePdb {
            image,
            tables,
            entry_point,
            pdb_id,
        })
    }

    /// The entry-point method (a `MethodDef`) recorded in the `#Pdb` stream, or `None`
    /// for a nil token or a PDB with no `#Pdb` stream. A debugger uses it to find a
    /// program's `Main` for "stop at entry".
    #[must_use]
    pub fn entry_point(&self) -> Option<Token> {
        (self.entry_point != 0).then(|| {
            Token::new((self.entry_point >> 24) as u8, self.entry_point & 0x00FF_FFFF)
        })
    }

    /// The 20-byte PDB id (a GUID plus a stamp) that matches this PDB to its PE's debug
    /// directory entry, or `None` for a PDB with no `#Pdb` stream. A debugger checks it
    /// so a stale PDB is not applied to a freshly built image.
    #[must_use]
    pub fn pdb_id(&self) -> Option<&'a [u8]> {
        self.pdb_id
    }

    /// The number of source documents.
    #[must_use]
    pub fn document_count(&self) -> u32 {
        self.tables.row_count(table::DOCUMENT)
    }

    /// The path of document `rid` (1-based), reconstructed from its name blob.
    #[must_use]
    pub fn document_name(&self, rid: u32) -> Option<String> {
        let row = self.tables.row(table::DOCUMENT, rid)?;
        let blob = self.image.blob().get(row.raw(0)).ok()?;
        Some(self.decode_document_name(blob))
    }

    /// The sequence points of the method at `method_rid` (its `MethodDef` row),
    /// ordered by IL offset. Empty when the method carries no debug info.
    #[must_use]
    pub fn sequence_points(&self, method_rid: u32) -> Vec<SequencePoint> {
        let Some(row) = self.tables.row(table::METHOD_DEBUG_INFORMATION, method_rid) else {
            return Vec::new();
        };
        let blob_index = row.raw(1);
        if blob_index == 0 {
            return Vec::new();
        }
        match self.image.blob().get(blob_index) {
            Ok(blob) => decode_sequence_points(blob),
            Err(_) => Vec::new(),
        }
    }

    /// The named local variables in scope for the method at `method_rid`, gathered
    /// across its local scopes.
    #[must_use]
    pub fn local_variables(&self, method_rid: u32) -> Vec<LocalVariable<'a>> {
        let mut variables = Vec::new();
        let scope_count = self.tables.row_count(table::LOCAL_SCOPE);
        let variable_count = self.tables.row_count(table::LOCAL_VARIABLE);
        for scope_rid in 1..=scope_count {
            let Some(scope) = self.tables.row(table::LOCAL_SCOPE, scope_rid) else {
                continue;
            };
            if scope.raw(0) != method_rid {
                continue;
            }
            let first = scope.raw(2);
            let end = self
                .tables
                .row(table::LOCAL_SCOPE, scope_rid + 1)
                .map_or(variable_count + 1, |next| next.raw(2));
            for variable_rid in first..end {
                if let Some(variable) = self.tables.row(table::LOCAL_VARIABLE, variable_rid) {
                    if let Ok(name) = self.image.strings().get(variable.raw(2)) {
                        variables.push(LocalVariable {
                            index: variable.raw(1) as u16,
                            name,
                        });
                    }
                }
            }
        }
        variables
    }

    /// The number of methods with a debug row (parallel to the `MethodDef` table).
    /// A consumer iterates `1..=method_count()` to scan every method.
    #[must_use]
    pub fn method_count(&self) -> u32 {
        self.tables.row_count(table::METHOD_DEBUG_INFORMATION)
    }

    /// The source document of the method at `method_rid` (its `MethodDef` row), if
    /// it has debug info.
    #[must_use]
    pub fn method_document(&self, method_rid: u32) -> Option<String> {
        let row = self
            .tables
            .row(table::METHOD_DEBUG_INFORMATION, method_rid)?;
        let document = row.raw(0);
        if document == 0 {
            return None;
        }
        self.document_name(document)
    }

    /// The sequence point whose range covers `il_offset` in the method at
    /// `method_rid` -- the source line a debugger shows for an instruction at that
    /// offset (the last non-hidden point at or before it). The IL -> source half of
    /// the map, for a `stackTrace` frame's source location.
    #[must_use]
    pub fn source_location(&self, method_rid: u32, il_offset: u32) -> Option<SequencePoint> {
        self.sequence_points(method_rid)
            .into_iter()
            .rfind(|point| !point.is_hidden && point.il_offset <= il_offset)
    }

    /// The `(method_rid, il_offset)` a source breakpoint at `line` in `document`
    /// binds to: the first non-hidden point on or after `line` in that document. The
    /// source -> IL half of the map, for resolving `setBreakpoints`.
    #[must_use]
    pub fn resolve_breakpoint(&self, document: &str, line: u32) -> Option<(u32, u32)> {
        let mut best: Option<(u32, u32, u32)> = None;
        for method_rid in 1..=self.method_count() {
            if self.method_document(method_rid).as_deref() != Some(document) {
                continue;
            }
            for point in self.sequence_points(method_rid) {
                if point.is_hidden || point.start_line < line {
                    continue;
                }
                if best.is_none_or(|(best_line, _, best_il)| {
                    (point.start_line, point.il_offset) < (best_line, best_il)
                }) {
                    best = Some((point.start_line, method_rid, point.il_offset));
                }
            }
        }
        best.map(|(_, method_rid, il_offset)| (method_rid, il_offset))
    }

    /// Reconstructs a document name from its blob: a separator byte followed by a
    /// run of `#Blob`-heap part indices, the parts joined by the separator (a `0`
    /// separator joins with nothing -- the common single-part case).
    fn decode_document_name(&self, blob: &[u8]) -> String {
        let Some((&separator, mut rest)) = blob.split_first() else {
            return String::new();
        };
        let mut name = String::new();
        let mut first = true;
        while let Ok((index, consumed)) = read_compressed_u32(rest) {
            if consumed > rest.len() {
                break;
            }
            rest = &rest[consumed..];
            if !first && separator != 0 {
                name.push(separator as char);
            }
            first = false;
            if index != 0 {
                if let Ok(part) = self.image.blob().get(index) {
                    if let Ok(text) = core::str::from_utf8(part) {
                        name.push_str(text);
                    }
                }
            }
        }
        name
    }
}

/// The parsed `#Pdb` stream of a standalone Portable PDB (Portable PDB spec, "Standalone
/// Debugging Metadata"): the PDB id, the entry-point token, and the row counts of the
/// type-system tables that the debug tables index into the associated module.
struct PdbStream<'a> {
    /// The 20-byte PDB id (a GUID plus a stamp).
    id: &'a [u8],
    /// The entry-point `MethodDef` token, or 0 for none.
    entry_point: u32,
    /// A bit vector of the referenced type-system tables.
    referenced_tables: u64,
    /// Each referenced table's row count, in ascending table order.
    type_system_rows: Vec<u32>,
}

impl<'a> PdbStream<'a> {
    /// Parses the `#Pdb` stream, or `None` when it is absent or truncated (then the
    /// reader falls back to sizing external indices narrow).
    fn parse(stream: &'a [u8]) -> Option<PdbStream<'a>> {
        if stream.is_empty() {
            return None;
        }
        let mut reader = Reader::new(stream);
        let id = reader.read_bytes(20).ok()?;
        let entry_point = reader.read_u32().ok()?;
        let referenced_tables = reader.read_u64().ok()?;
        let mut type_system_rows = Vec::new();
        for _ in 0..referenced_tables.count_ones() {
            type_system_rows.push(reader.read_u32().ok()?);
        }
        Some(PdbStream {
            id,
            entry_point,
            referenced_tables,
            type_system_rows,
        })
    }
}

/// Decodes a single-document sequence-points blob (the Portable PDB delta stream):
/// a local-signature header, then a first absolute point and signed deltas after.
fn decode_sequence_points(blob: &[u8]) -> Vec<SequencePoint> {
    let mut points = Vec::new();
    let Ok((_local_signature, consumed)) = read_compressed_u32(blob) else {
        return points;
    };
    let mut rest = &blob[consumed..];

    let mut il_offset = 0u32;
    let mut line = 0u32;
    let mut column = 0u32;
    let mut first = true;
    while !rest.is_empty() {
        let Ok((delta_il, n)) = read_compressed_u32(rest) else {
            break;
        };
        rest = &rest[n..];
        il_offset = if first {
            delta_il
        } else {
            il_offset + delta_il
        };

        let Ok((delta_lines, n)) = read_compressed_u32(rest) else {
            break;
        };
        rest = &rest[n..];
        let delta_columns = if delta_lines == 0 {
            let Ok((value, n)) = read_compressed_u32(rest) else {
                break;
            };
            rest = &rest[n..];
            value as i32
        } else {
            let Ok((value, n)) = read_compressed_i32(rest) else {
                break;
            };
            rest = &rest[n..];
            value
        };

        if delta_lines == 0 && delta_columns == 0 {
            points.push(SequencePoint {
                il_offset,
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
                is_hidden: true,
            });
            first = false;
            continue;
        }

        let (start_line, start_column) = if first {
            let Ok((sl, n)) = read_compressed_u32(rest) else {
                break;
            };
            rest = &rest[n..];
            let Ok((sc, n)) = read_compressed_u32(rest) else {
                break;
            };
            rest = &rest[n..];
            (sl, sc)
        } else {
            let Ok((dsl, n)) = read_compressed_i32(rest) else {
                break;
            };
            rest = &rest[n..];
            let Ok((dsc, n)) = read_compressed_i32(rest) else {
                break;
            };
            rest = &rest[n..];
            ((line as i32 + dsl) as u32, (column as i32 + dsc) as u32)
        };
        line = start_line;
        column = start_column;
        first = false;
        points.push(SequencePoint {
            il_offset,
            start_line,
            start_column,
            end_line: start_line + delta_lines,
            end_column: (start_column as i32 + delta_columns) as u32,
            is_hidden: false,
        });
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_one_absolute_sequence_point() {
        let blob = [0x00, 0x00, 0x00, 0x07, 0x03, 0x05];
        let points = decode_sequence_points(&blob);
        assert_eq!(
            points,
            [SequencePoint {
                il_offset: 0,
                start_line: 3,
                start_column: 5,
                end_line: 3,
                end_column: 12,
                is_hidden: false,
            }]
        );
    }

    #[test]
    fn decodes_a_delta_chained_second_point() {
        let blob = [
            0x01, 0x00, 0x00, 0x01, 0x03, 0x05, 0x04, 0x00, 0x01, 0x02, 0x00,
        ];
        let points = decode_sequence_points(&blob);
        assert_eq!(points.len(), 2);
        assert_eq!(points[1].il_offset, 4);
        assert_eq!(points[1].start_line, 4);
        assert_eq!(points[1].start_column, 5);
        assert_eq!(points[1].end_column, 6);
    }
}
