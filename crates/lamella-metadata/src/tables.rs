//! The metadata tables stream `#~` (ECMA-335 1st ed, II.24.2.6).

use crate::bytes::{ReadError, Reader};

/// The metadata table numbers used in ECMA-335 1st edition (II.22). Generics
/// tables (2nd edition) are absent.
pub mod table {
    /// `Module` (II.22.30).
    pub const MODULE: u8 = 0x00;
    /// `TypeRef` (II.22.38).
    pub const TYPE_REF: u8 = 0x01;
    /// `TypeDef` (II.22.37).
    pub const TYPE_DEF: u8 = 0x02;
    /// `Field` (II.22.15).
    pub const FIELD: u8 = 0x04;
    /// `MethodDef` (II.22.26).
    pub const METHOD_DEF: u8 = 0x06;
    /// `Param` (II.22.33).
    pub const PARAM: u8 = 0x08;
    /// `InterfaceImpl` (II.22.23).
    pub const INTERFACE_IMPL: u8 = 0x09;
    /// `MemberRef` (II.22.25).
    pub const MEMBER_REF: u8 = 0x0A;
    /// `Constant` (II.22.9).
    pub const CONSTANT: u8 = 0x0B;
    /// `CustomAttribute` (II.22.10).
    pub const CUSTOM_ATTRIBUTE: u8 = 0x0C;
    /// `FieldMarshal` (II.22.17).
    pub const FIELD_MARSHAL: u8 = 0x0D;
    /// `DeclSecurity` (II.22.11).
    pub const DECL_SECURITY: u8 = 0x0E;
    /// `ClassLayout` (II.22.8).
    pub const CLASS_LAYOUT: u8 = 0x0F;
    /// `FieldLayout` (II.22.16).
    pub const FIELD_LAYOUT: u8 = 0x10;
    /// `StandAloneSig` (II.22.36).
    pub const STAND_ALONE_SIG: u8 = 0x11;
    /// `EventMap` (II.22.12).
    pub const EVENT_MAP: u8 = 0x12;
    /// `Event` (II.22.13).
    pub const EVENT: u8 = 0x14;
    /// `PropertyMap` (II.22.35).
    pub const PROPERTY_MAP: u8 = 0x15;
    /// `Property` (II.22.34).
    pub const PROPERTY: u8 = 0x17;
    /// `MethodSemantics` (II.22.28).
    pub const METHOD_SEMANTICS: u8 = 0x18;
    /// `MethodImpl` (II.22.27).
    pub const METHOD_IMPL: u8 = 0x19;
    /// `ModuleRef` (II.22.31).
    pub const MODULE_REF: u8 = 0x1A;
    /// `TypeSpec` (II.22.39).
    pub const TYPE_SPEC: u8 = 0x1B;
    /// `ImplMap` (II.22.22).
    pub const IMPL_MAP: u8 = 0x1C;
    /// `FieldRVA` (II.22.18).
    pub const FIELD_RVA: u8 = 0x1D;
    /// `Assembly` (II.22.2).
    pub const ASSEMBLY: u8 = 0x20;
    /// `AssemblyRef` (II.22.5).
    pub const ASSEMBLY_REF: u8 = 0x23;
    /// `File` (II.22.19).
    pub const FILE: u8 = 0x26;
    /// `ExportedType` (II.22.14).
    pub const EXPORTED_TYPE: u8 = 0x27;
    /// `ManifestResource` (II.22.24).
    pub const MANIFEST_RESOURCE: u8 = 0x28;
    /// `NestedClass` (II.22.32).
    pub const NESTED_CLASS: u8 = 0x29;
    /// `GenericParam` (II.22.20).
    pub const GENERIC_PARAM: u8 = 0x2A;
    /// `MethodSpec` (II.22.29).
    pub const METHOD_SPEC: u8 = 0x2B;
    /// `GenericParamConstraint` (II.22.21).
    pub const GENERIC_PARAM_CONSTRAINT: u8 = 0x2C;


    /// `Document` (a source document).
    pub const DOCUMENT: u8 = 0x30;
    /// `MethodDebugInformation` (parallel to `MethodDef`: its sequence points).
    pub const METHOD_DEBUG_INFORMATION: u8 = 0x31;
    /// `LocalScope` (a method's local-variable scope).
    pub const LOCAL_SCOPE: u8 = 0x32;
    /// `LocalVariable` (a named local).
    pub const LOCAL_VARIABLE: u8 = 0x33;
    /// `LocalConstant` (a named constant local).
    pub const LOCAL_CONSTANT: u8 = 0x34;
    /// `ImportScope` (the `using` scope a `LocalScope` sits in).
    pub const IMPORT_SCOPE: u8 = 0x35;
    /// `StateMachineMethod` (maps a kickoff method to its moved-next method).
    pub const STATE_MACHINE_METHOD: u8 = 0x36;
    /// `CustomDebugInformation` (arbitrary per-entity debug blobs; csc emits these).
    pub const CUSTOM_DEBUG_INFORMATION: u8 = 0x37;
}

/// An error parsing the tables-stream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableError {
    /// A read ran past the end of the stream.
    Truncated,
}

impl From<ReadError> for TableError {
    fn from(_: ReadError) -> TableError {
        TableError::Truncated
    }
}

