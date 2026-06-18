#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Loads an ECMA-335 assembly into a runnable [`lamella_ves`] module.

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::fmt;

use lamella_cil::{Opcode, Operand};
use lamella_metadata::{Assembly, Method, MethodSig, SigType};
use lamella_token::Token;
use lamella_ves::intrinsics::{
    console_write, console_write_bool, console_write_char, console_write_double,
    console_write_int32, console_write_int64, console_write_line, console_write_line_bool,
    console_write_line_char, console_write_line_double, console_write_line_empty,
    console_write_line_int32, console_write_line_int64, string_concat, string_concat3,
    string_equals, string_get_chars, string_get_length, string_is_null_or_empty, string_not_equals,
    string_substring, string_substring_len,
};
use lamella_ves::{IntrinsicFn, MethodId, Module};

const TYPE_REF: u8 = 0x01;
const METHOD_DEF: u8 = 0x06;
const MEMBER_REF: u8 = 0x0A;

/// A loaded program: the runnable module and the entry-point method to start at.
pub struct Program {
    /// The module holding every loaded method, with tokens and strings bound.
    pub module: Module,
    /// The `MethodId` of the assembly's entry point.
    pub entry: MethodId,
}

/// Why [`load`] could not produce a runnable program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    /// The assembly declares no entry point (CLI header EntryPointToken is 0).
    NoEntryPoint,
    /// The entry-point token names no method that has an IL body.
    EntryHasNoBody,
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            LoadError::NoEntryPoint => "assembly has no entry point",
            LoadError::EntryHasNoBody => "entry point has no IL body",
        })
    }
}

/// Builds a runnable [`Program`] from `assembly`.
///
/// Every method with a body is added and bound to its MethodDef token (methods
/// iterate in table order, so the running 1-based count is the row that, tagged
/// [`METHOD_DEF`], reconstructs the token). `ldstr` and recognized BCL calls are
/// then resolved. The entry point is found by matching the CLI header's
/// entry-point token.
///
/// # Errors
/// [`LoadError::NoEntryPoint`] if the assembly names no entry point, or
/// [`LoadError::EntryHasNoBody`] if that token has no loadable body.
pub fn load(assembly: &Assembly) -> Result<Program, LoadError> {
    let entry_token = assembly.image().entry_point_token();
    if entry_token == 0 {
        return Err(LoadError::NoEntryPoint);
    }

    let mut module = Module::new();
    let mut entry = None;
    let mut string_tokens = BTreeSet::new();
    let mut bcl_call_tokens = BTreeSet::new();
    let mut row: u32 = 0;
    for type_def in assembly.type_defs() {
        for method in type_def.methods() {
            row += 1;
            let token = Token::new(METHOD_DEF, row);
            let Some(body) = method.body() else {
                continue;
            };
            for instruction in body.code.iter() {
                if let Operand::Token(operand) = &instruction.operand {
                    match instruction.opcode {
                        Opcode::Ldstr => {
                            string_tokens.insert(*operand);
                        }
                        Opcode::Call | Opcode::Callvirt if operand.table() == MEMBER_REF => {
                            bcl_call_tokens.insert(*operand);
                        }
                        _ => {}
                    }
                }
            }
            let id = module.add_method(body, arg_count(&method));
            module.bind_token(token, id);
            if token.0 == entry_token {
                entry = Some(id);
            }
        }
    }

    let entry = entry.ok_or(LoadError::EntryHasNoBody)?;
    bind_strings(assembly, &mut module, &string_tokens);
    bind_bcl_calls(assembly, &mut module, &bcl_call_tokens);
    Ok(Program { module, entry })
}

/// Binds each `ldstr` token to its `#US` string so the interpreter can materialize
/// it on the heap.
fn bind_strings(assembly: &Assembly, module: &mut Module, tokens: &BTreeSet<Token>) {
    let user_strings = assembly.image().user_strings();
    for token in tokens {
        if let Ok(blob) = user_strings.get(token.row()) {
            module.bind_string(*token, &decode_user_string(blob));
        }
    }
}

/// Binds recognized BCL `call` tokens to runtime intrinsics. Today that is
/// `System.Console.WriteLine`; an unrecognized call is left unbound and only traps
/// if actually executed.
fn bind_bcl_calls(assembly: &Assembly, module: &mut Module, tokens: &BTreeSet<Token>) {
    let mut bound: BTreeMap<usize, MethodId> = BTreeMap::new();
    for token in tokens {
        let Some(member) = assembly.member_ref(token.row()) else {
            continue;
        };
        let Some(method_name) = member.name() else {
            continue;
        };
        let parent = member.parent();
        if parent.table() != TYPE_REF {
            continue;
        }
        let Some(parent_type) = assembly
            .type_ref(parent.row())
            .and_then(|type_ref| type_ref.name())
        else {
            continue;
        };
        let signature = member.method_signature();
        let Some(function) = bcl_intrinsic(
            parent_type.namespace,
            parent_type.name,
            method_name,
            signature.as_ref(),
        ) else {
            continue;
        };
        let arg_count = signature
            .as_ref()
            .map_or(0, |sig| sig.parameters.len() + usize::from(sig.has_this));
        let arg_count = u16::try_from(arg_count).unwrap_or(u16::MAX);
        let id = match bound.get(&(function as usize)) {
            Some(&id) => id,
            None => {
                let id = module.add_intrinsic(function, arg_count);
                bound.insert(function as usize, id);
                id
            }
        };
        module.bind_token(*token, id);
    }
}

