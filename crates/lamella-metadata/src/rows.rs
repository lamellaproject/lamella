//! Table row layout and navigation (ECMA-335 1st ed, II.22).

use crate::coded::CodedIndex;
use crate::tables::{TableError, TablesHeader, table};
use lamella_token::Token;

/// A single column of a metadata table row.
#[derive(Debug, Clone, Copy)]
pub enum Col {
    /// A 2-byte integer.
    U16,
    /// A 4-byte integer.
    U32,
    /// A `#Strings` heap index.
    Str,
    /// A `#GUID` heap index.
    Guid,
    /// A `#Blob` heap index.
    Blob,
    /// A simple index into the given table.
    Idx(u8),
    /// A coded index.
    Coded(CodedIndex),
}

/// The column layout of `table` (II.22), or `None` for a table this reader does
/// not model (its presence makes an image unreadable rather than miscomputed).
#[allow(clippy::too_many_lines)]
pub fn columns(table_number: u8) -> Option<&'static [Col]> {
    use CodedIndex as C;
    use Col::{Blob, Coded, Guid, Idx, Str, U16, U32};
    Some(match table_number {
        table::MODULE => &[U16, Str, Guid, Guid, Guid],
        table::TYPE_REF => &[Coded(C::ResolutionScope), Str, Str],
        table::TYPE_DEF => &[
            U32,
            Str,
            Str,
            Coded(C::TypeDefOrRef),
            Idx(table::FIELD),
            Idx(table::METHOD_DEF),
        ],
        table::FIELD => &[U16, Str, Blob],
        table::METHOD_DEF => &[U32, U16, U16, Str, Blob, Idx(table::PARAM)],
        table::PARAM => &[U16, U16, Str],
        table::INTERFACE_IMPL => &[Idx(table::TYPE_DEF), Coded(C::TypeDefOrRef)],
        table::MEMBER_REF => &[Coded(C::MemberRefParent), Str, Blob],
        table::CONSTANT => &[U16, Coded(C::HasConstant), Blob],
        table::CUSTOM_ATTRIBUTE => &[
            Coded(C::HasCustomAttribute),
            Coded(C::CustomAttributeType),
            Blob,
        ],
        table::FIELD_MARSHAL => &[Coded(C::HasFieldMarshal), Blob],
        table::DECL_SECURITY => &[U16, Coded(C::HasDeclSecurity), Blob],
        table::CLASS_LAYOUT => &[U16, U32, Idx(table::TYPE_DEF)],
        table::FIELD_LAYOUT => &[U32, Idx(table::FIELD)],
        table::STAND_ALONE_SIG => &[Blob],
        table::EVENT_MAP => &[Idx(table::TYPE_DEF), Idx(table::EVENT)],
        table::EVENT => &[U16, Str, Coded(C::TypeDefOrRef)],
        table::PROPERTY_MAP => &[Idx(table::TYPE_DEF), Idx(table::PROPERTY)],
        table::PROPERTY => &[U16, Str, Blob],
        table::METHOD_SEMANTICS => &[U16, Idx(table::METHOD_DEF), Coded(C::HasSemantics)],
        table::METHOD_IMPL => &[
            Idx(table::TYPE_DEF),
            Coded(C::MethodDefOrRef),
            Coded(C::MethodDefOrRef),
        ],
        table::MODULE_REF => &[Str],
        table::TYPE_SPEC => &[Blob],
        table::IMPL_MAP => &[U16, Coded(C::MemberForwarded), Str, Idx(table::MODULE_REF)],
        table::FIELD_RVA => &[U32, Idx(table::FIELD)],
        table::ASSEMBLY => &[U32, U16, U16, U16, U16, U32, Blob, Str, Str],
        table::ASSEMBLY_REF => &[U16, U16, U16, U16, U32, Blob, Str, Str, Blob],
        table::FILE => &[U32, Str, Blob],
        table::EXPORTED_TYPE => &[U32, U32, Str, Str, Coded(C::Implementation)],
        table::MANIFEST_RESOURCE => &[U32, U32, Str, Coded(C::Implementation)],
        table::NESTED_CLASS => &[Idx(table::TYPE_DEF), Idx(table::TYPE_DEF)],
        table::GENERIC_PARAM => &[U16, U16, Coded(C::TypeOrMethodDef), Str],
        table::METHOD_SPEC => &[Coded(C::MethodDefOrRef), Blob],
        table::GENERIC_PARAM_CONSTRAINT => &[Idx(table::GENERIC_PARAM), Coded(C::TypeDefOrRef)],
        table::DOCUMENT => &[Blob, Guid, Blob, Guid],
        table::METHOD_DEBUG_INFORMATION => &[Idx(table::DOCUMENT), Blob],
        table::LOCAL_SCOPE => &[
            Idx(table::METHOD_DEF),
            Idx(table::IMPORT_SCOPE),
            Idx(table::LOCAL_VARIABLE),
            Idx(table::LOCAL_CONSTANT),
            U32,
            U32,
        ],
        table::LOCAL_VARIABLE => &[U16, U16, Str],
        table::LOCAL_CONSTANT => &[Str, Blob],
        table::IMPORT_SCOPE => &[Idx(table::IMPORT_SCOPE), Blob],
        _ => return None,
    })
}

