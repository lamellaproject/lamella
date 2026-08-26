#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Managed PE and ECMA-335 metadata writer (1st edition, Partition II).

extern crate alloc;

mod deflate;
pub mod heap;
pub mod module;
pub mod pdb;
pub mod pe;
pub mod root;
pub mod sha256;
pub mod signature;
pub mod tables;

pub use heap::{
    BlobHeapBuilder, GuidHeapBuilder, StringHeapBuilder, UserStringHeapBuilder, compress_i32,
    compress_u32,
};
pub use module::{ImageBuilder, PARAM_HAS_DEFAULT, PARAM_OPTIONAL, PARAM_OUT, ParamRow};
pub use pdb::{
    DebugDocument, LocalVariable, MethodDebug, SequencePoint, build_portable_pdb,
    sequence_points_blob,
};
pub use pe::{COMIMAGE_FLAGS_ILONLY, cli_header, write_image, write_image_with_debug, write_pe};
pub use root::metadata_root;
pub use root::metadata_root_from_streams;
pub use signature::{
    TypeSig, field_signature, generic_method_signature, local_signature, method_signature,
    method_spec_signature, property_signature, type_signature, vararg_call_site_signature,
    vararg_method_signature,
};
pub use tables::{Column, HeapSizes, TableStream};
