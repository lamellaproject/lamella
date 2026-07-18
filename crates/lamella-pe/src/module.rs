//! Assembling a managed module: the orchestration over heaps, tables, and the PE.

use crate::heap::{BlobHeapBuilder, GuidHeapBuilder, StringHeapBuilder, UserStringHeapBuilder};
use crate::pdb::{DebugDocument, MethodDebug, build_portable_pdb};
use crate::pe::{
    CLI_HEADER_SIZE, COMIMAGE_FLAGS_ILONLY, TEXT_RVA, cli_header, write_image_with_debug,
};
use crate::root::metadata_root;
use crate::signature::{TypeSig, method_signature};
use crate::tables::{Column, HeapSizes, TableStream};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_metadata::CodedIndex;
use lamella_metadata::tables::table;
use lamella_token::Token;

/// The runtime version string written into the metadata root.
const RUNTIME_VERSION: &str = "v4.0.30319";

fn align4(buffer: &mut Vec<u8>) {
    while buffer.len() % 4 != 0 {
        buffer.push(0);
    }
}

/// Derives a deterministic 20-byte debug id (16-byte GUID + 4-byte age) from the
/// module name via FNV-1a, so the PE debug directory and the PDB carry the same id
/// without a random source.
fn derive_pdb_id(module_name: &str) -> [u8; 20] {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in module_name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    let mut id = [0u8; 20];
    id[..8].copy_from_slice(&hash.to_le_bytes());
    id[8..16].copy_from_slice(&hash.rotate_left(32).to_le_bytes());
    id[16..].copy_from_slice(&1u32.to_le_bytes());
    id
}

/// A 16-byte RFC 4122 version-4 GUID whose payload is `bytes[..16]` -- the version and
/// variant nibbles set (at the .NET GUID byte positions) so it is a well-formed GUID, as
/// csc's deterministic MVID/debug ids are, with the rest content-derived.
fn guid_from(bytes: &[u8; 16]) -> [u8; 16] {
    let mut g = *bytes;
    g[7] = (g[7] & 0x0f) | 0x40;
    g[8] = (g[8] & 0x3f) | 0x80;
    g
}

/// Assembles a single managed module into a PE image.
pub struct ImageBuilder {
    strings: StringHeapBuilder,
    blobs: BlobHeapBuilder,
    guids: GuidHeapBuilder,
    user_strings: UserStringHeapBuilder,
    tables: TableStream,
    bodies: Vec<u8>,
    mscorlib: Option<u32>,
    /// `AssemblyRef` rows by assembly name, so each external type is referenced through the
    /// assembly that actually defines it (not just mscorlib).
    assembly_refs: BTreeMap<String, u32>,
    /// The defining assembly of an external type, by `namespace.name`, recorded before emission
    /// so [`ImageBuilder::type_ref`] scopes the `TypeRef` to the right `AssemblyRef`.
    type_assemblies: BTreeMap<String, String>,
    /// The identity (version + full public key) of a referenced assembly, by simple name, taken
    /// from the reference the unit was compiled against. An `AssemblyRef` we emit for that name
    /// carries this identity so an external consumer (csc + the ref pack) reconciles it with the
    /// same assembly instead of rejecting a `Version=0.0.0.0, PublicKeyToken=null` phantom (CS0012).
    assembly_identities: BTreeMap<String, (u16, u16, u16, u16, Vec<u8>)>,
    object: Option<Token>,
    object_ctor: Option<Token>,
    /// Per-method debug info, parallel to `MethodDef`: a placeholder is appended for
    /// every method, then filled in by [`ImageBuilder::set_sequence_points`].
    method_debug: Vec<MethodDebug>,
    /// The debug id shared by this image's PDB and its (eventual) debug directory.
    pdb_id: [u8; 20],
    /// The `#GUID` heap index of the module's MVID, so [`ImageBuilder::set_content_id`]
    /// can fill it once the content it is derived from is known.
    mvid: u32,
}

impl ImageBuilder {
    /// Starts a module: the `Module` row, the assembly manifest, and `<Module>`.
    #[must_use]
    pub fn new(module_name: &str, assembly_name: &str) -> ImageBuilder {
        let mut builder = ImageBuilder {
            strings: StringHeapBuilder::new(),
            blobs: BlobHeapBuilder::new(),
            guids: GuidHeapBuilder::new(),
            user_strings: UserStringHeapBuilder::new(),
            tables: TableStream::new(),
            bodies: Vec::new(),
            mscorlib: None,
            assembly_refs: BTreeMap::new(),
            type_assemblies: BTreeMap::new(),
            assembly_identities: BTreeMap::new(),
            object: None,
            object_ctor: None,
            method_debug: Vec::new(),
            pdb_id: derive_pdb_id(module_name),
            mvid: 0,
        };

        let mvid = builder.guids.add([0; 16]);
        builder.mvid = mvid;
        let module = builder.strings.intern(module_name);
        builder.tables.add_row(
            table::MODULE,
            alloc::vec![
                Column::U16(0),
                Column::StringRef(module),
                Column::GuidRef(mvid),
                Column::GuidRef(0),
                Column::GuidRef(0),
            ],
        );

        let assembly = builder.strings.intern(assembly_name);
        builder.tables.add_row(
            table::ASSEMBLY,
            alloc::vec![
                Column::U32(0),
                Column::U16(0),
                Column::U16(0),
                Column::U16(0),
                Column::U16(0),
                Column::U32(0),
                Column::BlobRef(0),
                Column::StringRef(assembly),
                Column::StringRef(0),
            ],
        );

        let module_type = builder.strings.intern("<Module>");
        builder.tables.add_row(
            table::TYPE_DEF,
            alloc::vec![
                Column::U32(0),
                Column::StringRef(module_type),
                Column::StringRef(0),
                Column::Coded(CodedIndex::TypeDefOrRef, Token::new(0, 0)),
                Column::Index(table::FIELD, 1),
                Column::Index(table::METHOD_DEF, 1),
            ],
        );

        builder
    }