/// Maps a recognized BCL member -- by declaring type, method name, and signature --
/// to a runtime intrinsic and its argument count. Returns `None` for anything not
/// implemented yet; that call stays unbound and only traps if executed.
fn bcl_intrinsic(
    namespace: &str,
    type_name: &str,
    method: &str,
    signature: Option<&MethodSig>,
) -> Option<IntrinsicFn> {
    if namespace != "System" {
        return None;
    }
    match (type_name, method) {
        ("Console", "WriteLine") => console_write_line_overload(signature),
        ("Console", "Write") => console_write_overload(signature),
        ("String", "Concat") => string_concat_overload(signature),
        ("String", "get_Length") => string_get_length_overload(signature),
        ("String", "get_Chars") => string_get_chars_overload(signature),
        ("String", "op_Equality") => string_equals_overload(signature),
        ("String", "op_Inequality") => string_not_equals_overload(signature),
        ("String", "IsNullOrEmpty") => string_is_null_or_empty_overload(signature),
        ("String", "Substring") => string_substring_overload(signature),
        _ => None,
    }
}

/// The parameter types of a signature (empty if absent).
fn parameters_of(signature: Option<&MethodSig>) -> &[SigType] {
    match signature {
        Some(method_sig) => &method_sig.parameters,
        None => &[],
    }
}

/// Picks the `Console.WriteLine` overload by its parameter type.
fn console_write_line_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
    let intrinsic: IntrinsicFn = match parameters_of(signature) {
        [] => console_write_line_empty,
        [SigType::String] => console_write_line,
        [SigType::I4] => console_write_line_int32,
        [SigType::I8] => console_write_line_int64,
        [SigType::Boolean] => console_write_line_bool,
        [SigType::Char] => console_write_line_char,
        [SigType::R8] => console_write_line_double,
        _ => return None,
    };
    Some(intrinsic)
}

/// Picks the `Console.Write` overload (no line terminator) by its parameter type.
fn console_write_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
    let intrinsic: IntrinsicFn = match parameters_of(signature) {
        [SigType::String] => console_write,
        [SigType::I4] => console_write_int32,
        [SigType::I8] => console_write_int64,
        [SigType::Boolean] => console_write_bool,
        [SigType::Char] => console_write_char,
        [SigType::R8] => console_write_double,
        _ => return None,
    };
    Some(intrinsic)
}

/// Picks the `String.Concat` overload by its parameter types (the two-string form
/// for now -- what `a + b` on strings emits).
fn string_concat_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
    match parameters_of(signature) {
        [SigType::String, SigType::String] => Some(string_concat),
        [SigType::String, SigType::String, SigType::String] => Some(string_concat3),
        _ => None,
    }
}

/// The `String.Length` getter -- an instance method with no explicit parameters.
fn string_get_length_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
    match parameters_of(signature) {
        [] => Some(string_get_length),
        _ => None,
    }
}

/// The `String.op_Equality(string, string)` operator (what `==` on strings emits).
fn string_equals_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
    match parameters_of(signature) {
        [SigType::String, SigType::String] => Some(string_equals),
        _ => None,
    }
}

/// The `String.op_Inequality(string, string)` operator (`!=`).
fn string_not_equals_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
    match parameters_of(signature) {
        [SigType::String, SigType::String] => Some(string_not_equals),
        _ => None,
    }
}

/// `String.IsNullOrEmpty(string)` -- a static one-string predicate.
fn string_is_null_or_empty_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
    match parameters_of(signature) {
        [SigType::String] => Some(string_is_null_or_empty),
        _ => None,
    }
}

/// `String.Substring(int)` / `Substring(int, int)` -- instance methods.
fn string_substring_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
    match parameters_of(signature) {
        [SigType::I4] => Some(string_substring),
        [SigType::I4, SigType::I4] => Some(string_substring_len),
        _ => None,
    }
}

/// The `String.get_Chars(int)` indexer (`s[i]`) -- an instance method taking an
/// `int` index.
fn string_get_chars_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
    match parameters_of(signature) {
        [SigType::I4] => Some(string_get_chars),
        _ => None,
    }
}

/// Decodes a `#US` blob (UTF-16 little-endian code units followed by a one-byte
/// flag) into the code units the interpreter stores.
fn decode_user_string(blob: &[u8]) -> Vec<u16> {
    if blob.is_empty() {
        return Vec::new();
    }
    blob[..blob.len() - 1]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// The interpreter's argument-slot count: signature parameters, plus one for the
/// implicit `this` of an instance method.
fn arg_count(method: &Method<'_>) -> u16 {
    let parameters = method.signature().map_or(0, |sig| sig.parameters.len());
    let this = usize::from(!method.is_static());
    u16::try_from(parameters + this).unwrap_or(u16::MAX)
}
