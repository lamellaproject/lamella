#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The in-memory metadata model and managed-PE reader.

extern crate alloc;

pub mod bytes;
pub mod coded;
pub mod constant;
pub mod flags;
pub mod heaps;
pub mod image;
pub mod layout;
pub mod pdb;
pub mod pe;
pub mod reader;
pub mod rows;
pub mod signature;
pub mod tables;

pub use bytes::{ReadError, Reader};
pub use coded::CodedIndex;
pub use constant::{ConstantValue, decode_constant};
pub use heaps::{
    BlobHeap, GuidHeap, HeapError, StringsHeap, UserStringsHeap, read_compressed_i32,
    read_compressed_u32,
};
pub use image::{MetadataError, MetadataImage};
pub use layout::{LayoutError, TargetLayout, TypeLayout, layout_value_type};
pub use pdb::{LocalVariable, PortablePdb, SequencePoint};
pub use pe::{PeError, PeImage};
pub use reader::{
    Assembly, AssemblyRef, AttrArg, AttrNamed, CharSet, CustomAttribute, DecodedAttribute, Event,
    ExceptionClause, ExceptionHandlerKind, Field, MemberRef, Method, MethodKind, Param, Property,
    ResolvedMethod, TypeDef, TypeName, TypeRef, decode_custom_attribute,
    encode_exception_base_chain, exception_tag_for_name, fnv1a32,
};
pub use rows::{Col, Row, Tables, columns};
pub use signature::{MethodSig, SigError, SigType, parse_field, parse_method, parse_type};
pub use tables::{TableError, TablesHeader};
