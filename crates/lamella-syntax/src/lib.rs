#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lexical and syntactic analysis for C#.

extern crate alloc;

pub mod ast;
/// Generated single-byte ANSI code-page tables (see `tools/gen-codepages.ps1`), used by `decode`.
mod codepages;
pub mod decode;
pub mod diagnostic;
pub mod lexer;
pub mod parser;
pub mod span;
pub mod token;
pub mod version;
