#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Managed PE and ECMA-335 metadata writer (1st edition, Partition II).

extern crate alloc;

pub mod heap;
pub mod module;
pub mod pdb;
pub mod pe;
pub mod root;
pub mod signature;
pub mod tables;

pub use heap::{
    BlobHeapBuilder, GuidHeapBuilder, StringHeapBuilder, UserStringHeapBuilder, compress_i32,
    compress_u32,
};
pub use module::ImageBuilder;
pub use pdb::{
    LocalVariable, MethodDebug, SequencePoint, build_portable_pdb, sequence_points_blob,
};
pub use pe::{COMIMAGE_FLAGS_ILONLY, cli_header, write_image, write_image_with_debug, write_pe};
pub use root::metadata_root;
pub use root::metadata_root_from_streams;
pub use signature::{
    TypeSig, field_signature, local_signature, method_signature, property_signature, type_signature,
};
pub use tables::{Column, HeapSizes, TableStream};
