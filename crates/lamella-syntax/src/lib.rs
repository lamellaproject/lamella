#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lexical and syntactic analysis for C#.

extern crate alloc;

pub mod ast;
pub mod diagnostic;
pub mod lexer;
pub mod parser;
pub mod span;
pub mod token;
pub mod version;
