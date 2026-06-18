//! A navigable reader over an assembly's metadata.

use crate::constant::{ConstantValue, decode_constant};
use crate::flags;
use crate::heaps::StringsHeap;
use crate::image::{MetadataError, MetadataImage};
use crate::pe::PeImage;
use crate::rows::Tables;
use crate::signature::{MethodSig, SigType, parse_field, parse_method};
use crate::tables::{TableError, TablesHeader, table};
use lamella_cil::{MethodBodyImage, read_method_body};
use lamella_token::Token;

impl From<TableError> for MetadataError {
    fn from(_: TableError) -> MetadataError {
        MetadataError::Truncated
    }
}

/// A type's namespace and name (II.22.37), as borrowed `#Strings` slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeName<'a> {
    /// The namespace, empty for the global namespace.
    pub namespace: &'a str,
    /// The unqualified type name.
    pub name: &'a str,
}

/// A custom attribute applied to a target (II.22.10): the constructor it invokes
/// and its raw argument blob (decoding the blob per II.23.3 is left to callers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CustomAttribute<'a> {
    /// The token of the attribute's constructor (a `MethodDef` or `MemberRef`).
    pub constructor: Token,
    /// The raw argument blob.
    pub value: &'a [u8],
}

/// A read-only, navigable view of one assembly's metadata.
#[derive(Debug, Clone, Copy)]
pub struct Assembly<'a> {
    image: MetadataImage<'a>,
    tables: Tables<'a>,
    /// The whole PE file, kept so method bodies (addressed by RVA) can be read.
    /// `None` when built from a bare metadata image.
    file: Option<&'a [u8]>,
}

impl<'a> Assembly<'a> {
    /// Reads a managed PE file into a navigable assembly.
    pub fn read(file: &'a [u8]) -> Result<Assembly<'a>, MetadataError> {
        let mut assembly = Assembly::from_image(MetadataImage::read(file)?)?;
        assembly.file = Some(file);
        Ok(assembly)
    }

    /// Builds the navigable view over an already-parsed metadata image. Method
    /// bodies are unavailable (no backing PE) when built this way.
    pub fn from_image(image: MetadataImage<'a>) -> Result<Assembly<'a>, MetadataError> {
        let header = TablesHeader::parse(image.tables())?;
        let tables = Tables::new(header)?;
        Ok(Assembly {
            image,
            tables,
            file: None,
        })
    }

    /// The underlying metadata image.
    #[must_use]
    pub fn image(&self) -> &MetadataImage<'a> {
        &self.image
    }

    /// The parsed tables.
    #[must_use]
    pub fn tables(&self) -> &Tables<'a> {
        &self.tables
    }

