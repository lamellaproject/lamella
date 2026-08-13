//! **A precompiled, XIP-able representation of an ECMAScript program.**

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]


pub mod format;
pub mod read;

pub use format::{
    FormatError, Header, Remedy, Tag, FORMAT_VERSION, MAGIC, MIN_READER, MIN_SUPPORTED_FORMAT,
};
pub use read::{Artifact, Fields, Node, Span, Utf16, Versions};