    /// The `AssemblyRef` row for `mscorlib`, added on first use.
    fn mscorlib(&mut self) -> u32 {
        if let Some(row) = self.mscorlib {
            return row;
        }
        let name = self.strings.intern("mscorlib");
        let row = self.tables.add_row(
            table::ASSEMBLY_REF,
            alloc::vec![
                Column::U16(4),
                Column::U16(0),
                Column::U16(0),
                Column::U16(0),
                Column::U32(0),
                Column::BlobRef(0),
                Column::StringRef(name),
                Column::StringRef(0),
                Column::BlobRef(0),
            ],
        );
        self.mscorlib = Some(row);
        row
    }

    /// The `AssemblyRef` row for a named assembly (other than mscorlib), added on first use --
    /// the same identity shape as mscorlib (a versioned, tokenless reference the host resolves
    /// by name), so a non-CoreLib BCL type (System.Diagnostics.Trace, ...) names its assembly.
    fn assembly_ref(&mut self, name: &str) -> u32 {
        if let Some(row) = self.assembly_refs.get(name) {
            return *row;
        }
        let interned = self.strings.intern(name);
        let (major, minor, build, revision, flags, public_key) =
            match self.assembly_identities.get(name) {
                Some((major, minor, build, revision, key)) if !key.is_empty() => {
                    let blob = self.blobs.intern(key);
                    (*major, *minor, *build, *revision, 0x0000_0001, blob)
                }
                Some((major, minor, build, revision, _)) => {
                    (*major, *minor, *build, *revision, 0, 0)
                }
                None => (4, 0, 0, 0, 0, 0),
            };
        let row = self.tables.add_row(
            table::ASSEMBLY_REF,
            alloc::vec![
                Column::U16(major),
                Column::U16(minor),
                Column::U16(build),
                Column::U16(revision),
                Column::U32(flags),
                Column::BlobRef(public_key),
                Column::StringRef(interned),
                Column::StringRef(0),
                Column::BlobRef(0),
            ],
        );
        self.assembly_refs.insert(name.to_string(), row);
        row
    }

    /// Records that the external type `qualified_name` (`namespace.name`) is defined by
    /// `assembly`, so its `TypeRef` scopes there rather than defaulting to mscorlib.
    pub fn set_type_assembly(&mut self, qualified_name: &str, assembly: &str) {
        self.type_assemblies
            .insert(qualified_name.to_string(), assembly.to_string());
    }

    /// Sets this assembly's own version (the `Assembly` row, II.22.2), from an
    /// `[assembly: AssemblyVersion("a.b.c.d")]` attribute. csc consumes that attribute into this
    /// field rather than emitting a `CustomAttribute` (oracle-verified), and this mirrors it. Left
    /// unset when the source declares no version -- the row keeps its constructor default.
    pub fn set_assembly_version(&mut self, version: (u16, u16, u16, u16)) {
        let (major, minor, build, revision) = version;
        self.tables
            .set_cell(table::ASSEMBLY, 1, 1, Column::U16(major));
        self.tables
            .set_cell(table::ASSEMBLY, 1, 2, Column::U16(minor));
        self.tables
            .set_cell(table::ASSEMBLY, 1, 3, Column::U16(build));
        self.tables
            .set_cell(table::ASSEMBLY, 1, 4, Column::U16(revision));
    }

    /// Sets this assembly's hash-algorithm id (the `Assembly.HashAlgId` column, II.22.2), from an
    /// `[assembly: AssemblyAlgorithmId(n)]` attribute. csc consumes that attribute into this column
    /// rather than emitting a `CustomAttribute` (oracle-verified), and this mirrors it.
    pub fn set_assembly_hash_algorithm(&mut self, algorithm: u32) {
        self.tables
            .set_cell(table::ASSEMBLY, 1, 0, Column::U32(algorithm));
    }

    /// Sets this assembly's flags (the `Assembly.Flags` column, II.22.2 / II.23.1.2), from an
    /// `[assembly: AssemblyFlags(n)]` attribute. csc consumes that attribute into this column rather
    /// than emitting a `CustomAttribute` (oracle-verified), and this mirrors it.
    pub fn set_assembly_flags(&mut self, flags: u32) {
        self.tables
            .set_cell(table::ASSEMBLY, 1, 5, Column::U32(flags));
    }

    /// Sets this assembly's culture (the `Assembly.Culture` column, II.22.2), from an
    /// `[assembly: AssemblyCulture("name")]` attribute. csc consumes that attribute into this column
    /// rather than emitting a `CustomAttribute` (oracle-verified); the empty (neutral) culture
    /// interns to heap offset 0, so it stays a nil column exactly as csc leaves it.
    pub fn set_assembly_culture(&mut self, culture: &str) {
        let interned = self.strings.intern(culture);
        self.tables
            .set_cell(table::ASSEMBLY, 1, 8, Column::StringRef(interned));
    }

    /// Records a referenced assembly's identity (version + full public key, empty if unsigned) by
    /// simple name, from the reference the unit compiled against. An `AssemblyRef` emitted for that
    /// name then carries this identity instead of a `Version=#.0.0.0, PublicKeyToken=null` default,
    /// so an external consumer (csc alongside the same reference pack) reconciles it rather than
    /// demanding a phantom assembly (CS0012).
    pub fn set_assembly_identity(
        &mut self,
        name: &str,
        version: (u16, u16, u16, u16),
        public_key: &[u8],
    ) {
        let (major, minor, build, revision) = version;
        self.assembly_identities.insert(
            name.to_string(),
            (major, minor, build, revision, public_key.to_vec()),
        );
    }