/// The byte width of one column given the header's heap and row sizes.
fn column_width(column: Col, header: &TablesHeader) -> usize {
    match column {
        Col::U16 => 2,
        Col::U32 => 4,
        Col::Str => header.string_index_size(),
        Col::Guid => header.guid_index_size(),
        Col::Blob => header.blob_index_size(),
        Col::Idx(target) => {
            if header.row_count(target) < (1 << 16) {
                2
            } else {
                4
            }
        }
        Col::Coded(kind) => kind.size(header),
    }
}

/// The byte size of one row of `table_number`.
fn row_size(table_number: u8, header: &TablesHeader) -> Option<usize> {
    Some(
        columns(table_number)?
            .iter()
            .map(|&column| column_width(column, header))
            .sum(),
    )
}

/// Navigable access to the metadata tables: each present table's row size and
/// the offset where its rows begin.
#[derive(Debug, Clone, Copy)]
pub struct Tables<'a> {
    header: TablesHeader<'a>,
    starts: [usize; 64],
    sizes: [usize; 64],
}

impl<'a> Tables<'a> {
    /// Computes the row size and start offset of every present table. Errors if a
    /// present table has no known layout.
    pub fn new(header: TablesHeader<'a>) -> Result<Tables<'a>, TableError> {
        let mut sizes = [0usize; 64];
        let mut starts = [0usize; 64];
        let mut offset = 0usize;
        for table_number in 0u8..64 {
            let count = header.row_count(table_number);
            if count == 0 {
                continue;
            }
            let size = row_size(table_number, &header).ok_or(TableError::Truncated)?;
            sizes[table_number as usize] = size;
            starts[table_number as usize] = offset;
            offset += size * count as usize;
        }
        Ok(Tables {
            header,
            starts,
            sizes,
        })
    }

    /// The header these tables were built from.
    #[must_use]
    pub fn header(&self) -> &TablesHeader<'a> {
        &self.header
    }

    /// The number of rows in `table_number`.
    #[must_use]
    pub fn row_count(&self, table_number: u8) -> u32 {
        self.header.row_count(table_number)
    }

    /// The 1-based `index`-th row of `table_number`, or `None` if out of range.
    #[must_use]
    pub fn row(&self, table_number: u8, index: u32) -> Option<Row<'a>> {
        if index == 0 || index > self.header.row_count(table_number) {
            return None;
        }
        let size = self.sizes[table_number as usize];
        let start = self.starts[table_number as usize] + (index as usize - 1) * size;
        let bytes = self.header.rows_data().get(start..start + size)?;
        Some(Row {
            bytes,
            table_number,
            header: self.header,
        })
    }
}