    fn strings(&self) -> StringsHeap<'a> {
        self.image.strings()
    }

    /// The module's name (the single `Module` row, II.22.30).
    #[must_use]
    pub fn module_name(&self) -> Option<&'a str> {
        let module = self.tables.row(table::MODULE, 1)?;
        self.strings().get(module.raw(1)).ok()
    }

    /// The number of type definitions (II.22.37).
    #[must_use]
    pub fn type_count(&self) -> u32 {
        self.tables.row_count(table::TYPE_DEF)
    }

    /// The namespace and name of the 1-based `index`-th type definition. Row 1 is
    /// the `<Module>` pseudo-type (II.22.37).
    #[must_use]
    pub fn type_name(&self, index: u32) -> Option<TypeName<'a>> {
        let row = self.tables.row(table::TYPE_DEF, index)?;
        let name = self.strings().get(row.raw(1)).ok()?;
        let namespace = self.strings().get(row.raw(2)).ok()?;
        Some(TypeName { namespace, name })
    }

    /// Iterates the type definitions by namespace and name.
    pub fn types(&self) -> impl Iterator<Item = TypeName<'a>> + '_ {
        (1..=self.type_count()).filter_map(move |index| self.type_name(index))
    }

    /// The 1-based `index`-th type definition as a navigable view.
    #[must_use]
    pub fn type_def(&self, index: u32) -> Option<TypeDef<'a>> {
        if index == 0 || index > self.type_count() {
            return None;
        }
        Some(TypeDef {
            assembly: *self,
            index,
        })
    }

    /// Iterates the type definitions as navigable views.
    pub fn type_defs(&self) -> impl Iterator<Item = TypeDef<'a>> + '_ {
        (1..=self.type_count()).filter_map(move |index| self.type_def(index))
    }

    /// The type definition with the given namespace and name, if any. The binder's
    /// basic resolution step against a reference assembly.
    #[must_use]
    pub fn find_type(&self, namespace: &str, name: &str) -> Option<TypeDef<'a>> {
        self.type_defs()
            .find(|type_def| type_def.name() == Some(TypeName { namespace, name }))
    }

    /// This assembly's own simple name (the `Assembly` row, II.22.2), if present.
    #[must_use]
    pub fn assembly_name(&self) -> Option<&'a str> {
        let row = self.tables.row(table::ASSEMBLY, 1)?;
        self.strings().get(row.raw(7)).ok()
    }

    /// The 1-based `index`-th `TypeRef` (a referenced type, II.22.38).
    #[must_use]
    pub fn type_ref(&self, index: u32) -> Option<TypeRef<'a>> {
        if index == 0 || index > self.tables.row_count(table::TYPE_REF) {
            return None;
        }
        Some(TypeRef {
            assembly: *self,
            index,
        })
    }

    /// Iterates the referenced types.
    pub fn type_refs(&self) -> impl Iterator<Item = TypeRef<'a>> + '_ {
        (1..=self.tables.row_count(table::TYPE_REF)).filter_map(move |index| self.type_ref(index))
    }

    /// The 1-based `index`-th `AssemblyRef` (a referenced assembly, II.22.5).
    #[must_use]
    pub fn assembly_ref(&self, index: u32) -> Option<AssemblyRef<'a>> {
        if index == 0 || index > self.tables.row_count(table::ASSEMBLY_REF) {
            return None;
        }
        Some(AssemblyRef {
            assembly: *self,
            index,
        })
    }

    /// Iterates the referenced assemblies.
    pub fn assembly_refs(&self) -> impl Iterator<Item = AssemblyRef<'a>> + '_ {
        (1..=self.tables.row_count(table::ASSEMBLY_REF))
            .filter_map(move |index| self.assembly_ref(index))
    }

    /// The custom attributes applied to `parent`, from the `CustomAttribute`
    /// table (II.22.10): each yields the constructor token and the raw value blob.
    pub fn custom_attributes(
        &self,
        parent: Token,
    ) -> impl Iterator<Item = CustomAttribute<'a>> + '_ {
        let blob = self.image.blob();
        (1..=self.tables.row_count(table::CUSTOM_ATTRIBUTE)).filter_map(move |index| {
            let row = self.tables.row(table::CUSTOM_ATTRIBUTE, index)?;
            let owner = row.token(0);
            if owner.table() == parent.table() && owner.row() == parent.row() {
                Some(CustomAttribute {
                    constructor: row.token(1),
                    value: blob.get(row.raw(2)).unwrap_or(&[]),
                })
            } else {
                None
            }
        })
    }

    /// The constant value attached to `parent` (a Field/Param/Property token),
    /// from the `Constant` table (II.22.9), or `None` if it has no constant.
    #[must_use]
    pub fn constant(&self, parent: Token) -> Option<ConstantValue> {
        for index in 1..=self.tables.row_count(table::CONSTANT) {
            let row = self.tables.row(table::CONSTANT, index)?;
            let owner = row.token(1);
            if owner.table() == parent.table() && owner.row() == parent.row() {
                let element_type = row.raw(0) as u8;
                let blob = self.image.blob().get(row.raw(2)).ok()?;
                return decode_constant(element_type, blob);
            }
        }
        None
    }

    /// The `[first, last)` child run for a type located through a map table
    /// (`PropertyMap`/`EventMap`, II.22.35/12): find the map row for the type,
    /// then take its list column to the next map row's (or the child table end).
    fn mapped_range(&self, map: u8, type_index: u32, child: u8) -> (u32, u32) {
        let map_rows = self.tables.row_count(map);
        for map_index in 1..=map_rows {
            let Some(row) = self.tables.row(map, map_index) else {
                break;
            };
            if row.raw(0) == type_index {
                let first = row.raw(1);
                let last = if map_index < map_rows {
                    self.tables
                        .row(map, map_index + 1)
                        .map_or(first, |next| next.raw(1))
                } else {
                    self.tables.row_count(child) + 1
                };
                return (first, last);
            }
        }
        (1, 1)
    }

    /// The half-open `[first, last)` range of child rows owned by an owner row,
    /// from the owner's list column and the next owner's (II.22.11 run pattern).
    fn child_range(&self, owner: u8, owner_index: u32, list_col: usize, child: u8) -> (u32, u32) {
        let first = self
            .tables
            .row(owner, owner_index)
            .map_or(1, |row| row.raw(list_col));
        let last = if owner_index < self.tables.row_count(owner) {
            self.tables
                .row(owner, owner_index + 1)
                .map_or(first, |row| row.raw(list_col))
        } else {
            self.tables.row_count(child) + 1
        };
        (first, last)
    }
}