    /// The `TypeRef` token for `System.Object`, added on first use.
    pub fn object_type(&mut self) -> Token {
        if let Some(token) = self.object {
            return token;
        }
        let scope = self.mscorlib();
        let namespace = self.strings.intern("System");
        let name = self.strings.intern("Object");
        let row = self.tables.add_row(
            table::TYPE_REF,
            alloc::vec![
                Column::Coded(
                    CodedIndex::ResolutionScope,
                    Token::new(table::ASSEMBLY_REF, scope)
                ),
                Column::StringRef(name),
                Column::StringRef(namespace),
            ],
        );
        let token = Token::new(table::TYPE_REF, row);
        self.object = Some(token);
        token
    }

    /// The `MemberRef` token for `System.Object`'s parameterless constructor, added
    /// on first use -- the base constructor every constructor chains to.
    pub fn object_ctor(&mut self) -> Token {
        if let Some(token) = self.object_ctor {
            return token;
        }
        let object = self.object_type();
        let name = self.strings.intern(".ctor");
        let signature = self
            .blobs
            .intern(&method_signature(true, &[], &TypeSig::Void));
        let row = self.tables.add_row(
            table::MEMBER_REF,
            alloc::vec![
                Column::Coded(CodedIndex::MemberRefParent, object),
                Column::StringRef(name),
                Column::BlobRef(signature),
            ],
        );
        let token = Token::new(table::MEMBER_REF, row);
        self.object_ctor = Some(token);
        token
    }

    /// Adds a `TypeDef`, returning its token. The field and method lists start at
    /// the next rows in those tables, so a type's members are added right after.
    pub fn add_type(&mut self, namespace: &str, name: &str, extends: Token, flags: u32) -> Token {
        let namespace = self.strings.intern(namespace);
        let name = self.strings.intern(name);
        let first_field = self.tables.row_count(table::FIELD) + 1;
        let first_method = self.tables.row_count(table::METHOD_DEF) + 1;
        let row = self.tables.add_row(
            table::TYPE_DEF,
            alloc::vec![
                Column::U32(flags),
                Column::StringRef(name),
                Column::StringRef(namespace),
                Column::Coded(CodedIndex::TypeDefOrRef, extends),
                Column::Index(table::FIELD, first_field),
                Column::Index(table::METHOD_DEF, first_method),
            ],
        );
        Token::new(table::TYPE_DEF, row)
    }

    /// Interns a UTF-16 string in the `#US` heap, returning its `ldstr` token (the
    /// `0x70` user-string tag plus the heap offset).
    pub fn user_string(&mut self, text: &[u16]) -> Token {
        Token::new(0x70, self.user_strings.intern(text))
    }

    /// A `MemberRef` to a method on `parent` (a `TypeRef`/`TypeDef` token), with the
    /// given name and signature blob -- for calling a method in another assembly.
    pub fn member_ref(&mut self, parent: Token, name: &str, signature: &[u8]) -> Token {
        let name = self.strings.intern(name);
        let signature = self.blobs.intern(signature);
        let row = self.tables.add_row(
            table::MEMBER_REF,
            alloc::vec![
                Column::Coded(CodedIndex::MemberRefParent, parent),
                Column::StringRef(name),
                Column::BlobRef(signature),
            ],
        );
        Token::new(table::MEMBER_REF, row)
    }

    /// Adds a `TypeSpec` row (II.22.39) holding `signature` (a type-signature blob,
    /// such as a multi-dimensional array type), returning its token -- the parent of
    /// the array's `.ctor`/`Get`/`Set` member references.
    pub fn type_spec(&mut self, signature: &[u8]) -> Token {
        let blob = self.blobs.intern(signature);
        let row = self
            .tables
            .add_row(table::TYPE_SPEC, alloc::vec![Column::BlobRef(blob)]);
        Token::new(table::TYPE_SPEC, row)
    }

    /// A `TypeRef` to `namespace.name` in `mscorlib`, for naming an external type.
    pub fn type_ref(&mut self, namespace: &str, name: &str) -> Token {
        let qualified = if namespace.is_empty() {
            name.to_string()
        } else {
            alloc::format!("{namespace}.{name}")
        };
        let scope = match self.type_assemblies.get(&qualified).cloned() {
            Some(assembly) if assembly != "mscorlib" => self.assembly_ref(&assembly),
            _ => self.mscorlib(),
        };
        let namespace = self.strings.intern(namespace);
        let name = self.strings.intern(name);
        let row = self.tables.add_row(
            table::TYPE_REF,
            alloc::vec![
                Column::Coded(
                    CodedIndex::ResolutionScope,
                    Token::new(table::ASSEMBLY_REF, scope)
                ),
                Column::StringRef(name),
                Column::StringRef(namespace),
            ],
        );
        Token::new(table::TYPE_REF, row)
    }

    /// Adds a `Field` row with the given name, signature blob, and flags, returning
    /// its token. Call right after [`add_type`] so the type's `FieldList` covers it.
    ///
    /// [`add_type`]: ImageBuilder::add_type
    pub fn add_field(&mut self, name: &str, signature: &[u8], flags: u16) -> Token {
        let name = self.strings.intern(name);
        let signature = self.blobs.intern(signature);
        let row = self.tables.add_row(
            table::FIELD,
            alloc::vec![
                Column::U16(flags),
                Column::StringRef(name),
                Column::BlobRef(signature),
            ],
        );
        Token::new(table::FIELD, row)
    }

