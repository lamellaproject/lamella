#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Loads an ECMA-335 assembly into a runnable [`lamella_ves`] module.

extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::fmt;

use lamella_cil::{Opcode, Operand};
use lamella_metadata::{Assembly, Method};
use lamella_token::Token;
use lamella_ves::intrinsics::console_write_line;
use lamella_ves::{MethodId, Module};

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
    let mut write_line: Option<MethodId> = None;
    for token in tokens {
        let Some((namespace, type_name, method_name)) = resolve_bcl_member(assembly, *token) else {
            continue;
        };
        if namespace == "System" && type_name == "Console" && method_name == "WriteLine" {
            let id = *write_line.get_or_insert_with(|| module.add_intrinsic(console_write_line, 1));
            module.bind_token(*token, id);
        }
    }
}

/// Resolves a MemberRef token to `(namespace, type, method)` when its parent is a
/// TypeRef (the BCL case), via the metadata reader's `member_ref`/`type_ref`.
fn resolve_bcl_member<'a>(
    assembly: &Assembly<'a>,
    token: Token,
) -> Option<(&'a str, &'a str, &'a str)> {
    let member = assembly.member_ref(token.row())?;
    let method_name = member.name()?;
    let parent = member.parent();
    if parent.table() != TYPE_REF {
        return None;
    }
    let parent_type = assembly.type_ref(parent.row())?.name()?;
    Some((parent_type.namespace, parent_type.name, method_name))
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