/// A navigable type definition (II.22.37).
#[derive(Debug, Clone, Copy)]
pub struct TypeDef<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> TypeDef<'a> {
    /// The type's namespace and name.
    #[must_use]
    pub fn name(&self) -> Option<TypeName<'a>> {
        self.assembly.type_name(self.index)
    }

    /// The type attribute flags (II.23.1.15).
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::TYPE_DEF, self.index)
            .map_or(0, |row| row.raw(0))
    }

    /// The base-class/interface token from the `Extends` column, nil for none.
    #[must_use]
    pub fn extends(&self) -> Token {
        self.assembly
            .tables
            .row(table::TYPE_DEF, self.index)
            .map_or(Token::new(0, 0), |row| row.token(3))
    }

    /// Whether the type is public.
    #[must_use]
    pub fn is_public(&self) -> bool {
        flags::type_is_public(self.flags())
    }

    /// Whether the type is an interface.
    #[must_use]
    pub fn is_interface(&self) -> bool {
        flags::type_is_interface(self.flags())
    }

    /// Whether the type is abstract.
    #[must_use]
    pub fn is_abstract(&self) -> bool {
        flags::type_is_abstract(self.flags())
    }

    /// Whether the type is sealed.
    #[must_use]
    pub fn is_sealed(&self) -> bool {
        flags::type_is_sealed(self.flags())
    }

    /// The type's fields.
    pub fn fields(&self) -> impl Iterator<Item = Field<'a>> + '_ {
        let (first, last) = self
            .assembly
            .child_range(table::TYPE_DEF, self.index, 4, table::FIELD);
        (first..last).map(move |index| Field {
            assembly: self.assembly,
            index,
        })
    }

    /// The type's methods.
    pub fn methods(&self) -> impl Iterator<Item = Method<'a>> + '_ {
        let (first, last) =
            self.assembly
                .child_range(table::TYPE_DEF, self.index, 5, table::METHOD_DEF);
        (first..last).map(move |index| Method {
            assembly: self.assembly,
            index,
        })
    }

    /// The interfaces this type implements, as `TypeDefOrRef` tokens (II.22.23).
    /// `InterfaceImpl` is a side table keyed by the implementing type, so this
    /// scans it for rows whose `Class` is this type.
    pub fn interfaces(&self) -> impl Iterator<Item = Token> + '_ {
        let index = self.index;
        (1..=self.assembly.tables.row_count(table::INTERFACE_IMPL)).filter_map(move |row_index| {
            let row = self.assembly.tables.row(table::INTERFACE_IMPL, row_index)?;
            (row.raw(0) == index).then(|| row.token(1))
        })
    }

    /// The custom attributes applied to this type.
    pub fn custom_attributes(&self) -> impl Iterator<Item = CustomAttribute<'a>> + '_ {
        self.assembly
            .custom_attributes(Token::new(table::TYPE_DEF, self.index))
    }

    /// The type's properties (II.22.34, located through `PropertyMap`).
    pub fn properties(&self) -> impl Iterator<Item = Property<'a>> + '_ {
        let (first, last) =
            self.assembly
                .mapped_range(table::PROPERTY_MAP, self.index, table::PROPERTY);
        (first..last).map(move |index| Property {
            assembly: self.assembly,
            index,
        })
    }

    /// The type's events (II.22.13, located through `EventMap`).
    pub fn events(&self) -> impl Iterator<Item = Event<'a>> + '_ {
        let (first, last) = self
            .assembly
            .mapped_range(table::EVENT_MAP, self.index, table::EVENT);
        (first..last).map(move |index| Event {
            assembly: self.assembly,
            index,
        })
    }

    /// The type this type is nested in, if any (II.22.32).
    #[must_use]
    pub fn enclosing_type(&self) -> Option<TypeDef<'a>> {
        for index in 1..=self.assembly.tables.row_count(table::NESTED_CLASS) {
            let row = self.assembly.tables.row(table::NESTED_CLASS, index)?;
            if row.raw(0) == self.index {
                return self.assembly.type_def(row.raw(1));
            }
        }
        None
    }

    /// The types nested directly within this type (II.22.32).
    pub fn nested_types(&self) -> impl Iterator<Item = TypeDef<'a>> + '_ {
        let enclosing = self.index;
        (1..=self.assembly.tables.row_count(table::NESTED_CLASS)).filter_map(move |index| {
            let row = self.assembly.tables.row(table::NESTED_CLASS, index)?;
            (row.raw(1) == enclosing).then(|| self.assembly.type_def(row.raw(0)))?
        })
    }
}