    /// Adds a `Constant` row (II.22.9): the literal value attached to `parent` (a
    /// Field/Param/Property token). `element_type` is the value's element-type byte
    /// (II.23.1.16) and `value` its little-endian blob -- the form an enum member or
    /// a `const` field takes. The table is sorted by parent (a `HasConstant` coded
    /// index), so callers must add constants in increasing parent-row order; its
    /// sorted bit is set so a reader (the CLR) may binary-search it.
    pub fn add_constant(&mut self, parent: Token, element_type: u8, value: &[u8]) {
        let blob = self.blobs.intern(value);
        self.tables.mark_sorted(table::CONSTANT);
        self.tables.add_row(
            table::CONSTANT,
            alloc::vec![
                Column::U16(u16::from(element_type)),
                Column::Coded(CodedIndex::HasConstant, parent),
                Column::BlobRef(blob),
            ],
        );
    }

    /// The token of this module's single `Assembly` row (II.22.2, always row 1) -- the
    /// `HasCustomAttribute` parent an `[assembly: ...]` global attribute (24.2) attaches to.
    #[must_use]
    pub fn assembly_token(&self) -> Token {
        Token::new(table::ASSEMBLY, 1)
    }

    /// The token of this module's single `Module` row (II.22.30, always row 1) -- the
    /// `HasCustomAttribute` parent a `[module: ...]` global attribute (24.2) attaches to.
    #[must_use]
    pub fn module_token(&self) -> Token {
        Token::new(table::MODULE, 1)
    }

    /// Adds a `CustomAttribute` row (II.22.10): the `value` blob (an attribute-argument
    /// blob) attached to `parent` (a `HasCustomAttribute` token) and identified by
    /// `constructor` (its `.ctor`, a MethodDef/MemberRef -- the `CustomAttributeType`).
    ///
    /// Rows may be added in any order; `finish` sorts the table by parent (a reader, e.g.
    /// the CLR, binary-searches it by `HasCustomAttribute`). Used both for user attributes
    /// and synthesized markers such as the AOT base-chain vector (`<ExceptionBaseChain>`).
    pub fn add_custom_attribute(&mut self, parent: Token, constructor: Token, value: &[u8]) {
        let blob = self.blobs.intern(value);
        self.tables.add_row(
            table::CUSTOM_ATTRIBUTE,
            alloc::vec![
                Column::Coded(CodedIndex::HasCustomAttribute, parent),
                Column::Coded(CodedIndex::CustomAttributeType, constructor),
                Column::BlobRef(blob),
            ],
        );
    }

    /// Adds a `Property` row (flags, name, the property-signature blob), returning
    /// its token. Add a type's properties right after its accessor methods.
    pub fn add_property(&mut self, name: &str, signature: &[u8], flags: u16) -> Token {
        let name = self.strings.intern(name);
        let signature = self.blobs.intern(signature);
        let row = self.tables.add_row(
            table::PROPERTY,
            alloc::vec![
                Column::U16(flags),
                Column::StringRef(name),
                Column::BlobRef(signature),
            ],
        );
        Token::new(table::PROPERTY, row)
    }

    /// Maps a type to its first `Property` row (II.22.35), so the type's property
    /// range is known. Call once per type that declares a property.
    pub fn add_property_map(&mut self, type_token: Token, first_property: Token) {
        self.tables.add_row(
            table::PROPERTY_MAP,
            alloc::vec![
                Column::Index(table::TYPE_DEF, type_token.row()),
                Column::Index(table::PROPERTY, first_property.row()),
            ],
        );
    }

    /// Adds an `Event` row (II.22.13): flags, name, and the event's delegate type as a
    /// `TypeDefOrRef`. Add a type's events right after its accessor methods. Returns its
    /// token, which is also the `HasSemantics` parent of its add/remove accessors.
    pub fn add_event(&mut self, name: &str, event_type: Token) -> Token {
        let name = self.strings.intern(name);
        let row = self.tables.add_row(
            table::EVENT,
            alloc::vec![
                Column::U16(0),
                Column::StringRef(name),
                Column::Coded(CodedIndex::TypeDefOrRef, event_type),
            ],
        );
        Token::new(table::EVENT, row)
    }

    /// Maps a type to its first `Event` row (II.22.12), so the type's event range is known.
    /// Call once per type that declares an event.
    pub fn add_event_map(&mut self, type_token: Token, first_event: Token) {
        self.tables.add_row(
            table::EVENT_MAP,
            alloc::vec![
                Column::Index(table::TYPE_DEF, type_token.row()),
                Column::Index(table::EVENT, first_event.row()),
            ],
        );
    }

    /// Links an accessor method to its property or event via a `MethodSemantics` row
    /// (II.22.28). `semantics` is `0x1` setter, `0x2` getter, `0x8` addon, `0x10` removeon.
    pub fn add_method_semantics(&mut self, semantics: u16, method: Token, property: Token) {
        self.tables.add_row(
            table::METHOD_SEMANTICS,
            alloc::vec![
                Column::U16(semantics),
                Column::Index(table::METHOD_DEF, method.row()),
                Column::Coded(CodedIndex::HasSemantics, property),
            ],
        );
    }

    /// Adds a `StandAloneSig` row holding `signature` (a local-variable signature
    /// blob), returning its token for a method body's `local_var_sig`.
    pub fn add_standalone_sig(&mut self, signature: &[u8]) -> Token {
        let blob = self.blobs.intern(signature);
        let row = self
            .tables
            .add_row(table::STAND_ALONE_SIG, alloc::vec![Column::BlobRef(blob)]);
        Token::new(table::STAND_ALONE_SIG, row)
    }

