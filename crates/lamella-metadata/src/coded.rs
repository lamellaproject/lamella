//! Coded indices (ECMA-335 1st ed, II.24.2.6).

use crate::tables::{TablesHeader, table};
use lamella_token::Token;

/// A tag value that does not name a real table (an unused coded-index tag).
const NONE: u8 = 0xFF;

/// One of the coded-index kinds (II.24.2.6). Each names the tables it can point
/// into (indexed by tag) and therefore how many tag bits it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodedIndex {
    /// `TypeDefOrRef`: TypeDef, TypeRef, TypeSpec.
    TypeDefOrRef,
    /// `HasConstant`: Field, Param, Property.
    HasConstant,
    /// `HasCustomAttribute`: many tables.
    HasCustomAttribute,
    /// `HasFieldMarshal`: Field, Param.
    HasFieldMarshal,
    /// `HasDeclSecurity`: TypeDef, MethodDef, Assembly.
    HasDeclSecurity,
    /// `MemberRefParent`: TypeDef, TypeRef, ModuleRef, MethodDef, TypeSpec.
    MemberRefParent,
    /// `HasSemantics`: Event, Property.
    HasSemantics,
    /// `MethodDefOrRef`: MethodDef, MemberRef.
    MethodDefOrRef,
    /// `MemberForwarded`: Field, MethodDef.
    MemberForwarded,
    /// `Implementation`: File, AssemblyRef, ExportedType.
    Implementation,
    /// `CustomAttributeType`: MethodDef, MemberRef (tags 2 and 3).
    CustomAttributeType,
    /// `ResolutionScope`: Module, ModuleRef, AssemblyRef, TypeRef.
    ResolutionScope,
    /// `TypeOrMethodDef`: TypeDef, MethodDef (the owner of a generic parameter).
    TypeOrMethodDef,
}

impl CodedIndex {
    /// The tables this coded index selects, indexed by tag value. `NONE` marks an
    /// unused tag.
    fn variants(self) -> &'static [u8] {
        match self {
            CodedIndex::TypeDefOrRef => &[table::TYPE_DEF, table::TYPE_REF, table::TYPE_SPEC],
            CodedIndex::HasConstant => &[table::FIELD, table::PARAM, table::PROPERTY],
            CodedIndex::HasCustomAttribute => &[
                table::METHOD_DEF,
                table::FIELD,
                table::TYPE_REF,
                table::TYPE_DEF,
                table::PARAM,
                table::INTERFACE_IMPL,
                table::MEMBER_REF,
                table::MODULE,
                table::DECL_SECURITY,
                table::PROPERTY,
                table::EVENT,
                table::STAND_ALONE_SIG,
                table::MODULE_REF,
                table::TYPE_SPEC,
                table::ASSEMBLY,
                table::ASSEMBLY_REF,
                table::FILE,
                table::EXPORTED_TYPE,
                table::MANIFEST_RESOURCE,
            ],
            CodedIndex::HasFieldMarshal => &[table::FIELD, table::PARAM],
            CodedIndex::HasDeclSecurity => &[table::TYPE_DEF, table::METHOD_DEF, table::ASSEMBLY],
            CodedIndex::MemberRefParent => &[
                table::TYPE_DEF,
                table::TYPE_REF,
                table::MODULE_REF,
                table::METHOD_DEF,
                table::TYPE_SPEC,
            ],
            CodedIndex::HasSemantics => &[table::EVENT, table::PROPERTY],
            CodedIndex::MethodDefOrRef => &[table::METHOD_DEF, table::MEMBER_REF],
            CodedIndex::MemberForwarded => &[table::FIELD, table::METHOD_DEF],
            CodedIndex::Implementation => &[table::FILE, table::ASSEMBLY_REF, table::EXPORTED_TYPE],
            CodedIndex::CustomAttributeType => &[NONE, NONE, table::METHOD_DEF, table::MEMBER_REF],
            CodedIndex::ResolutionScope => &[
                table::MODULE,
                table::MODULE_REF,
                table::ASSEMBLY_REF,
                table::TYPE_REF,
            ],
            CodedIndex::TypeOrMethodDef => &[table::TYPE_DEF, table::METHOD_DEF],
        }
    }

    /// The number of tag bits: the bits needed to index `variants`, that is
    /// ceil(log2(count)).
    fn tag_bits(self) -> u32 {
        let count = self.variants().len() as u32;
        u32::BITS - (count - 1).leading_zeros()
    }

    /// The column width in bytes (2 or 4) given the table row counts.
    #[must_use]
    pub fn size(self, header: &TablesHeader) -> usize {
        let max_rows = self
            .variants()
            .iter()
            .filter(|&&t| t != NONE)
            .map(|&t| header.row_count(t))
            .max()
            .unwrap_or(0);
        let room = 1u64 << (16 - self.tag_bits());
        if u64::from(max_rows) < room { 2 } else { 4 }
    }

    /// Decodes a raw coded-index value into a [`Token`]. A tag with no table or a
    /// row of 0 yields the nil token.
    #[must_use]
    pub fn decode(self, raw: u32) -> Token {
        let bits = self.tag_bits();
        let tag = (raw & ((1 << bits) - 1)) as usize;
        let row = raw >> bits;
        match self.variants().get(tag) {
            Some(&t) if t != NONE => Token::new(t, row),
            _ => Token::new(0, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::TablesHeader;
    use alloc::vec::Vec;

    fn header_with(rows: &[(u8, u32)]) -> Vec<u8> {
        let mut stream = Vec::new();
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&[2, 0, 0, 0]);
        let mut valid = 0u64;
        for &(t, _) in rows {
            valid |= 1u64 << t;
        }
        stream.extend_from_slice(&valid.to_le_bytes());
        stream.extend_from_slice(&0u64.to_le_bytes());
        for table in 0u8..64 {
            if let Some(&(_, count)) = rows.iter().find(|&&(t, _)| t == table) {
                stream.extend_from_slice(&count.to_le_bytes());
            }
        }
        stream
    }

    #[test]
    fn tag_bits_are_the_ceil_log2_of_the_variant_count() {
        assert_eq!(CodedIndex::MethodDefOrRef.tag_bits(), 1);
        assert_eq!(CodedIndex::TypeDefOrRef.tag_bits(), 2);
        assert_eq!(CodedIndex::MemberRefParent.tag_bits(), 3);
        assert_eq!(CodedIndex::HasCustomAttribute.tag_bits(), 5);
    }

    #[test]
    fn small_tables_give_a_two_byte_index() {
        let stream = header_with(&[(table::TYPE_DEF, 10), (table::TYPE_REF, 4)]);
        let header = TablesHeader::parse(&stream).unwrap();
        assert_eq!(CodedIndex::TypeDefOrRef.size(&header), 2);
    }

    #[test]
    fn a_large_referenced_table_widens_the_index() {
        let stream = header_with(&[(table::TYPE_DEF, 20_000)]);
        let header = TablesHeader::parse(&stream).unwrap();
        assert_eq!(CodedIndex::TypeDefOrRef.size(&header), 4);
    }

    #[test]
    fn decode_splits_tag_and_row() {
        let token = CodedIndex::TypeDefOrRef.decode((5 << 2) | 1);
        assert_eq!(token.table(), table::TYPE_REF);
        assert_eq!(token.row(), 5);
    }
}
