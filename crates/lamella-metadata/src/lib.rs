#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The in-memory metadata model and managed-PE reader.

extern crate alloc;

pub mod bytes;
pub mod coded;
pub mod constant;
pub mod demands;
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
pub use signature::{
    LocalVar, MethodSig, SigError, SigType, parse_field, parse_local_vars, parse_method,
    parse_method_spec, parse_type,
};
pub use tables::{TableError, TablesHeader};

/// The inherited vtable slot that a virtual method declared without `newslot` overrides, as an index
/// into `inherited`: the most-derived slot whose signature matches, or `None` when none matches and
/// the method takes a slot of its own.
///
/// A vtable laid out base-first appends each type's new slots after its base's, so a later matching
/// slot belongs to a more-derived declaration. When one type hides a virtual method with `newslot`
/// and a type below it overrides that method, the override replaces the hiding method's slot; the
/// hidden method's slot keeps its own body, and a call made through the older base still reaches it.
pub fn overridden_slot<S>(inherited: &[S], same_signature: impl FnMut(&S) -> bool) -> Option<usize> {
    inherited.iter().rposition(same_signature)
}
