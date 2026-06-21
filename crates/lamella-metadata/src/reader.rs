//! A navigable reader over an assembly's metadata.

use crate::constant::{ConstantValue, decode_constant};
use crate::flags;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use crate::heaps::StringsHeap;
use crate::image::{MetadataError, MetadataImage};
use crate::layout::{LayoutError, TargetLayout, TypeLayout, layout_value_type};
use crate::pe::PeImage;
use crate::rows::Tables;
use crate::signature::{
    MethodSig, SigType, calling, parse_field, parse_local_var_sig, parse_method, parse_type,
};
use crate::tables::{TableError, TablesHeader, table};
use lamella_cil::{EhKind, MethodBodyImage, instruction_offsets, read_method_body};
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

/// One exception-handling clause of a method ([`Method::exception_clauses`]), with its
/// protected and handler regions as IL BYTE offsets (II.25.4.6) -- the form an AOT/CFG
/// lowering maps onto its basic blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceptionClause {
    /// What the handler is, and any caught type or filter it carries.
    pub kind: ExceptionHandlerKind,
    /// IL byte offset where the protected (try) region begins.
    pub try_offset: u32,
    /// Byte length of the protected region.
    pub try_length: u32,
    /// IL byte offset where the handler region begins.
    pub handler_offset: u32,
    /// Byte length of the handler region.
    pub handler_length: u32,
}

/// What an [`ExceptionClause`] handler does (II.25.4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionHandlerKind {
    /// A typed `catch`, carrying the caught type's metadata token.
    Catch(Token),
    /// A filtered handler; the filter expression begins at this IL byte offset.
    Filter {
        /// The IL byte offset where the filter expression starts.
        filter_offset: u32,
    },
    /// A `finally` handler, run on both normal exit and exception.
    Finally,
    /// A `fault` handler, run only when an exception leaves the try region.
    Fault,
}

/// The exception tag for a type's `namespace` + `name`: FNV-1a 32-bit over the full name
/// (`"System" + "." + "DivideByZeroException"`), the high bit forced so every tag is a
/// nonzero failure value. The AOT computes a catch/throw tag from a token via
/// [`Assembly::exception_tag`]; the compiler computes a base-chain vector's tags by name
/// with this same function -- both agree because the formula is one.
#[must_use]
pub fn exception_tag_for_name(namespace: &str, name: &str) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    if !namespace.is_empty() {
        hash = fnv1a32(hash, namespace.as_bytes());
        hash = fnv1a32(hash, b".");
    }
    hash = fnv1a32(hash, name.as_bytes());
    hash | 0x8000_0000
}