    /// Adds a `MethodDef` whose body bytes (a CIL method body) go into `.text`,
    /// returning its token. `signature` is the encoded method signature blob.
    pub fn add_method(
        &mut self,
        name: &str,
        signature: &[u8],
        body: &[u8],
        flags: u16,
        impl_flags: u16,
        parameters: &[Box<str>],
    ) -> Token {
        align4(&mut self.bodies);
        let rva = TEXT_RVA + CLI_HEADER_SIZE + self.bodies.len() as u32;
        self.bodies.extend_from_slice(body);

        let name = self.strings.intern(name);
        let signature = self.blobs.intern(signature);
        let first_param = self.tables.row_count(table::PARAM) + 1;
        let row = self.tables.add_row(
            table::METHOD_DEF,
            alloc::vec![
                Column::U32(rva),
                Column::U16(impl_flags),
                Column::U16(flags),
                Column::StringRef(name),
                Column::BlobRef(signature),
                Column::Index(table::PARAM, first_param),
            ],
        );
        self.add_param_rows(parameters);
        self.method_debug.push(MethodDebug {
            sequence_points: Vec::new(),
            local_signature: 0,
            locals: Vec::new(),
            scope_length: 0,
            document: 0,
        });
        Token::new(table::METHOD_DEF, row)
    }

    /// Adds a `Param` row (II.22.33) per parameter -- `Flags=0`, `Sequence` 1..N, `Name`
    /// -- so a debugger/PDB consumer can show argument names instead of `argN`. The rows
    /// follow the just-added `MethodDef` whose `ParamList` points at the first of them.
    fn add_param_rows(&mut self, parameters: &[Box<str>]) {
        for (index, parameter) in parameters.iter().enumerate() {
            let name = self.strings.intern(parameter);
            self.tables.add_row(
                table::PARAM,
                alloc::vec![
                    Column::U16(0),
                    Column::U16((index + 1) as u16),
                    Column::StringRef(name),
                ],
            );
        }
    }

    /// Adds a `Param` row (II.22.33) for a method's return value: `Flags` 0, `Sequence` 0,
    /// no name. Call immediately after the owning method's [`add_method`] and before any
    /// later method, so the row falls inside this method's `ParamList` run; it is then the
    /// `HasCustomAttribute` parent a `[return:]` attribute (ECMA-334 24) is attached to.
    /// Returns its `Param` token.
    ///
    /// [`add_method`]: ImageBuilder::add_method
    pub fn add_return_param(&mut self) -> Token {
        let row = self.tables.add_row(
            table::PARAM,
            alloc::vec![Column::U16(0), Column::U16(0), Column::StringRef(0)],
        );
        Token::new(table::PARAM, row)
    }

    /// Adds an abstract `MethodDef` (RVA 0, no body, IL impl) -- an interface method or
    /// an abstract class method. `flags` carries Abstract | Virtual.
    pub fn add_abstract_method(&mut self, name: &str, signature: &[u8], flags: u16) -> Token {
        self.add_bodyless_method(name, signature, flags, 0)
    }

    /// Adds a runtime-implemented `MethodDef` (RVA 0, no body, `Runtime` impl) -- a
    /// delegate's `.ctor` or `Invoke`, whose body the runtime supplies.
    pub fn add_runtime_method(&mut self, name: &str, signature: &[u8], flags: u16) -> Token {
        self.add_bodyless_method(name, signature, flags, 0x0003)
    }

    /// A `MethodDef` with no IL body (RVA 0); `impl_flags` is 0 (IL/abstract) or
    /// `0x0003` (`Runtime`).
    fn add_bodyless_method(
        &mut self,
        name: &str,
        signature: &[u8],
        flags: u16,
        impl_flags: u16,
    ) -> Token {
        let name = self.strings.intern(name);
        let signature = self.blobs.intern(signature);
        let first_param = self.tables.row_count(table::PARAM) + 1;
        let row = self.tables.add_row(
            table::METHOD_DEF,
            alloc::vec![
                Column::U32(0),
                Column::U16(impl_flags),
                Column::U16(flags),
                Column::StringRef(name),
                Column::BlobRef(signature),
                Column::Index(table::PARAM, first_param),
            ],
        );
        self.method_debug.push(MethodDebug {
            sequence_points: Vec::new(),
            local_signature: 0,
            locals: Vec::new(),
            scope_length: 0,
            document: 0,
        });
        Token::new(table::METHOD_DEF, row)
    }

    /// Adds a P/Invoke `MethodDef` (RVA 0, no body) -- `flags` carry `PinvokeImpl` (II.23.1.10), and
    /// the matching [`ImplMap`](Self::add_impl_map) names the native entry point. II.15.5. The
    /// MethodImplAttributes set `PreserveSig` (0x0080), the C# `[DllImport]` default: the native
    /// return is used as-is. Without it the CLR treats the return as an HRESULT and shuffles it
    /// (so e.g. an `int` result is lost).
    pub fn add_pinvoke_method(&mut self, name: &str, signature: &[u8], flags: u16) -> Token {
        self.add_bodyless_method(name, signature, flags, 0x0080)
    }

    /// Adds a `ModuleRef` row (II.22.31) naming an unmanaged module (a DLL), returning its token --
    /// the `ImportScope` a P/Invoke's `ImplMap` points at. Not deduplicated; one per `[DllImport]`.
    pub fn add_module_ref(&mut self, name: &str) -> Token {
        let name = self.strings.intern(name);
        let row = self.tables.add_row(table::MODULE_REF, alloc::vec![Column::StringRef(name)]);
        Token::new(table::MODULE_REF, row)
    }