/// A navigable property definition (II.22.34).
#[derive(Debug, Clone, Copy)]
pub struct Property<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> Property<'a> {
    /// The property's name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::PROPERTY, self.index)?;
        self.assembly.strings().get(row.raw(1)).ok()
    }

    /// The property attribute flags (II.23.1.14).
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::PROPERTY, self.index)
            .map_or(0, |row| row.raw(0))
    }
}

/// A navigable event definition (II.22.13).
#[derive(Debug, Clone, Copy)]
pub struct Event<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> Event<'a> {
    /// The event's name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::EVENT, self.index)?;
        self.assembly.strings().get(row.raw(1)).ok()
    }

    /// The event's delegate type, as a `TypeDefOrRef` token.
    #[must_use]
    pub fn event_type(&self) -> Token {
        self.assembly
            .tables
            .row(table::EVENT, self.index)
            .map_or(Token::new(0, 0), |row| row.token(2))
    }
}

/// A navigable field definition (II.22.15).
#[derive(Debug, Clone, Copy)]
pub struct Field<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> Field<'a> {
    /// The field's name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::FIELD, self.index)?;
        self.assembly.strings().get(row.raw(1)).ok()
    }

    /// The field attribute flags (II.23.1.5).
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::FIELD, self.index)
            .map_or(0, |row| row.raw(0))
    }

    /// The field's decoded type signature.
    #[must_use]
    pub fn signature(&self) -> Option<SigType> {
        let row = self.assembly.tables.row(table::FIELD, self.index)?;
        let blob = self.assembly.image.blob().get(row.raw(2)).ok()?;
        parse_field(blob).ok()
    }

    /// Whether the field is static.
    #[must_use]
    pub fn is_static(&self) -> bool {
        flags::field_is_static(self.flags())
    }

    /// Whether the field is a `const` literal.
    #[must_use]
    pub fn is_literal(&self) -> bool {
        flags::field_is_literal(self.flags())
    }

    /// The field's constant value, if it has one (a `const` field or enum member).
    #[must_use]
    pub fn constant(&self) -> Option<ConstantValue> {
        self.assembly.constant(Token::new(table::FIELD, self.index))
    }
}

/// A navigable method definition (II.22.26).
#[derive(Debug, Clone, Copy)]
pub struct Method<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> Method<'a> {
    /// The method's name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::METHOD_DEF, self.index)?;
        self.assembly.strings().get(row.raw(3)).ok()
    }

    /// The method attribute flags (II.23.1.10).
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::METHOD_DEF, self.index)
            .map_or(0, |row| row.raw(2))
    }

    /// The relative virtual address of the method body, 0 for none (abstract,
    /// extern).
    #[must_use]
    pub fn rva(&self) -> u32 {
        self.assembly
            .tables
            .row(table::METHOD_DEF, self.index)
            .map_or(0, |row| row.raw(0))
    }

    /// The method's decoded signature.
    #[must_use]
    pub fn signature(&self) -> Option<MethodSig> {
        let row = self.assembly.tables.row(table::METHOD_DEF, self.index)?;
        let blob = self.assembly.image.blob().get(row.raw(4)).ok()?;
        parse_method(blob).ok()
    }

    /// The method's CIL body, decoded through [`lamella_cil`]. `None` for a method
    /// with no body (abstract, extern), or when the assembly was built from a bare
    /// metadata image with no backing PE.
    #[must_use]
    pub fn body(&self) -> Option<MethodBodyImage> {
        let file = self.assembly.file?;
        let rva = self.rva();
        if rva == 0 {
            return None;
        }
        let offset = PeImage::parse(file).ok()?.rva_to_offset(rva).ok()?;
        read_method_body(file.get(offset..)?).ok()
    }

    /// Whether the method is static.
    #[must_use]
    pub fn is_static(&self) -> bool {
        flags::method_is_static(self.flags())
    }

    /// Whether the method is virtual.
    #[must_use]
    pub fn is_virtual(&self) -> bool {
        flags::method_is_virtual(self.flags())
    }

    /// Whether the method is abstract.
    #[must_use]
    pub fn is_abstract(&self) -> bool {
        flags::method_is_abstract(self.flags())
    }

    /// The method's declared parameters (II.22.33). Note the parameter list may
    /// include a row for the return value (sequence 0).
    pub fn params(&self) -> impl Iterator<Item = Param<'a>> + '_ {
        let (first, last) =
            self.assembly
                .child_range(table::METHOD_DEF, self.index, 5, table::PARAM);
        (first..last).map(move |index| Param {
            assembly: self.assembly,
            index,
        })
    }
}

