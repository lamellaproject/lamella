//! **An ECMAScript engine for constrained devices.** It parses source, compiles it to a
//! pointer-free artifact, and executes that artifact -- on a host, or straight out of flash on a
//! microcontroller with no parser present.

#![cfg_attr(not(test), no_std)]

/// `alloc` NOT `std`: this engine has to reach a Cortex-M33, and a crate that pulls in `std`
/// cannot. The audit that preceded this found the dependency was almost entirely `core` and
/// `alloc` ALREADY -- exactly TWO `HashMap`s stood between it and a bare-metal target, both
/// swapped for `BTreeMap`. The object model had needed an ordered store since its first commit
/// anyway, for property-order reasons, so it was never a `HashMap` to begin with.

extern crate alloc;

/// THE `alloc` PRELUDE IS NOT AUTOMATIC. Under `no_std` the `std` prelude is gone, so `String`,
/// `Vec`, `Box`, `format!` and `vec!` have to be brought in by name -- and every module needs them,
/// which is why they are re-exported here rather than imported nine times.

pub(crate) use alloc::boxed::Box;
pub(crate) use alloc::string::{String, ToString};
pub(crate) use alloc::vec::Vec;
pub(crate) use alloc::{format, vec};

/// **The interpreter's semantics revision**, stamped into every precompiled artifact and checked
/// before one runs.
///
/// # WHY THE ENGINE IS VERSIONED SEPARATELY FROM THE BYTECODE FORMAT
///
/// They answer different questions and either can change without the other:
///
/// - the **format** version asks *can these bytes be parsed at all* -- a layout question, owned by
///   `lamella-js-bytecode`;
/// - this one asks *does the interpreter about to run this program implement what it means* -- a
///   SEMANTICS question, and only the engine can answer it.
///
/// A build that adds `Symbol`, fixes a coercion, or changes what an operator does has not touched
/// a single byte of the format. An artifact compiled against it and handed to an older engine would
/// parse perfectly and then **quietly compute something else** -- which is the one failure this
/// engine was created to refuse: not a missing feature, which is loud and bounded, but a silent
/// semantic deviation wearing the same syntax.
///
/// **BUMP THIS WHENEVER AN OBSERVABLE BEHAVIOUR CHANGES**, not merely when the format does. The
/// encoder stamps it as the artifact's `min_engine`, so an engine older than the one that compiled
/// a program refuses to run it and says which side is out of date.
///
/// It lives HERE, with the interpreter, and not in the format crate. A format crate declaring the
/// engine's version would be a second place for one fact, and the two would drift.
pub const ENGINE_VERSION: u16 = 1;

pub mod absence;
pub(crate) mod abstract_ops;
pub mod ast;
/// The bridges to the precompiled artifact format -- see the module docs for why they live here
/// and not in `lamella-js-bytecode`.
pub mod bytecode;
pub(crate) mod binary;
pub(crate) mod builtins;
pub(crate) mod collections;
pub(crate) mod context;
/// MEASUREMENT ONLY, AND OFF BY DEFAULT. The counters exist to supply the denominator for a
/// per-node cost; a build that is being TIMED must not carry them. See the module docs for why the
/// count and the time are deliberately taken from two different builds.
#[cfg(feature = "bench-counters")]
pub mod counters;
pub(crate) mod decimal;
pub mod diagnostic;
pub(crate) mod date;
pub(crate) mod early_errors;
/// `eval` COMPILES SOURCE AT RUN TIME, so it references the parser. Behind a feature because a
/// run-time check would keep that reference in every image -- see the module docs.
#[cfg(feature = "eval")]
pub(crate) mod eval;
pub(crate) mod generator;
pub(crate) mod generator_transform;
/// The object table and the whole-object write barrier -- what an `ObjectId` resolves to, and why
/// a realm object can live in flash. WARNING: nothing to do with the `lamella-heap` ALLOCATOR.
pub(crate) mod heap;
pub mod interpreter;
pub(crate) mod iterator;
pub(crate) mod json;
pub mod lexer;
pub(crate) mod math;
pub mod object;
pub mod parser;
/// The JOB QUEUE lives here, not only `Promise`. `await`, a REPL that continues a session and
/// ECMA-419's `onReadable`/`onWritable` all need the same drain point -- see the module docs.
pub(crate) mod promise;
/// The thirteen internal methods, and the routing that lets a proxy be found part-way up a
/// prototype chain -- see the module docs for why that reaches outside this file.
pub(crate) mod proxy;
/// GENERATED: the realm's objects and properties as constant data, read where they lie rather
/// than built by an installer that allocates. See its own header, and `realm_tables` for what
/// writes and checks it.
pub(crate) mod realm;
/// The realm rendered as text, the emitter that writes `realm`, and the gate that compares both
/// against the realm the installers build -- so that a change to the order built-ins are
/// registered in cannot silently repoint a generated table.
///
/// TEST-ONLY, AND DELIBERATELY: rendering and emitting the realm are development activities, and a
/// device image that carried a serializer for its own contents would be paying flash for a gate.
#[cfg(test)]
mod realm_tables;
pub(crate) mod reflect;
/// `RegExp` and the `lastIndex` protocol. The MATCHER is not here: it is shared with the other
/// languages in this tree, and this module is the ECMAScript object around it.
pub(crate) mod regexp;
pub mod source;
pub mod string_value;
pub mod token;
pub(crate) mod typed_array;
pub(crate) mod uri;
pub mod value;
pub(crate) mod unicode;

pub use context::{Context, ParseGoal};
pub use diagnostic::{Diagnostic, DiagnosticKind, Phase, Severity};
pub use lexer::{LexError, LexErrorKind, Lexer};
pub use parser::{parse_script, Parsed, Parser};
pub use source::{LineIndex, Position, Span};
pub use string_value::JsString;
pub use interpreter::{Completion, Interpreter};
pub use value::JsValue;
pub use token::{Goal, Punctuator, TemplateKind, TemplatePart, Token, TokenKind};