/// The parsed tables-stream header: per-table row counts, heap index widths, and
/// the offset where the row data begins.
#[derive(Debug, Clone, Copy)]
pub struct TablesHeader<'a> {
    rows: [u32; 64],
    /// Row counts of type-system tables that live in another module, seeded from a
    /// standalone Portable PDB's `#Pdb` stream. These tables are NOT present in this
    /// stream's row data, so they affect column WIDTHS (an `Idx`/coded index into them
    /// is 2 vs 4 bytes) but never the row layout -- see [`TablesHeader::sizing_row_count`].
    external_rows: [u32; 64],
    string_index_size: u8,
    guid_index_size: u8,
    blob_index_size: u8,
    rows_data: &'a [u8],
}

impl<'a> TablesHeader<'a> {
    /// Parses the `#~` header at the start of the tables stream.
    pub fn parse(stream: &'a [u8]) -> Result<TablesHeader<'a>, TableError> {
        let mut reader = Reader::new(stream);
        reader.skip(4)?;
        reader.skip(2)?;
        let heap_sizes = reader.read_u8()?;
        reader.skip(1)?;
        let valid = reader.read_u64()?;
        reader.skip(8)?;
        let mut rows = [0u32; 64];
        for (table, slot) in rows.iter_mut().enumerate() {
            if valid & (1u64 << table) != 0 {
                *slot = reader.read_u32()?;
            }
        }
        let rows_data = stream
            .get(reader.position()..)
            .ok_or(TableError::Truncated)?;
        Ok(TablesHeader {
            rows,
            external_rows: [0u32; 64],
            string_index_size: if heap_sizes & 0x01 != 0 { 4 } else { 2 },
            guid_index_size: if heap_sizes & 0x02 != 0 { 4 } else { 2 },
            blob_index_size: if heap_sizes & 0x04 != 0 { 4 } else { 2 },
            rows_data,
        })
    }

    /// Seeds the row counts of type-system tables that live in another module, from a
    /// Portable PDB's `#Pdb` stream: `referenced` is the bit vector of referenced tables
    /// and `counts` their row counts in ascending table order. Used for column sizing
    /// only -- it does not make those tables present in this stream's row data.
    pub fn apply_external_rows(&mut self, referenced: u64, counts: &[u32]) {
        let mut counts = counts.iter();
        for table in 0..64 {
            if referenced & (1u64 << table) != 0 {
                if let Some(&count) = counts.next() {
                    self.external_rows[table] = count;
                }
            }
        }
    }

    /// The number of rows of `table` physically present in this stream's row data
    /// (a `table::*` number), 0 if absent. Drives row layout and row access.
    #[must_use]
    pub fn row_count(&self, table: u8) -> u32 {
        self.rows.get(table as usize).copied().unwrap_or(0)
    }

    /// The row count used to size a column that indexes `table`: the larger of the
    /// rows present here and any external count seeded from a `#Pdb` stream. A
    /// standalone PDB indexes type-system tables that live in another module, so an
    /// `Idx`/coded width must account for their size even though they are absent here.
    #[must_use]
    pub fn sizing_row_count(&self, table: u8) -> u32 {
        let present = self.rows.get(table as usize).copied().unwrap_or(0);
        let external = self.external_rows.get(table as usize).copied().unwrap_or(0);
        present.max(external)
    }

    /// The `#Strings` index width in bytes (2 or 4).
    #[must_use]
    pub fn string_index_size(&self) -> usize {
        self.string_index_size as usize
    }

    /// The `#GUID` index width in bytes (2 or 4).
    #[must_use]
    pub fn guid_index_size(&self) -> usize {
        self.guid_index_size as usize
    }

    /// The `#Blob` index width in bytes (2 or 4).
    #[must_use]
    pub fn blob_index_size(&self) -> usize {
        self.blob_index_size as usize
    }

    /// The row data following the header.
    #[must_use]
    pub fn rows_data(&self) -> &'a [u8] {
        self.rows_data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn synthetic_header() -> Vec<u8> {
        let mut stream = Vec::new();
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.push(2);
        stream.push(0);
        stream.push(0x00);
        stream.push(0);
        let valid = (1u64 << table::MODULE) | (1u64 << table::TYPE_DEF);
        stream.extend_from_slice(&valid.to_le_bytes());
        stream.extend_from_slice(&0u64.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        stream.extend_from_slice(&3u32.to_le_bytes());
        stream.extend_from_slice(&[0xDE, 0xAD]);
        stream
    }

    #[test]
    fn parses_row_counts_and_heap_widths() {
        let stream = synthetic_header();
        let header = TablesHeader::parse(&stream).unwrap();
        assert_eq!(header.row_count(table::MODULE), 1);
        assert_eq!(header.row_count(table::TYPE_DEF), 3);
        assert_eq!(header.row_count(table::FIELD), 0);
        assert_eq!(header.string_index_size(), 2);
        assert_eq!(header.blob_index_size(), 2);
        assert_eq!(header.rows_data(), &[0xDE, 0xAD]);
    }

    #[test]
    fn wide_heaps_are_four_bytes() {
        let mut stream = synthetic_header();
        stream[6] = 0x07;
        let header = TablesHeader::parse(&stream).unwrap();
        assert_eq!(header.string_index_size(), 4);
        assert_eq!(header.guid_index_size(), 4);
        assert_eq!(header.blob_index_size(), 4);
    }

    #[test]
    fn truncated_header_errors() {
        assert_eq!(
            TablesHeader::parse(&[0u8; 8]).unwrap_err(),
            TableError::Truncated
        );
    }
}