/// One table row, read by column position.
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    bytes: &'a [u8],
    table_number: u8,
    header: TablesHeader<'a>,
}

impl Row<'_> {
    /// The byte offset and width of column `index` within the row.
    fn locate(&self, index: usize) -> Option<(usize, Col)> {
        let layout = columns(self.table_number)?;
        let column = *layout.get(index)?;
        let offset = layout[..index]
            .iter()
            .map(|&c| column_width(c, &self.header))
            .sum();
        Some((offset, column))
    }

    /// The raw value of column `index` (2- or 4-byte little-endian), 0 if absent.
    #[must_use]
    pub fn raw(&self, index: usize) -> u32 {
        let Some((offset, column)) = self.locate(index) else {
            return 0;
        };
        let width = column_width(column, &self.header);
        let mut value = 0u32;
        for byte in 0..width {
            if let Some(&b) = self.bytes.get(offset + byte) {
                value |= u32::from(b) << (8 * byte);
            }
        }
        value
    }

    /// Column `index` decoded as a coded-index [`Token`]. The column must be a
    /// coded index; otherwise the nil token is returned.
    #[must_use]
    pub fn token(&self, index: usize) -> Token {
        match self.locate(index) {
            Some((_, Col::Coded(kind))) => kind.decode(self.raw(index)),
            _ => Token::new(0, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn synthetic() -> Vec<u8> {
        let mut stream = Vec::new();
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&[2, 0, 0, 0]);
        let valid = (1u64 << table::MODULE) | (1u64 << table::TYPE_DEF);
        stream.extend_from_slice(&valid.to_le_bytes());
        stream.extend_from_slice(&0u64.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        stream.extend_from_slice(&[0, 0]);
        stream.extend_from_slice(&7u16.to_le_bytes());
        stream.extend_from_slice(&[1, 0, 0, 0, 0, 0]);
        stream.extend_from_slice(&0x0010_0001u32.to_le_bytes());
        stream.extend_from_slice(&11u16.to_le_bytes());
        stream.extend_from_slice(&20u16.to_le_bytes());
        stream.extend_from_slice(&((3u16 << 2) | 1).to_le_bytes());
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream
    }

    #[test]
    fn locates_rows_and_reads_columns() {
        let stream = synthetic();
        let header = TablesHeader::parse(&stream).unwrap();
        let tables = Tables::new(header).unwrap();
        let module = tables.row(table::MODULE, 1).unwrap();
        assert_eq!(module.raw(1), 7);

        let type_def = tables.row(table::TYPE_DEF, 1).unwrap();
        assert_eq!(type_def.raw(0), 0x0010_0001);
        assert_eq!(type_def.raw(1), 11);
        assert_eq!(type_def.raw(2), 20);
        let extends = type_def.token(3);
        assert_eq!(extends.table(), table::TYPE_REF);
        assert_eq!(extends.row(), 3);
    }

    #[test]
    fn out_of_range_rows_are_none() {
        let stream = synthetic();
        let header = TablesHeader::parse(&stream).unwrap();
        let tables = Tables::new(header).unwrap();
        assert!(tables.row(table::TYPE_DEF, 0).is_none());
        assert!(tables.row(table::TYPE_DEF, 2).is_none());
        assert!(tables.row(table::FIELD, 1).is_none());
    }

    #[test]
    fn the_generics_tables_have_a_known_row_size() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&[2, 0, 0, 0]);
        let valid = (1u64 << table::GENERIC_PARAM)
            | (1u64 << table::METHOD_SPEC)
            | (1u64 << table::GENERIC_PARAM_CONSTRAINT);
        stream.extend_from_slice(&valid.to_le_bytes());
        stream.extend_from_slice(&0u64.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        let header = TablesHeader::parse(&stream).unwrap();
        assert!(row_size(table::GENERIC_PARAM, &header).is_some());
        assert!(row_size(table::METHOD_SPEC, &header).is_some());
        assert!(row_size(table::GENERIC_PARAM_CONSTRAINT, &header).is_some());
    }
}
