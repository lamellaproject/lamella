#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! A WebAssembly 1.0 (MVP) interpreter for the Lamella WASM tier: it loads and RUNS modules
//! (`lamella-asm-wasm` is the emitting half), executing a guest against a host-granted import
//! world so a program compiled from any WASM-targeting language can drive real peripherals.
//! **THIS TIER IS EXPERIMENTAL AND SHIPS AS A PREVIEW.** It does work on hardware today, but
//! its scope is expected to expand considerably, and its API may change in any release --
//! treat everything here as provisional rather than settled. Two things to know up front.
//! Its SCOPE is WASM 1.0 plus the three post-MVP features current `rustc` emits by
//! default (sign-extension operators, non-trapping float-to-int,
//! `memory.copy`/`memory.fill`); anything else is a loud
//! [`DecodeErrorKind::UnsupportedFeature`], never a silent misread. And its VALIDATION is
//! deliberately partial -- rather than rejecting every ill-typed module up front, a malformed
//! binary fails decode loudly and a misbehaving module traps, so a type error a full validator
//! would have caught surfaces as a [`Trap`] at run time; what no input can do is reach
//! undefined behavior, since the crate is `forbid(unsafe_code)` with checked indexing
//! throughout.

extern crate alloc;

pub mod decode;
pub mod exec;
mod num;
pub mod ops;
pub mod simulated_events;
pub mod simulated_i2c;

use alloc::string::String;
use alloc::vec::Vec;

pub use decode::decode;
pub use exec::{EngineConfig, HostFunc, Instance, InstantiateError, Trap, Value, World};
pub use ops::{LabelKind, NumOp, Op};

/// A WebAssembly value type -- the four number types of WASM 1.0 (core spec 2.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValType {
    /// A 32-bit integer (signedness lives in the operation).
    I32,
    /// A 64-bit integer.
    I64,
    /// A 32-bit IEEE-754 float.
    F32,
    /// A 64-bit IEEE-754 float.
    F64,
}

/// A function signature: parameters and results, in order (core spec 2.3.3; results are zero
/// or one in this crate's scope).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FuncType {
    /// The parameter types, in order.
    pub params: Vec<ValType>,
    /// The result types (empty or a single type here; multi-value is out of scope).
    pub results: Vec<ValType>,
}

/// What an import requests from the host (core spec 2.5.11). All four kinds DECODE (the
/// structure is not the policy); the instantiation gate then admits only what the granted
/// world actually carries, so a table/memory/global import fails loudly THERE, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportKind {
    /// A function import, typed by an index into the module's type section.
    Func {
        /// The imported function's signature, as a type-section index.
        type_index: u32,
    },
    /// A table import (out of every granted world's scope today).
    Table,
    /// A memory import (guests so far declare their own memory rather than importing one).
    Memory,
    /// A global import.
    Global,
}

/// One import entry: the two-level name the host resolves, and what kind of thing it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The import's module namespace (e.g. `lamella_i2c`).
    pub module: String,
    /// The name within that namespace (e.g. `write_read`).
    pub name: String,
    /// What is being imported.
    pub kind: ImportKind,
}

/// What an export refers to (core spec 2.5.10), by index in the respective space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    /// A function, indexed across imports-then-defined.
    Func(u32),
    /// A table.
    Table(u32),
    /// A linear memory.
    Memory(u32),
    /// A global.
    Global(u32),
}

/// One export entry: a name the host looks up, and what it designates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    /// The exported name (e.g. `memory`, `_start`).
    pub name: String,
    /// What the name designates.
    pub kind: ExportKind,
}

/// A size range in units of the owning space (pages for memories, elements for tables).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The initial size.
    pub min: u32,
    /// The maximum size, or `None` for unbounded.
    pub max: Option<u32>,
}

/// A memory's type: limits counted in the memory's OWN pages, plus the page size itself.
///
/// `page_size_log2 == 16` is the classic 64 KiB page. `0` (single-BYTE pages) comes from
/// the `custom-page-sizes` proposal, phase 3, and ships here as a gated EXPERIMENT
/// ([`exec::EngineConfig::experimental_custom_page_sizes`], ON by default): the encoding
/// and semantics MAY CHANGE if the proposal changes before phase 4 -- precedents cut both
/// ways (exception handling was rewritten at phase 3; sign-ext and bulk-memory shipped
/// unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryType {
    /// The limits, in this memory's pages.
    pub limits: Limits,
    /// log2 of the page size in bytes: 16 (64 KiB) or 0 (1 B) -- the proposal's only two
    /// legal values today.
    pub page_size_log2: u8,
}

impl MemoryType {
    /// The page size in bytes.
    #[must_use]
    pub fn page_size(&self) -> u32 {
        1u32 << self.page_size_log2
    }
}

/// An MVP constant expression: a single const-shaped instruction plus `end` (core spec 3.3.30's
/// legal forms for this scope). Used by global initializers and active segment offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstExpr {
    /// `i32.const` -- the value as its bit pattern.
    I32(u32),
    /// `i64.const` -- the value as its bit pattern.
    I64(u64),
    /// `f32.const` -- the raw IEEE-754 bits.
    F32(u32),
    /// `f64.const` -- the raw IEEE-754 bits.
    F64(u64),
    /// `global.get` of an (imported, immutable) global, by global index.
    GlobalGet(u32),
}