/// A navigable parameter definition (II.22.33).
#[derive(Debug, Clone, Copy)]
pub struct Param<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> Param<'a> {
    /// The parameter's name, if recorded.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::PARAM, self.index)?;
        self.assembly.strings().get(row.raw(2)).ok()
    }

    /// The parameter attribute flags (II.23.1.13).
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::PARAM, self.index)
            .map_or(0, |row| row.raw(0))
    }

    /// The 1-based parameter position; 0 marks the return value's row.
    #[must_use]
    pub fn sequence(&self) -> u32 {
        self.assembly
            .tables
            .row(table::PARAM, self.index)
            .map_or(0, |row| row.raw(1))
    }
}

/// A navigable referenced-type row (II.22.38).
#[derive(Debug, Clone, Copy)]
pub struct TypeRef<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> TypeRef<'a> {
    /// The referenced type's namespace and name.
    #[must_use]
    pub fn name(&self) -> Option<TypeName<'a>> {
        let row = self.assembly.tables.row(table::TYPE_REF, self.index)?;
        let name = self.assembly.strings().get(row.raw(1)).ok()?;
        let namespace = self.assembly.strings().get(row.raw(2)).ok()?;
        Some(TypeName { namespace, name })
    }

    /// The resolution scope token: where the type is defined (an `AssemblyRef`,
    /// `ModuleRef`, `Module`, or enclosing `TypeRef`).
    #[must_use]
    pub fn resolution_scope(&self) -> Token {
        self.assembly
            .tables
            .row(table::TYPE_REF, self.index)
            .map_or(Token::new(0, 0), |row| row.token(0))
    }
}