    /// Adds an `ImplMap` row (II.22.22): the P/Invoke mapping for `method` (a MethodDef). `flags` is
    /// the `MappingFlags` (II.23.1.8: char set / calling convention / SetLastError), `import_name`
    /// the native entry-point name, `scope` the `ModuleRef` of the DLL. The table is sorted by
    /// `MemberForwarded`, so callers add rows in increasing method-row order.
    pub fn add_impl_map(&mut self, method: Token, flags: u16, import_name: &str, scope: Token) {
        let import_name = self.strings.intern(import_name);
        self.tables.mark_sorted(table::IMPL_MAP);
        self.tables.add_row(
            table::IMPL_MAP,
            alloc::vec![
                Column::U16(flags),
                Column::Coded(CodedIndex::MemberForwarded, method),
                Column::StringRef(import_name),
                Column::Index(table::MODULE_REF, scope.row()),
            ],
        );
    }

    /// Records that `class` (a `TypeDef`) implements `interface` (a `TypeDef`/`TypeRef`)
    /// via an `InterfaceImpl` row (II.22.23).
    pub fn add_interface_impl(&mut self, class: Token, interface: Token) {
        self.tables.add_row(
            table::INTERFACE_IMPL,
            alloc::vec![
                Column::Index(table::TYPE_DEF, class.row()),
                Column::Coded(CodedIndex::TypeDefOrRef, interface),
            ],
        );
    }

    /// Records that `class` (a `TypeDef`) provides `body` (one of its own `MethodDef`s)
    /// as the implementation of the interface method `declaration` (a `MethodDef` for a
    /// this-module interface, else a `MemberRef`), via a `MethodImpl` row (II.22.27).
    /// This is how an explicit interface member implementation (`int I.M() {...}`) is
    /// wired: `body` is private, so the override is reached only through `declaration`.
    pub fn add_method_impl(&mut self, class: Token, body: Token, declaration: Token) {
        self.tables.add_row(
            table::METHOD_IMPL,
            alloc::vec![
                Column::Index(table::TYPE_DEF, class.row()),
                Column::Coded(CodedIndex::MethodDefOrRef, body),
                Column::Coded(CodedIndex::MethodDefOrRef, declaration),
            ],
        );
    }

    /// Records that `nested` is a type nested in `enclosing` (II.22.32), via a
    /// `NestedClass` row. The nested type's own `TypeDef` carries an empty namespace.
    pub fn add_nested_class(&mut self, nested: Token, enclosing: Token) {
        self.tables.add_row(
            table::NESTED_CLASS,
            alloc::vec![
                Column::Index(table::TYPE_DEF, nested.row()),
                Column::Index(table::TYPE_DEF, enclosing.row()),
            ],
        );
    }

    /// Records a method's debug info (sequence points, local names) for the PDB.
    pub fn set_method_debug(&mut self, method: Token, debug: MethodDebug) {
        let index = method.row() as usize - 1;
        self.method_debug[index] = debug;
    }

    /// The 20-byte debug id, so the PE debug directory can point at the matching PDB.
    #[must_use]
    pub fn pdb_id(&self) -> [u8; 20] {
        self.pdb_id
    }

    /// Re-derives the module MVID and the 20-byte debug id (the `#Pdb` id + the PE
    /// CodeView GUID, which stay equal) from a SHA-256 of `content`, so each build is
    /// uniquely identifiable instead of keyed on the module name (the FNV fallback) with a
    /// zero MVID -- a debugger then never binds a stale PDB to a rebuilt binary. The MVID
    /// and the debug GUID take distinct halves of the 32-byte digest. Call before `finish`.
    pub fn set_content_id(&mut self, content: &[u8]) {
        let digest = crate::sha256::sha256(content);
        let (mvid_bytes, id_bytes) = digest.split_at(16);
        self.guids.set(
            self.mvid,
            guid_from(mvid_bytes.try_into().expect("16 bytes")),
        );
        self.pdb_id[..16].copy_from_slice(&guid_from(id_bytes.try_into().expect("16 bytes")));
        self.pdb_id[16..].copy_from_slice(&1u32.to_le_bytes());
    }

    /// Builds the standalone Portable PDB for this image's methods, attributing them
    /// to their source files in `documents` (each method's `document` indexes this
    /// list; each document carries its source hash) and recording `entry_point` (0 for
    /// a library).
    #[must_use]
    pub fn build_pdb(&self, documents: &[DebugDocument], entry_point: Token) -> Vec<u8> {
        build_portable_pdb(documents, &self.method_debug, entry_point, self.pdb_id)
    }

    /// Serializes the module to a PE image, naming `entry_point` (a `MethodDef`
    /// token, or the nil token for a library).
    #[must_use]
    pub fn finish(self, entry_point: Token, is_dll: bool) -> Vec<u8> {
        self.finish_inner(entry_point, is_dll, Vec::new())
    }

    /// Like [`ImageBuilder::finish`], but also emits a debug directory whose
    /// CodeView record points a debugger at `pdb_name` (with this image's id).
    #[must_use]
    pub fn finish_with_debug(self, entry_point: Token, is_dll: bool, pdb_name: &str) -> Vec<u8> {
        let codeview = codeview_record(self.pdb_id, pdb_name);
        self.finish_inner(entry_point, is_dll, alloc::vec![(2u32, codeview)])
    }