/// A module-defined global: type, mutability, and its constant initializer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Global {
    /// The global's value type.
    pub ty: ValType,
    /// Whether the module may write it.
    pub mutable: bool,
    /// The initializer evaluated at instantiation.
    pub init: ConstExpr,
}

/// An active element segment: function indices copied into the table at `offset` when the
/// module is instantiated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElemSegment {
    /// Where in the table the entries land.
    pub offset: ConstExpr,
    /// The function indices, in order.
    pub funcs: Vec<u32>,
}

/// An active data segment: bytes copied into linear memory at `offset` at instantiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSegment {
    /// Where in memory the bytes land.
    pub offset: ConstExpr,
    /// The bytes themselves.
    pub bytes: Vec<u8>,
}

/// A defined function's body after decode: its declared locals (expanded, params NOT included)
/// and the internal instruction stream with every structured-control target resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct FuncBody {
    /// The declared locals, expanded one entry per local.
    pub locals: Vec<ValType>,
    /// The decoded instruction stream.
    pub ops: Vec<Op>,
}

/// A decoded module: every section in loaded form, ready to instantiate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Module {
    /// The type section: every signature the module references.
    pub types: Vec<FuncType>,
    /// The imports, in declaration order (this order defines the imported half of each
    /// index space).
    pub imports: Vec<Import>,
    /// The defined functions' signatures, as type-section indices (parallel to [`Self::code`]).
    pub functions: Vec<u32>,
    /// The funcref table, if declared.
    pub table: Option<Limits>,
    /// The linear memory, if declared (at most one in this scope).
    pub memory: Option<MemoryType>,
    /// The module-defined globals.
    pub globals: Vec<Global>,
    /// The exports.
    pub exports: Vec<Export>,
    /// The start function to run at instantiation, if declared.
    pub start: Option<u32>,
    /// The active element segments.
    pub elements: Vec<ElemSegment>,
    /// The defined functions' bodies (parallel to [`Self::functions`]).
    pub code: Vec<FuncBody>,
    /// The active data segments.
    pub data: Vec<DataSegment>,
}

impl Module {
    /// How many of the function index space's entries are imports (they come first).
    #[must_use]
    pub fn imported_func_count(&self) -> u32 {
        let mut count = 0;
        for import in &self.imports {
            if matches!(import.kind, ImportKind::Func { .. }) {
                count += 1;
            }
        }
        count
    }

    /// The signature of function `index` in the joint imports-then-defined index space.
    #[must_use]
    pub fn func_type(&self, index: u32) -> Option<&FuncType> {
        let imported = self.imported_func_count();
        let type_index = if index < imported {
            let mut seen = 0;
            let mut found = None;
            for import in &self.imports {
                if let ImportKind::Func { type_index } = import.kind {
                    if seen == index {
                        found = Some(type_index);
                        break;
                    }
                    seen += 1;
                }
            }
            found?
        } else {
            *self.functions.get((index - imported) as usize)?
        };
        self.types.get(type_index as usize)
    }

    /// Looks up an export by name.
    #[must_use]
    pub fn export(&self, name: &str) -> Option<&Export> {
        self.exports.iter().find(|e| e.name == name)
    }
}

/// Why a decode failed, positioned at the byte offset where the reader stood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    /// The absolute byte offset into the module image.
    pub offset: usize,
    /// What went wrong.
    pub kind: DecodeErrorKind,
}

/// The decode failure taxonomy. Structural malformations and out-of-scope features are kept
/// distinct: the former mean "not a module", the latter "a module this engine refuses".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeErrorKind {
    /// The input ended inside a construct.
    UnexpectedEof,
    /// The leading magic was not `\0asm`.
    BadMagic,
    /// The version field was not 1.
    BadVersion,
    /// A LEB128 integer overran its maximum byte count or its type's range.
    LebOverflow,
    /// A byte that should have named a value type did not.
    BadValType(u8),
    /// An unknown section id.
    BadSectionId(u8),
    /// A non-custom section appeared out of order or twice.
    SectionOrder(u8),
    /// A section's contents did not fill its declared size exactly.
    SectionSize,
    /// The function and code sections declare different counts.
    FuncCodeMismatch,
    /// An unknown or out-of-scope opcode byte.
    BadOpcode(u8),
    /// An unknown or out-of-scope `0xFC`-prefixed opcode.
    BadPrefixOpcode(u32),
    /// A recognized construct that is deliberately outside this engine's scope.
    UnsupportedFeature(&'static str),
    /// An index referred past its space; the label names the space.
    IndexOutOfRange(&'static str),
    /// A count exceeded a sanity bound; the label names the vector.
    TooMany(&'static str),
    /// An `if` with a result type has no `else` arm (its false edge could produce no value).
    IfResultWithoutElse,
    /// An `else` appeared outside an `if`, or twice.
    BadElseContext,
    /// More `end`s than open control frames.
    ControlUnderflow,
    /// A function body ended with control frames still open.
    UnbalancedControl,
    /// A constant expression used a form outside the MVP grammar.
    BadConstExpr,
    /// A flags byte carried a value outside this engine's scope; the label names the site.
    BadFlags(&'static str),
    /// A name was not valid UTF-8.
    BadUtf8,
    /// Bytes remained after the last section.
    TrailingBytes,
}
