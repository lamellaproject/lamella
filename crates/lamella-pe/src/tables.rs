//! The metadata tables stream `#~` (ECMA-335 1st ed, II.24.2.6).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use lamella_metadata::CodedIndex;
use lamella_token::Token;

/// Which variable-width heaps use 4-byte offsets (the HeapSizes byte, II.24.2.6).
#[derive(Clone, Copy, Default, Debug)]
pub struct HeapSizes {
    /// `#Strings` offsets are 4 bytes.
    pub wide_strings: bool,
    /// `#GUID` indices are 4 bytes.
    pub wide_guid: bool,
    /// `#Blob` offsets are 4 bytes.
    pub wide_blob: bool,
}

impl HeapSizes {
    const STRINGS: u8 = 0x01;
    const GUID: u8 = 0x02;
    const BLOB: u8 = 0x04;

    /// The HeapSizes byte.
    #[must_use]
    pub fn flags(self) -> u8 {
        let mut flags = 0;
        if self.wide_strings {
            flags |= Self::STRINGS;
        }
        if self.wide_guid {
            flags |= Self::GUID;
        }
        if self.wide_blob {
            flags |= Self::BLOB;
        }
        flags
    }
}

/// One cell of a metadata row.
#[derive(Clone, Debug)]
pub enum Column {
    /// A fixed 2-byte value, such as a flags field.
    U16(u16),
    /// A fixed 4-byte value, such as a method-body RVA.
    U32(u32),
    /// A `#Strings` heap offset.
    StringRef(u32),
    /// A `#GUID` heap index.
    GuidRef(u32),
    /// A `#Blob` heap offset.
    BlobRef(u32),
    /// A 1-based row index into a single table.
    Index(u8, u32),
    /// A coded index that may point into one of several tables.
    Coded(CodedIndex, Token),
}

/// The metadata tables being built, each a list of rows in insertion order.
#[derive(Default, Debug)]
pub struct TableStream {
    rows: BTreeMap<u8, Vec<Vec<Column>>>,
}

impl TableStream {
    /// An empty set of tables.
    #[must_use]
    pub fn new() -> TableStream {
        TableStream::default()
    }

    /// Appends a row to `table`, returning its 1-based row index.
    pub fn add_row(&mut self, table: u8, columns: Vec<Column>) -> u32 {
        let rows = self.rows.entry(table).or_default();
        rows.push(columns);
        rows.len() as u32
    }

    /// The number of rows in `table`.
    #[must_use]
    pub fn row_count(&self, table: u8) -> u32 {
        self.rows.get(&table).map_or(0, |rows| rows.len() as u32)
    }

    /// Serializes the `#~` stream: the header then every present table's rows,
    /// with column widths chosen from `heaps` and the row counts.
    #[must_use]
    pub fn serialize(&self, heaps: HeapSizes) -> Vec<u8> {
        let mut valid = 0u64;
        for (&table, rows) in &self.rows {
            if !rows.is_empty() {
                valid |= 1u64 << table;
            }
        }

        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(2);
        out.push(0);
        out.push(heaps.flags());
        out.push(1);
        out.extend_from_slice(&valid.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());

        for table in 0u8..64 {
            if valid & (1u64 << table) != 0 {
                out.extend_from_slice(&self.row_count(table).to_le_bytes());
            }
        }
        for table in 0u8..64 {
            if valid & (1u64 << table) != 0 {
                for row in &self.rows[&table] {
                    for column in row {
                        self.write_column(column, heaps, &mut out);
                    }
                }
            }
        }
        out
    }

    fn write_column(&self, column: &Column, heaps: HeapSizes, out: &mut Vec<u8>) {
        match column {
            Column::U16(value) => out.extend_from_slice(&value.to_le_bytes()),
            Column::U32(value) => out.extend_from_slice(&value.to_le_bytes()),
            Column::StringRef(offset) => write_ref(*offset, heaps.wide_strings, out),
            Column::GuidRef(index) => write_ref(*index, heaps.wide_guid, out),
            Column::BlobRef(offset) => write_ref(*offset, heaps.wide_blob, out),
            Column::Index(table, row) => write_ref(*row, self.row_count(*table) >= 0x1_0000, out),
            Column::Coded(kind, token) => {
                let wide = kind.width(|table| self.row_count(table)) == 4;
                write_ref(kind.encode(*token), wide, out);
            }
        }
    }
}

fn write_ref(value: u32, wide: bool, out: &mut Vec<u8>) {
    if wide {
        out.extend_from_slice(&value.to_le_bytes());
    } else {
        out.extend_from_slice(&(value as u16).to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_metadata::tables::table;

    fn u64_at(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }
    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn an_empty_stream_is_just_the_header() {
        let stream = TableStream::new().serialize(HeapSizes::default());
        assert_eq!(stream.len(), 24);
        assert_eq!(stream[4], 2);
        assert_eq!(stream[7], 1);
        assert_eq!(u64_at(&stream, 8), 0);
    }

    #[test]
    fn a_module_row_sets_its_valid_bit_and_count() {
        let mut tables = TableStream::new();
        let row = tables.add_row(
            table::MODULE,
            alloc::vec![
                Column::U16(0),
                Column::StringRef(1),
                Column::GuidRef(1),
                Column::GuidRef(0),
                Column::GuidRef(0),
            ],
        );
        assert_eq!(row, 1);

        let stream = tables.serialize(HeapSizes::default());
        assert_eq!(u64_at(&stream, 8), 1);
        assert_eq!(u32_at(&stream, 24), 1);
        assert_eq!(stream.len(), 24 + 4 + 10);
    }

    #[test]
    fn a_coded_index_column_encodes_and_sizes() {
        let mut tables = TableStream::new();
        tables.add_row(
            table::TYPE_DEF,
            alloc::vec![Column::Coded(
                CodedIndex::TypeDefOrRef,
                Token::new(table::TYPE_REF, 1),
            )],
        );
        let stream = tables.serialize(HeapSizes::default());
        let row_start = stream.len() - 2;
        assert_eq!(
            u16::from_le_bytes([stream[row_start], stream[row_start + 1]]),
            5
        );
    }
}