    /// Like [`ImageBuilder::finish_with_debug`], but EMBEDS the Portable PDB in the image itself
    /// (no separate `.pdb`): the debug directory carries the CodeView record (type 2, keyed by
    /// `pdb_name` so a debugger still matches the id), the DEFLATE-compressed PDB (type 17,
    /// `EmbeddedPortablePdb`), and the PDB checksum (type 19). Builds the PDB from `documents`.
    #[must_use]
    pub fn finish_with_embedded_debug(
        self,
        entry_point: Token,
        is_dll: bool,
        documents: &[DebugDocument],
        pdb_name: &str,
    ) -> Vec<u8> {
        let pdb = self.build_pdb(documents, entry_point);
        let entries = alloc::vec![
            (2u32, codeview_record(self.pdb_id, pdb_name)),
            (17u32, embedded_pdb_record(&pdb)),
            (19u32, pdb_checksum_record(&pdb)),
        ];
        self.finish_inner(entry_point, is_dll, entries)
    }

    fn finish_inner(
        mut self,
        entry_point: Token,
        is_dll: bool,
        debug_entries: Vec<(u32, Vec<u8>)>,
    ) -> Vec<u8> {
        align4(&mut self.bodies);
        self.tables.sort_by_coded_parent(table::CUSTOM_ATTRIBUTE);
        self.tables.sort_by_coded_column(table::METHOD_SEMANTICS, 2);
        let tables = self.tables.serialize(HeapSizes::default());
        let strings = self.strings.into_bytes();
        let guids = self.guids.into_bytes();
        let blobs = self.blobs.into_bytes();
        let user_strings = self.user_strings.into_bytes();
        let user_strings = (user_strings.len() > 1).then_some(user_strings.as_slice());

        let metadata = metadata_root(
            RUNTIME_VERSION,
            &tables,
            &strings,
            user_strings,
            &guids,
            &blobs,
        );

        let metadata_rva = TEXT_RVA + CLI_HEADER_SIZE + self.bodies.len() as u32;
        let cli = cli_header(
            metadata_rva,
            metadata.len() as u32,
            COMIMAGE_FLAGS_ILONLY,
            entry_point.0,
        );

        let mut text = Vec::with_capacity(cli.len() + self.bodies.len() + metadata.len());
        text.extend_from_slice(&cli);
        text.extend_from_slice(&self.bodies);
        text.extend_from_slice(&metadata);
        let borrowed: Vec<(u32, &[u8])> =
            debug_entries.iter().map(|(kind, data)| (*kind, data.as_slice())).collect();
        write_image_with_debug(&text, is_dll, &borrowed)
    }
}

/// The `EmbeddedPortablePdb` debug record (PE-COFF type 17): the `"MPDB"` magic, the uncompressed
/// PDB size, then the PDB compressed with (stored-block) DEFLATE. A reader inflates the tail back
/// to the standalone PDB the debugger would otherwise load from disk.
fn embedded_pdb_record(pdb: &[u8]) -> Vec<u8> {
    let mut record = Vec::with_capacity(8 + pdb.len());
    record.extend_from_slice(b"MPDB");
    record.extend_from_slice(&(pdb.len() as u32).to_le_bytes());
    record.extend_from_slice(&crate::deflate::deflate_store(pdb));
    record
}

/// The `PdbChecksum` debug record (PE-COFF type 19): the zero-terminated algorithm name, then the
/// digest -- so a loader can verify an embedded (or on-disk) PDB matches the image.
fn pdb_checksum_record(pdb: &[u8]) -> Vec<u8> {
    let mut record = Vec::with_capacity(7 + 32);
    record.extend_from_slice(b"SHA256\0");
    record.extend_from_slice(&crate::sha256::sha256(pdb));
    record
}