/// FNV-1a 32-bit, folding `bytes` into the running `hash` (seed with the FNV offset basis).
fn fnv1a32(hash: u32, bytes: &[u8]) -> u32 {
    let mut hash = hash;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Encodes an exception base-chain tag vector as the `<ExceptionBaseChain>` attribute value
/// blob the compiler emits: a standard custom-attribute blob (II.23.3) for a single `uint[]`
/// positional argument -- a `0x0001` prolog, the array length, the little-endian `u32` tags,
/// then a zero named-argument count. The inverse of [`Assembly::exception_base_chain`]'s
/// decode, kept here so the emit and read sides share one format.
#[must_use]
pub fn encode_exception_base_chain(tags: &[u32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(8 + tags.len() * 4);
    blob.extend_from_slice(&0x0001u16.to_le_bytes());
    blob.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for tag in tags {
        blob.extend_from_slice(&tag.to_le_bytes());
    }
    blob.extend_from_slice(&0x0000u16.to_le_bytes());
    blob
}

/// Decodes a `<ExceptionBaseChain>` attribute value blob produced by
/// [`encode_exception_base_chain`]. `None` if the blob is malformed.
fn decode_exception_base_chain(blob: &[u8]) -> Option<Vec<u32>> {
    if blob.len() < 6 || blob[0] != 0x01 || blob[1] != 0x00 {
        return None;
    }
    let count = u32::from_le_bytes(blob[2..6].try_into().ok()?) as usize;
    let mut tags = Vec::with_capacity(count);
    let mut offset = 6;
    for _ in 0..count {
        let bytes = blob.get(offset..offset + 4)?;
        tags.push(u32::from_le_bytes(bytes.try_into().ok()?));
        offset += 4;
    }
    Some(tags)
}

/// A method a call token resolves to ([`Assembly::resolve_method`]): its name,
/// declaring type, and signature, plus where it is defined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMethod<'a> {
    /// The method's name.
    pub name: Option<&'a str>,
    /// The namespace and name of the type that declares it.
    pub declaring_type: Option<TypeName<'a>>,
    /// The decoded method signature.
    pub signature: Option<MethodSig>,
    /// Whether the method is defined here or referenced from elsewhere.
    pub kind: MethodKind,
}

/// Where a resolved method lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    /// Defined in this assembly: the `MethodDef` row index, which an AOT lowering
    /// maps to a compiled callee.
    Definition(u32),
    /// Referenced from another assembly (a `MemberRef`) -- e.g. a BCL method an AOT
    /// backend recognizes as an intrinsic.
    Reference,
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

    /// The 1-based `index`-th `MemberRef` (a referenced method or field, II.22.25).
    #[must_use]
    pub fn member_ref(&self, index: u32) -> Option<MemberRef<'a>> {
        if index == 0 || index > self.tables.row_count(table::MEMBER_REF) {
            return None;
        }
        Some(MemberRef {
            assembly: *self,
            index,
        })
    }

    /// Iterates the referenced members.
    pub fn member_refs(&self) -> impl Iterator<Item = MemberRef<'a>> + '_ {
        (1..=self.tables.row_count(table::MEMBER_REF))
            .filter_map(move |index| self.member_ref(index))
    }

    /// The decoded signature of the `TypeSpec` named by `token` (II.23.2.14): the type a
    /// `TypeSpec` row stands for, e.g. `SigType::Array { element, rank }` for `int[,]`.
    /// `None` when `token` is not a `TypeSpec` or its blob does not decode. This is the
    /// seam an AOT lowering uses to recover an array's element type and rank from the
    /// `TypeSpec` that a `newobj`/`Get`/`Set` `MemberRef` points at.
    #[must_use]
    pub fn type_spec_signature(&self, token: Token) -> Option<SigType> {
        if token.table() != table::TYPE_SPEC {
            return None;
        }
        let row = self.tables.row(table::TYPE_SPEC, token.row())?;
        let blob = self.image.blob().get(row.raw(0)).ok()?;
        parse_type(blob).ok()
    }

    /// The `MethodDef` at 1-based `index`, or `None` if out of range.
    #[must_use]
    pub fn method(&self, index: u32) -> Option<Method<'a>> {
        if index == 0 || index > self.tables.row_count(table::METHOD_DEF) {
            return None;
        }
        Some(Method {
            assembly: *self,
            index,
        })
    }

    /// The underlying method a `MethodSpec` token (II.22.29) instantiates: its
    /// `MethodDefOrRef` (a `MethodDef` or `MemberRef`), so a generic-method call (e.g.
    /// `Array.Empty<int>()`) can be resolved through the generic definition. `None` if the
    /// token is not a `MethodSpec` or is out of range.
    #[must_use]
    pub fn method_spec_method(&self, token: Token) -> Option<Token> {
        if token.table() != table::METHOD_SPEC {
            return None;
        }
        self.tables
            .row(table::METHOD_SPEC, token.row())
            .map(|row| row.token(0))
    }

    /// The raw initializer bytes a `Field` token's RVA points at (II.22.18 `FieldRVA`) --
    /// the data backing a `RuntimeHelpers.InitializeArray` (a constant array literal). The
    /// slice is sized exactly to the field's value-type byte size (its `ClassLayout`
    /// `ClassSize`, the compiler-synthesized `__StaticArrayInitTypeSize=N`), so the caller
    /// reads a precise blob. `None` if the field has no `FieldRVA` row, no resolvable size,
    /// or the RVA is unmapped (e.g. a bare metadata image with no backing PE).
    #[must_use]
    pub fn field_rva_data(&self, field_token: Token) -> Option<&'a [u8]> {
        if field_token.table() != table::FIELD {
            return None;
        }
        let field_index = field_token.row();
        let rva = (1..=self.tables.row_count(table::FIELD_RVA))
            .filter_map(|index| self.tables.row(table::FIELD_RVA, index))
            .find(|row| row.raw(1) == field_index)
            .map(|row| row.raw(0))
            .filter(|&rva| rva != 0)?;
        let size = self.field_rva_blob_size(field_index)? as usize;
        let file = self.file?;
        PeImage::parse(file).ok()?.slice_at_rva(rva, size).ok()
    }

    /// The byte size of an RVA-backed field's initializer blob: the `ClassSize` of the
    /// field's value type (the synthesized `__StaticArrayInitTypeSize=N`), from `ClassLayout`.
    fn field_rva_blob_size(&self, field_index: u32) -> Option<u32> {
        let blob = self
            .tables
            .row(table::FIELD, field_index)
            .and_then(|row| self.image.blob().get(row.raw(2)).ok())?;
        let SigType::ValueType(type_token) = parse_field(blob).ok()? else {
            return None;
        };
        if type_token.table() != table::TYPE_DEF {
            return None;
        }
        (1..=self.tables.row_count(table::CLASS_LAYOUT))
            .filter_map(|index| self.tables.row(table::CLASS_LAYOUT, index))
            .find(|row| row.raw(2) == type_token.row())
            .map(|row| row.raw(1))
    }

    /// Resolves a `call`/`callvirt` method token to its target: the name, declaring
    /// type, and signature, plus whether it is defined in this assembly (a
    /// `MethodDef`) or referenced from another (a `MemberRef`). This is what a
    /// consumer of decoded CIL -- an interpreter or an AOT lowering -- needs to bind
    /// a call to its callee. Returns `None` for a token that is neither.
    #[must_use]
    pub fn resolve_method(&self, token: Token) -> Option<ResolvedMethod<'a>> {
        match token.table() {
            table::METHOD_DEF => {
                let method = self.method(token.row())?;
                Some(ResolvedMethod {
                    name: method.name(),
                    declaring_type: self
                        .method_owner(token.row())
                        .and_then(|owner| owner.name()),
                    signature: method.signature(),
                    kind: MethodKind::Definition(token.row()),
                })
            }
            table::MEMBER_REF => {
                let member = self.member_ref(token.row())?;
                Some(ResolvedMethod {
                    name: member.name(),
                    declaring_type: self.type_token_name(member.parent()),
                    signature: member.method_signature(),
                    kind: MethodKind::Reference,
                })
            }
            _ => None,
        }
    }

    /// The layout of a value type (a struct or enum) named by a `TypeDef` token: its
    /// instance fields, in declaration order, placed per `target`, with the
    /// reference-offset map. Recurses into nested value-type fields. This is the one
    /// shared layout the backend (stack slots + GC stack maps) and the runtime (the
    /// flat heap + boxing) consume, so neither re-derives it. A field whose type is a
    /// value type in another assembly (a `TypeRef`) cannot be resolved here and is a
    /// `LayoutError::UnresolvedValueType`.
    pub fn value_type_layout(
        &self,
        token: Token,
        target: &TargetLayout,
    ) -> Result<TypeLayout, LayoutError> {
        if token.table() != table::TYPE_DEF {
            return Err(LayoutError::UnresolvedValueType(token));
        }
        let fields: Vec<SigType> = match self.type_def(token.row()) {
            Some(type_def) => type_def
                .fields()
                .filter(|field| !field.is_static())
                .filter_map(|field| field.signature())
                .collect(),
            None => return Err(LayoutError::UnresolvedValueType(token)),
        };
        layout_value_type(&fields, target, &|nested| {
            self.value_type_layout(nested, target).ok()
        })
    }

    /// The byte offset of an instance field within its declaring type, by the field's
    /// token -- the seam from a `Field` token to a layout offset. Finds the type that
    /// declares the field, lays it out, and returns the offset of that field's slot
    /// (in declaration order among the instance fields). `None` if the token names no
    /// instance field of a layable type. The offset is from the type's field block;
    /// a reference type's object header, if any, is the runtime's to add.
    #[must_use]
    pub fn field_offset(&self, field: Token, target: &TargetLayout) -> Option<u32> {
        if field.table() != table::FIELD {
            return None;
        }
        for type_def in self.type_defs() {
            let mut index = 0usize;
            for candidate in type_def.fields() {
                if candidate.is_static() {
                    continue;
                }
                if candidate.token() == field {
                    let layout = self.value_type_layout(type_def.token(), target).ok()?;
                    return layout.field_offsets.get(index).copied();
                }
                index += 1;
            }
        }
        None
    }

    /// The signature type of a field, by its `Field` token -- the seam from an `ldfld`/`stfld`
    /// operand to the field's type, so the lowering can type the loaded value (a reference field
    /// reads an `ObjectRef`, not an `int`). `None` if the token names no field.
    #[must_use]
    pub fn field_signature(&self, field: Token) -> Option<SigType> {
        if field.table() != table::FIELD {
            return None;
        }
        Field {
            assembly: *self,
            index: field.row(),
        }
        .signature()
    }

    /// The `TypeDef` that owns the method at `method_index`, found by the method
    /// ranges the `TypeDef.MethodList` column delimits.
    fn method_owner(&self, method_index: u32) -> Option<TypeDef<'a>> {
        (1..=self.tables.row_count(table::TYPE_DEF)).find_map(|type_index| {
            let (first, last) = self.child_range(table::TYPE_DEF, type_index, 5, table::METHOD_DEF);
            (first..last).contains(&method_index).then_some(TypeDef {
                assembly: *self,
                index: type_index,
            })
        })
    }

    /// The namespace and name of a type token -- a `TypeRef` or `TypeDef` (the two
    /// `TypeDefOrRef` cases that name a concrete type, e.g. a `MemberRef` parent, a base
    /// type, or a `newarr` element); `None` for any other table (such as a `TypeSpec`,
    /// which carries a signature rather than a name). Also normalizes a signature's
    /// `Class`/`ValueType` token to a portable name for cross-assembly matching.
    #[must_use]
    pub fn type_token_name(&self, token: Token) -> Option<TypeName<'a>> {
        match token.table() {
            table::TYPE_REF => self
                .type_ref(token.row())
                .and_then(|type_ref| type_ref.name()),
            table::TYPE_DEF => self
                .type_def(token.row())
                .and_then(|type_def| type_def.name()),
            _ => None,
        }
    }

    /// The exception TAG for a type, for the no-GC tag-dispatch exception model: one `u32`
    /// identifying the exception type identically wherever it is named -- the throw site,
    /// every catch, and the runtime -- so the compiler, interpreter, and AOT backend never
    /// diverge. It is a deterministic FNV-1a hash of the type's full name with the high bit
    /// set, so it needs no shared table and is always a nonzero "failure" value (0 = no
    /// exception in flight). `0` if the token names no type.
    ///
    /// A per-name hash (not .NET's canonical HResults) is used because a tag-DISPATCH model
    /// needs tags UNIQUE per type, and the canonical HResults collide (e.g.
    /// ArgumentNullException and NullReferenceException are both 0x80004003). A future
    /// `[HResult(N)]` attribute will pin a chosen value; a well-known-BCL-HResult table is a
    /// later refinement layered on top.
    #[must_use]
    pub fn exception_tag(&self, type_token: Token) -> u32 {
        let Some(name) = self.type_token_name(type_token) else {
            return 0;
        };
        exception_tag_for_name(name.namespace, name.name)
    }

    /// The AOT exception base-chain tag vector for `type_token` -- `[tag(T), tag(base(T)),
    /// ..., tag(System.Exception)]`, leaf first -- read from the `<ExceptionBaseChain>`
    /// custom attribute the compiler emits on a referenced (BCL) exception type. The tags
    /// use the same formula as [`Assembly::exception_tag`].
    ///
    /// `None` for a type with no such attribute -- an in-program exception, whose chain the
    /// AOT walks itself via `extends()`. The middle-base `catch (BaseType)` subtype test is
    /// `catch_tag`'s membership in this vector (exact-match and catch-all need only the tag).
    #[must_use]
    pub fn exception_base_chain(&self, type_token: Token) -> Option<Vec<u32>> {
        for attribute in self.custom_attributes(type_token) {
            let is_chain = self
                .resolve_method(attribute.constructor)
                .and_then(|method| method.declaring_type)
                .is_some_and(|name| {
                    name.namespace.is_empty() && name.name == "<ExceptionBaseChain>"
                });
            if is_chain {
                return decode_exception_base_chain(attribute.value);
            }
        }
        None
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

    /// The `Param` row indices that carry `[System.ParamArrayAttribute]` -- a C# `params`
    /// array. Computed in ONE pass over the CustomAttribute table (II.22.10), so a caller
    /// can mark every `params` method at load without an O(rows)-per-parameter scan (which
    /// is catastrophic across a large reference assembly). `resolve_method` returns `None`
    /// for an unresolvable attribute ctor, so a foreign attribute is skipped.
    #[must_use]
    pub fn param_array_params(&self) -> BTreeSet<u32> {
        let mut params = BTreeSet::new();
        for index in 1..=self.tables.row_count(table::CUSTOM_ATTRIBUTE) {
            let Some(row) = self.tables.row(table::CUSTOM_ATTRIBUTE, index) else {
                continue;
            };
            let parent = row.token(0);
            if parent.table() != table::PARAM {
                continue;
            }
            let is_param_array = self
                .resolve_method(row.token(1))
                .and_then(|target| target.declaring_type)
                .is_some_and(|name| {
                    name.namespace == "System" && name.name == "ParamArrayAttribute"
                });
            if is_param_array {
                params.insert(parent.row());
            }
        }
        params
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

    /// This type's `TypeDef` token (e.g. to pass to [`Assembly::value_type_layout`]).
    #[must_use]
    pub fn token(&self) -> Token {
        Token::new(table::TYPE_DEF, self.index)
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

    /// Whether the type is a value type -- it extends `System.ValueType` or `System.Enum`.
    /// Reference types (classes) extend `System.Object` or another class, so this is how the
    /// backend tells a `newobj` of a struct (built in place) from one of a class (heap-allocated).
    #[must_use]
    pub fn is_value_type(&self) -> bool {
        self.assembly
            .type_token_name(self.extends())
            .is_some_and(|base| {
                base.namespace == "System" && matches!(base.name, "ValueType" | "Enum")
            })
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

    /// This type's explicit method overrides (II.22.27 `MethodImpl`), each as a
    /// `(body, declaration)` pair of `MethodDefOrRef` tokens: `body` is the method
    /// in this type that implements `declaration` (the overridden virtual/interface
    /// method). `MethodImpl` is a side table keyed by the implementing type's `Class`
    /// column, so this scans it for rows whose `Class` is this type -- the wiring an
    /// explicit interface member implementation (`int IA.Value()`) needs, since it is
    /// reachable only through the interface slot, never by its own (mangled) name.
    pub fn method_impls(&self) -> impl Iterator<Item = (Token, Token)> + '_ {
        let index = self.index;
        (1..=self.assembly.tables.row_count(table::METHOD_IMPL)).filter_map(move |row_index| {
            let row = self.assembly.tables.row(table::METHOD_IMPL, row_index)?;
            (row.raw(0) == index).then(|| (row.token(1), row.token(2)))
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

    /// This field's `Field` token.
    #[must_use]
    pub fn token(&self) -> Token {
        Token::new(table::FIELD, self.index)
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

    /// The method's `MethodDef` row index (its metadata rid) -- the key a Portable PDB
    /// uses for sequence points and local-variable names.
    #[must_use]
    pub fn rid(&self) -> u32 {
        self.index
    }

    /// The method attribute flags (II.23.1.10).
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::METHOD_DEF, self.index)
            .map_or(0, |row| row.raw(2))
    }

    /// The method implementation flags (II.23.1.11): the `ImplFlags` column. Its
    /// `CodeTypeMask` says whether the body is CIL, native, or provided by the runtime
    /// -- the last is the conforming seam a managed BCL method crosses to a runtime
    /// intrinsic (see [`crate::flags::method_impl`]).
    #[must_use]
    pub fn impl_flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::METHOD_DEF, self.index)
            .map_or(0, |row| row.raw(1))
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

    /// The method's local-variable types, resolving the body's local-variable
    /// signature (a `StandAloneSig`, II.23.2.6). The index in the returned vector is
    /// the local's slot number. Empty when the method declares no locals (or has no
    /// body). This is what an interpreter or AOT lowering needs to type its locals,
    /// and what `lamella-dap` needs to show them.
    #[must_use]
    pub fn local_variables(&self) -> Vec<SigType> {
        let Some(token) = self.body().and_then(|body| body.local_var_sig) else {
            return Vec::new();
        };
        self.assembly
            .tables
            .row(table::STAND_ALONE_SIG, token.row())
            .and_then(|row| self.assembly.image.blob().get(row.raw(0)).ok())
            .and_then(|blob| parse_local_var_sig(blob).ok())
            .unwrap_or_default()
    }

    /// The method's exception-handling clauses (II.25.4.6) with regions as IL BYTE offsets
    /// -- what an AOT/CFG lowering maps to its basic blocks. (`lamella-cil` decodes the EH
    /// table into instruction-index ranges; these are mapped back to byte offsets via the
    /// instruction layout, the same round-trip the body writer uses.) Empty when the method
    /// has no body or no handlers.
    #[must_use]
    pub fn exception_clauses(&self) -> Vec<ExceptionClause> {
        let Some(body) = self.body() else {
            return Vec::new();
        };
        if body.handlers.is_empty() {
            return Vec::new();
        }
        let Some(offsets) = instruction_offsets(&body.code) else {
            return Vec::new();
        };
        let byte_at = |index: u32| {
            offsets
                .get(index as usize)
                .copied()
                .unwrap_or_else(|| offsets.last().copied().unwrap_or(0))
        };
        body.handlers
            .iter()
            .map(|handler| {
                let try_offset = byte_at(handler.try_range.start);
                let handler_offset = byte_at(handler.handler_range.start);
                ExceptionClause {
                    kind: match handler.kind {
                        EhKind::Catch(token) => ExceptionHandlerKind::Catch(token),
                        EhKind::Filter { filter_start } => ExceptionHandlerKind::Filter {
                            filter_offset: byte_at(filter_start),
                        },
                        EhKind::Finally => ExceptionHandlerKind::Finally,
                        EhKind::Fault => ExceptionHandlerKind::Fault,
                    },
                    try_offset,
                    try_length: byte_at(handler.try_range.end).saturating_sub(try_offset),
                    handler_offset,
                    handler_length: byte_at(handler.handler_range.end)
                        .saturating_sub(handler_offset),
                }
            })
            .collect()
    }

    /// Whether the method is static.
    #[must_use]
    pub fn is_static(&self) -> bool {
        flags::method_is_static(self.flags())
    }

    /// Whether the method's body is provided by the runtime (II.23.1.11 `Runtime`): a
    /// bodyless method the runtime supplies. This is the conforming seam a managed BCL
    /// method crosses to a native intrinsic (the standard alternative to the
    /// non-conforming `internalcall`).
    #[must_use]
    pub fn is_runtime_impl(&self) -> bool {
        flags::method_impl_is_runtime(self.impl_flags())
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

    /// Whether a parameter carries `System.ParamArrayAttribute` (II.23.2) -- a C#
    /// `params` array, so the method is callable with a variable number of trailing
    /// arguments. Only the last parameter may, but any is checked.
    #[must_use]
    pub fn has_param_array(&self) -> bool {
        self.params().any(|param| {
            self.assembly
                .custom_attributes(param.token())
                .any(|attribute| {
                    self.assembly
                        .resolve_method(attribute.constructor)
                        .and_then(|target| target.declaring_type)
                        .is_some_and(|name| {
                            name.namespace == "System" && name.name == "ParamArrayAttribute"
                        })
                })
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

    /// This parameter's metadata token (for, e.g., its custom attributes).
    #[must_use]
    pub fn token(&self) -> Token {
        Token::new(table::PARAM, self.index)
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

    /// This referenced type's `TypeRef` metadata token.
    #[must_use]
    pub fn token(&self) -> Token {
        Token::new(table::TYPE_REF, self.index)
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

/// A navigable member reference (II.22.25): a method or field referenced through
/// its parent type, by name and signature. The runtime resolves a `call`/`ldfld`
/// token to this to find the target.
#[derive(Debug, Clone, Copy)]
pub struct MemberRef<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> MemberRef<'a> {
    /// The token of the member's parent (a `TypeRef`, `TypeDef`, `ModuleRef`,
    /// `MethodDef`, or `TypeSpec`).
    #[must_use]
    pub fn parent(&self) -> Token {
        self.assembly
            .tables
            .row(table::MEMBER_REF, self.index)
            .map_or(Token::new(0, 0), |row| row.token(0))
    }

    /// The referenced member's name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::MEMBER_REF, self.index)?;
        self.assembly.strings().get(row.raw(1)).ok()
    }

    /// The raw signature blob.
    #[must_use]
    pub fn signature_blob(&self) -> &'a [u8] {
        self.assembly
            .tables
            .row(table::MEMBER_REF, self.index)
            .and_then(|row| self.assembly.image.blob().get(row.raw(2)).ok())
            .unwrap_or(&[])
    }

    /// Whether the reference is to a field (its signature starts with the FIELD
    /// calling convention) rather than a method.
    #[must_use]
    pub fn is_field(&self) -> bool {
        self.signature_blob().first() == Some(&calling::FIELD)
    }

    /// The referenced method's signature, if this is a method reference.
    #[must_use]
    pub fn method_signature(&self) -> Option<MethodSig> {
        if self.is_field() {
            return None;
        }
        parse_method(self.signature_blob()).ok()
    }

    /// The referenced field's type, if this is a field reference.
    #[must_use]
    pub fn field_type(&self) -> Option<SigType> {
        if self.is_field() {
            parse_field(self.signature_blob()).ok()
        } else {
            None
        }
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
        assert_eq!(assembly.member_refs().count(), 0);
        assert!(assembly.member_ref(1).is_none());
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