/// A navigable referenced-assembly row (II.22.5).
#[derive(Debug, Clone, Copy)]
pub struct AssemblyRef<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> AssemblyRef<'a> {
    /// The referenced assembly's simple name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::ASSEMBLY_REF, self.index)?;
        self.assembly.strings().get(row.raw(6)).ok()
    }

    /// The referenced assembly's version `(major, minor, build, revision)`.
    #[must_use]
    pub fn version(&self) -> (u16, u16, u16, u16) {
        self.assembly
            .tables
            .row(table::ASSEMBLY_REF, self.index)
            .map_or((0, 0, 0, 0), |row| {
                (
                    row.raw(0) as u16,
                    row.raw(1) as u16,
                    row.raw(2) as u16,
                    row.raw(3) as u16,
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::MetadataImage;
    use alloc::vec::Vec;

    const METADATA_SIGNATURE: u32 = 0x424A_5342;

    /// Builds a metadata root with a `#Strings` heap and a `#~` stream holding one
    /// Module row and two TypeDef rows (`<Module>` and `Foo.Bar`).
    fn synthetic_assembly() -> Vec<u8> {
        let mut strings = Vec::new();
        strings.push(0);
        strings.extend_from_slice(b"MyModule\0");
        strings.extend_from_slice(b"<Module>\0");
        strings.extend_from_slice(b"Bar\0");
        strings.extend_from_slice(b"Foo\0");
        strings.extend_from_slice(b"Object\0");
        strings.extend_from_slice(b"System\0");

        let mut tables = Vec::new();
        tables.extend_from_slice(&0u32.to_le_bytes());
        tables.extend_from_slice(&[2, 0, 0, 0]);
        let valid = (1u64 << table::MODULE) | (1u64 << table::TYPE_REF) | (1u64 << table::TYPE_DEF);
        tables.extend_from_slice(&valid.to_le_bytes());
        tables.extend_from_slice(&0u64.to_le_bytes());
        tables.extend_from_slice(&1u32.to_le_bytes());
        tables.extend_from_slice(&1u32.to_le_bytes());
        tables.extend_from_slice(&2u32.to_le_bytes());
        tables.extend_from_slice(&[0, 0]);
        tables.extend_from_slice(&1u16.to_le_bytes());
        tables.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        tables.extend_from_slice(&0u16.to_le_bytes());
        tables.extend_from_slice(&27u16.to_le_bytes());
        tables.extend_from_slice(&34u16.to_le_bytes());
        tables.extend_from_slice(&0u32.to_le_bytes());
        tables.extend_from_slice(&10u16.to_le_bytes());
        tables.extend_from_slice(&0u16.to_le_bytes());
        tables.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
        tables.extend_from_slice(&0u32.to_le_bytes());
        tables.extend_from_slice(&19u16.to_le_bytes());
        tables.extend_from_slice(&23u16.to_le_bytes());
        tables.extend_from_slice(&[0, 0, 1, 0, 1, 0]);

        let mut root = Vec::new();
        root.extend_from_slice(&METADATA_SIGNATURE.to_le_bytes());
        root.extend_from_slice(&[1, 0, 1, 0]);
        root.extend_from_slice(&0u32.to_le_bytes());
        root.extend_from_slice(&4u32.to_le_bytes());
        root.extend_from_slice(b"v1\0\0");
        root.extend_from_slice(&0u16.to_le_bytes());
        root.extend_from_slice(&2u16.to_le_bytes());
        let headers_len = 24 + (8 + 12) + (8 + 4);
        let strings_offset = headers_len;
        let tables_offset = headers_len + strings.len();
        root.extend_from_slice(&(strings_offset as u32).to_le_bytes());
        root.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        root.extend_from_slice(b"#Strings\0\0\0\0");
        root.extend_from_slice(&(tables_offset as u32).to_le_bytes());
        root.extend_from_slice(&(tables.len() as u32).to_le_bytes());
        root.extend_from_slice(b"#~\0\0");
        assert_eq!(root.len(), headers_len);
        root.extend_from_slice(&strings);
        root.extend_from_slice(&tables);
        root
    }

    #[test]
    fn reads_type_refs_with_scope_and_name() {
        let root = synthetic_assembly();
        let image = MetadataImage::parse_metadata_root(&root).unwrap();
        let assembly = Assembly::from_image(image).unwrap();
        let object = assembly.type_ref(1).unwrap();
        assert_eq!(
            object.name(),
            Some(TypeName {
                namespace: "System",
                name: "Object"
            })
        );
        assert!(object.resolution_scope().is_nil());
        assert_eq!(assembly.type_count(), 2);
    }

    #[test]
    fn type_def_views_expose_name_flags_and_empty_member_ranges() {
        let root = synthetic_assembly();
        let image = MetadataImage::parse_metadata_root(&root).unwrap();
        let assembly = Assembly::from_image(image).unwrap();
        let foo_bar = assembly.type_def(2).unwrap();
        assert_eq!(
            foo_bar.name(),
            Some(TypeName {
                namespace: "Foo",
                name: "Bar"
            })
        );
        assert_eq!(foo_bar.flags(), 0);
        assert!(foo_bar.extends().is_nil());
        assert_eq!(foo_bar.fields().count(), 0);
        assert_eq!(foo_bar.methods().count(), 0);
        assert_eq!(foo_bar.interfaces().count(), 0);
        assert_eq!(foo_bar.custom_attributes().count(), 0);
        assert_eq!(foo_bar.properties().count(), 0);
        assert_eq!(foo_bar.events().count(), 0);
        assert!(foo_bar.enclosing_type().is_none());
        assert_eq!(foo_bar.nested_types().count(), 0);
        assert!(assembly.type_def(0).is_none());
        assert!(assembly.type_def(3).is_none());
    }

    #[test]
    fn enumerates_the_module_and_types() {
        let root = synthetic_assembly();
        let image = MetadataImage::parse_metadata_root(&root).unwrap();
        let assembly = Assembly::from_image(image).unwrap();
        assert_eq!(assembly.module_name(), Some("MyModule"));
        assert_eq!(assembly.type_count(), 2);
        assert!(assembly.find_type("Foo", "Bar").is_some());
        assert!(assembly.find_type("", "Nope").is_none());
        let names: Vec<_> = assembly.types().collect();
        assert_eq!(
            names,
            [
                TypeName {
                    namespace: "",
                    name: "<Module>"
                },
                TypeName {
                    namespace: "Foo",
                    name: "Bar"
                },
            ]
        );
    }
}