/// The CodeView `RSDS` record a debug directory points at: the signature, the
/// 20-byte id (GUID + age), and the PDB file name a debugger should load.
fn codeview_record(pdb_id: [u8; 20], pdb_name: &str) -> Vec<u8> {
    let mut record = Vec::with_capacity(4 + 20 + pdb_name.len() + 1);
    record.extend_from_slice(b"RSDS");
    record.extend_from_slice(&pdb_id);
    record.extend_from_slice(pdb_name.as_bytes());
    record.push(0);
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLIC_CLASS: u32 = 0x0000_0001;
    const PUBLIC_STATIC: u16 = 0x0006 | 0x0010;
    const IL_MANAGED: u16 = 0x0000;

    #[test]
    fn embedded_debug_carries_codeview_the_deflated_pdb_and_checksum() {
        let mut builder = ImageBuilder::new("test.dll", "test");
        let object = builder.object_type();
        builder.add_type("App", "Program", object, PUBLIC_CLASS);
        let entry = builder.add_method(
            "Main",
            &[0x00, 0x00, 0x01],
            &[0x06, 0x2A],
            PUBLIC_STATIC,
            IL_MANAGED,
            &[],
        );

        let expected_pdb = builder.build_pdb(&[], entry);
        let image = builder.finish_with_embedded_debug(entry, false, &[], "test.pdb");

        let entries = crate::pe::read_debug_directory(&image);
        assert_eq!(entries.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(), [2, 17, 19]);

        let embedded = &entries[1].1;
        assert_eq!(&embedded[..4], b"MPDB");
        assert_eq!(u32::from_le_bytes(embedded[4..8].try_into().unwrap()) as usize, expected_pdb.len());
        assert_eq!(&embedded[8..], crate::deflate::deflate_store(&expected_pdb).as_slice());

        let checksum = &entries[2].1;
        assert_eq!(&checksum[..7], b"SHA256\0");
        assert_eq!(&checksum[7..], &crate::sha256::sha256(&expected_pdb)[..]);
    }

    #[test]
    fn assembles_a_module_with_a_method_that_round_trips() {
        let mut builder = ImageBuilder::new("test.dll", "test");
        let object = builder.object_type();
        builder.add_type("App", "Program", object, PUBLIC_CLASS);

        let body = [0x06u8, 0x2A];
        let signature = [0x00u8, 0x00, 0x01];
        let entry = builder.add_method("Main", &signature, &body, PUBLIC_STATIC, IL_MANAGED, &[]);
        assert_eq!(entry.table(), table::METHOD_DEF);

        let pe = builder.finish(entry, false);

        let image = lamella_metadata::pe::PeImage::parse(&pe).expect("valid PE");
        assert_eq!(image.cli_header_rva(), TEXT_RVA);
        assert!(lamella_metadata::image::MetadataImage::read(&pe).is_ok());
    }

    #[test]
    fn content_id_is_deterministic_content_sensitive_and_well_formed() {
        let id = |content: &[u8]| {
            let mut builder = ImageBuilder::new("m", "a");
            builder.set_content_id(content);
            builder.pdb_id()
        };
        assert_eq!(id(b"alpha"), id(b"alpha"));
        assert_ne!(id(b"alpha"), id(b"beta"));
        assert_ne!(id(b"alpha"), ImageBuilder::new("m", "a").pdb_id());
        let guid = id(b"alpha");
        assert_eq!(guid[7] & 0xf0, 0x40, "version 4");
        assert_eq!(guid[8] & 0xc0, 0x80, "variant 10xx");
    }

    #[test]
    fn build_pdb_carries_the_methods_and_shared_id() {
        let mut builder = ImageBuilder::new("test.dll", "test");
        let object = builder.object_type();
        builder.add_type("App", "Program", object, PUBLIC_CLASS);
        let body = [0x06u8, 0x2A];
        let signature = [0x00u8, 0x00, 0x01];
        let main = builder.add_method("Main", &signature, &body, PUBLIC_STATIC, IL_MANAGED, &[]);
        builder.add_method("Other", &signature, &body, PUBLIC_STATIC, IL_MANAGED, &[]);
        builder.set_method_debug(
            main,
            crate::pdb::MethodDebug {
                sequence_points: alloc::vec![crate::pdb::SequencePoint {
                    il_offset: 0,
                    start_line: 1,
                    start_column: 1,
                    end_line: 1,
                    end_column: 2,
                    is_hidden: false,
                }],
                local_signature: 0,
                locals: Vec::new(),
                scope_length: 0,
                document: 1,
            },
        );

        let pdb = builder.build_pdb(
            &[crate::pdb::DebugDocument { path: "App.cs", source: "" }],
            main,
        );
        assert_eq!(&pdb[0..4], b"BSJB");
        assert!(pdb.windows(20).any(|window| window == builder.pdb_id()));
    }

    #[test]
    fn exception_base_chain_attribute_round_trips() {
        let mut builder = ImageBuilder::new("test.dll", "test");
        let marker = builder.type_ref("", "<ExceptionBaseChain>");
        let ctor_sig = method_signature(true, &[], &TypeSig::Void);
        let marker_ctor = builder.member_ref(marker, ".ctor", &ctor_sig);
        let exception = builder.type_ref("System", "DivideByZeroException");
        let chain = [0xAAAA_AAAAu32, 0xBBBB_BBBB, 0xCCCC_CCCC];
        builder.add_custom_attribute(
            exception,
            marker_ctor,
            &lamella_metadata::encode_exception_base_chain(&chain),
        );
        let pe = builder.finish(Token::new(0, 0), true);

        let assembly = lamella_metadata::Assembly::read(&pe).expect("valid assembly");
        assert_eq!(
            assembly.exception_base_chain(exception),
            Some(chain.to_vec())
        );
        assert_eq!(assembly.exception_base_chain(marker), None);
    }

    #[test]
    fn assembly_ref_carries_the_reference_identity() {
        let mut builder = ImageBuilder::new("test.dll", "test");
        let key: [u8; 8] = [0x00, 0x24, 0x00, 0x00, 0x04, 0x80, 0x00, 0x00];
        builder.set_assembly_identity("System.Runtime", (8, 0, 0, 0), &key);
        builder.set_type_assembly("System.EventArgs", "System.Runtime");
        let _identified = builder.type_ref("System", "EventArgs");
        let _defaulted = builder.type_ref("System.Diagnostics", "Trace");
        let pe = builder.finish(Token::new(0, 0), true);

        let assembly = lamella_metadata::Assembly::read(&pe).expect("valid assembly");
        let runtime = assembly
            .assembly_refs()
            .find(|r| r.name() == Some("System.Runtime"))
            .expect("System.Runtime AssemblyRef present");
        assert_eq!(runtime.version(), (8, 0, 0, 0));
        assert_eq!(runtime.flags() & 0x0000_0001, 0x0000_0001, "afPublicKey set");
        assert_eq!(runtime.public_key_or_token(), &key);
    }

    #[test]
    fn set_assembly_version_reaches_the_assembly_row() {
        let mut builder = ImageBuilder::new("test.dll", "test");
        builder.set_assembly_version((1, 0, 0, 0));
        let pe = builder.finish(Token::new(0, 0), true);

        let assembly = lamella_metadata::Assembly::read(&pe).expect("valid assembly");
        assert_eq!(assembly.assembly_version(), (1, 0, 0, 0));
    }

    #[test]
    fn assembly_row_defaults_to_zero_version() {
        let builder = ImageBuilder::new("test.dll", "test");
        let pe = builder.finish(Token::new(0, 0), true);

        let assembly = lamella_metadata::Assembly::read(&pe).expect("valid assembly");
        assert_eq!(assembly.assembly_version(), (0, 0, 0, 0));
    }
}
