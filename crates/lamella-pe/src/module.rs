//! Assembling a managed module: the orchestration over heaps, tables, and the PE.

use crate::heap::{BlobHeapBuilder, GuidHeapBuilder, StringHeapBuilder, UserStringHeapBuilder};
use crate::pe::{CLI_HEADER_SIZE, COMIMAGE_FLAGS_ILONLY, TEXT_RVA, cli_header, write_image};
use crate::root::metadata_root;
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
        Token::new(table::METHOD_DEF, row)
    }

    /// Serializes the module to a PE image, naming `entry_point` (a `MethodDef`
    /// token, or the nil token for a library).
    #[must_use]
    pub fn finish(mut self, entry_point: Token, is_dll: bool) -> Vec<u8> {
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
        write_image(&text, is_dll)
    }
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
}
