//! Assembling a managed module: the orchestration over heaps, tables, and the PE.

use crate::heap::{BlobHeapBuilder, GuidHeapBuilder, StringHeapBuilder, UserStringHeapBuilder};
use crate::pdb::{MethodDebug, build_portable_pdb};
use crate::pe::{
    CLI_HEADER_SIZE, COMIMAGE_FLAGS_ILONLY, TEXT_RVA, cli_header, write_image_with_debug,
};
use crate::root::metadata_root;
use crate::signature::{TypeSig, method_signature};
use crate::tables::{Column, HeapSizes, TableStream};
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

/// Assembles a single managed module into a PE image.
pub struct ImageBuilder {
    strings: StringHeapBuilder,
    blobs: BlobHeapBuilder,
    guids: GuidHeapBuilder,
    user_strings: UserStringHeapBuilder,
    tables: TableStream,
    bodies: Vec<u8>,
    mscorlib: Option<u32>,
    object: Option<Token>,
    object_ctor: Option<Token>,
    /// Per-method debug info, parallel to `MethodDef`: a placeholder is appended for
    /// every method, then filled in by [`ImageBuilder::set_sequence_points`].
    method_debug: Vec<MethodDebug>,
    /// The debug id shared by this image's PDB and its (eventual) debug directory.
    pdb_id: [u8; 20],
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
            object: None,
            object_ctor: None,
            method_debug: Vec::new(),
            pdb_id: derive_pdb_id(module_name),
        };

        let mvid = builder.guids.add([0; 16]);
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
                Column::U16(4),
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

    /// A `TypeRef` to `namespace.name` in `mscorlib`, for naming an external type.
    pub fn type_ref(&mut self, namespace: &str, name: &str) -> Token {
        let scope = self.mscorlib();
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

    /// Links an accessor method to its property via a `MethodSemantics` row
    /// (II.22.28). `semantics` is `0x1` for a setter, `0x2` for a getter.
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
        self.method_debug.push(MethodDebug {
            sequence_points: Vec::new(),
            local_signature: 0,
            locals: Vec::new(),
            scope_length: 0,
        });
        Token::new(table::METHOD_DEF, row)
    }

    /// Adds an abstract `MethodDef` (RVA 0, no body) -- an interface method or an
    /// abstract class method. `flags` carries Abstract | Virtual.
    pub fn add_abstract_method(&mut self, name: &str, signature: &[u8], flags: u16) -> Token {
        let name = self.strings.intern(name);
        let signature = self.blobs.intern(signature);
        let first_param = self.tables.row_count(table::PARAM) + 1;
        let row = self.tables.add_row(
            table::METHOD_DEF,
            alloc::vec![
                Column::U32(0),
                Column::U16(0),
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
        });
        Token::new(table::METHOD_DEF, row)
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

    /// Builds the standalone Portable PDB for this image's methods, attributing them
    /// to `document_path` and recording `entry_point` (0 for a library).
    #[must_use]
    pub fn build_pdb(&self, document_path: &str, entry_point: Token) -> Vec<u8> {
        build_portable_pdb(document_path, &self.method_debug, entry_point, self.pdb_id)
    }

    /// Serializes the module to a PE image, naming `entry_point` (a `MethodDef`
    /// token, or the nil token for a library).
    #[must_use]
    pub fn finish(self, entry_point: Token, is_dll: bool) -> Vec<u8> {
        self.finish_inner(entry_point, is_dll, None)
    }

    /// Like [`ImageBuilder::finish`], but also emits a debug directory whose
    /// CodeView record points a debugger at `pdb_name` (with this image's id).
    #[must_use]
    pub fn finish_with_debug(self, entry_point: Token, is_dll: bool, pdb_name: &str) -> Vec<u8> {
        let codeview = codeview_record(self.pdb_id, pdb_name);
        self.finish_inner(entry_point, is_dll, Some(codeview))
    }

    fn finish_inner(
        mut self,
        entry_point: Token,
        is_dll: bool,
        codeview: Option<Vec<u8>>,
    ) -> Vec<u8> {
        align4(&mut self.bodies);
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
        write_image_with_debug(&text, is_dll, codeview.as_deref())
    }
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
    fn assembles_a_module_with_a_method_that_round_trips() {
        let mut builder = ImageBuilder::new("test.dll", "test");
        let object = builder.object_type();
        builder.add_type("App", "Program", object, PUBLIC_CLASS);

        let body = [0x06u8, 0x2A];
        let signature = [0x00u8, 0x00, 0x01];
        let entry = builder.add_method("Main", &signature, &body, PUBLIC_STATIC, IL_MANAGED);
        assert_eq!(entry.table(), table::METHOD_DEF);

        let pe = builder.finish(entry, false);

        let image = lamella_metadata::pe::PeImage::parse(&pe).expect("valid PE");
        assert_eq!(image.cli_header_rva(), TEXT_RVA);
        assert!(lamella_metadata::image::MetadataImage::read(&pe).is_ok());
    }

    #[test]
    fn build_pdb_carries_the_methods_and_shared_id() {
        let mut builder = ImageBuilder::new("test.dll", "test");
        let object = builder.object_type();
        builder.add_type("App", "Program", object, PUBLIC_CLASS);
        let body = [0x06u8, 0x2A];
        let signature = [0x00u8, 0x00, 0x01];
        let main = builder.add_method("Main", &signature, &body, PUBLIC_STATIC, IL_MANAGED);
        builder.add_method("Other", &signature, &body, PUBLIC_STATIC, IL_MANAGED);
        builder.set_method_debug(
            main,
            crate::pdb::MethodDebug {
                sequence_points: alloc::vec![crate::pdb::SequencePoint {
                    il_offset: 0,
                    start_line: 1,
                    start_column: 1,
                    end_line: 1,
                    end_column: 2,
                }],
                local_signature: 0,
                locals: Vec::new(),
                scope_length: 0,
            },
        );

        let pdb = builder.build_pdb("App.cs", main);
        assert_eq!(&pdb[0..4], b"BSJB");
        assert!(pdb.windows(20).any(|window| window == builder.pdb_id()));
    }
}
