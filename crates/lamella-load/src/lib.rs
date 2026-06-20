#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Loads an ECMA-335 assembly into a runnable [`lamella_ves`] module.

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use lamella_cil::{Opcode, Operand};
use lamella_metadata::{Assembly, ConstantValue, Method, MethodSig, SigType};
use lamella_token::Token;
use lamella_ves::intrinsics::{
    array_get_value, array_set_value, boolean_to_string, char_to_string, console_write,
    console_write_bool, console_write_char, console_write_int32, console_write_int64,
    console_write_line, console_write_line_bool, console_write_line_char, console_write_line_empty,
    console_write_line_int32, console_write_line_int64, console_write_line_object,
    delegate_combine, delegate_remove, enum_is_defined, enum_parse, exception_ctor,
    exception_get_message, int32_to_string, int64_to_string, md_array_get, md_array_get_length,
    md_array_length, md_array_set, object_ctor, object_reference_equals, object_to_string,
    string_concat, string_concat_object2, string_concat_object3, string_concat3, string_equals,
    string_get_chars, string_get_length, string_is_null_or_empty, string_not_equals,
    string_substring, string_substring_len, type_from_handle,
};
#[cfg(feature = "gc")]
use lamella_ves::intrinsics::gc_collect;
#[cfg(feature = "finalizers")]
use lamella_ves::intrinsics::{
    reregister_finalize, suppress_finalize, wait_for_pending_finalizers,
};
#[cfg(feature = "NETMFv4_4")]
use lamella_ves::intrinsics::{
    boolean_parse, char_is_digit, char_is_letter, char_is_letter_or_digit, char_is_lower,
    char_is_upper, char_is_white_space, char_to_lower, char_to_upper, collection_contains,
    collection_push, convert_to_boolean_int, convert_to_byte_int, convert_to_char_int, int32_parse,
    int64_parse, list_add, list_clear, list_get_count, list_get_item, list_insert, list_remove_at,
    list_set_item, map_add, map_contains, map_get_count, map_get_item, map_remove, map_set_item,
    math_abs_int32, math_abs_int64, math_max_int32, math_max_int64, math_min_int32, math_min_int64,
    math_sign_int32, math_sign_int64, queue_dequeue, queue_peek, stack_peek, stack_pop,
    string_builder_append_char, string_builder_append_int, string_builder_append_string,
    string_builder_get_length, string_builder_insert, string_builder_remove,
    string_builder_replace_char, string_builder_to_string, string_contains, string_ends_with,
    string_index_of_char, string_index_of_string, string_insert, string_join,
    string_last_index_of_char, string_pad_left, string_pad_right, string_remove,
    string_replace_char, string_replace_string, string_split_char, string_starts_with,
    string_to_char_array, string_to_lower, string_to_upper, string_trim,
};
#[cfg(feature = "float")]
use lamella_ves::intrinsics::{
    console_write_double, console_write_line_double, convert_to_int32_double, double_to_string,
    math_abs_f64, math_ceiling_f64, math_floor_f64, math_max_f64, math_min_f64, math_round_f64,
    math_sign_f64, math_truncate_f64,
};
#[cfg(feature = "math-transcendental")]
use lamella_ves::intrinsics::{
    math_cos_f64, math_exp_f64, math_log_f64, math_log10_f64, math_pow_f64, math_sin_f64,
    math_sqrt_f64, math_tan_f64,
};
use lamella_ves::{IntrinsicFn, MethodId, Module, Value};

const TYPE_REF: u8 = 0x01;
const TYPE_DEF: u8 = 0x02;
const FIELD: u8 = 0x04;
const METHOD_DEF: u8 = 0x06;
const MEMBER_REF: u8 = 0x0A;
const TYPE_SPEC: u8 = 0x1B;

const METHOD_VIRTUAL: u32 = 0x0040;
const METHOD_NEWSLOT: u32 = 0x0100;

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

/// A name index mapping a stable encoding of a method's identity -- namespace, declaring
/// type, method name, and parameter types -- to its [`MethodId`]. Built while loading an
/// assembly, it lets a later assembly resolve a cross-assembly call to the defining
/// assembly's method by name (the metadata `MemberRef` carries the name, not a `MethodId`).
type NameIndex = BTreeMap<String, MethodId>;

/// A stable key for a method's identity across assemblies: its namespace, declaring type
/// name, method name, and parameter types. A program's `MemberRef` to a corlib method
/// computes the same key the corlib's `MethodDef` did, so they match.
///
/// Each parameter is encoded by [`encode_sig_type`] against `assembly`, which resolves a
/// `Class` / `ValueType` token to the named type -- a `Class(token)` carries a metadata
/// token that differs between assemblies (the program's `TypeRef` vs the corlib's
/// `TypeDef` for the same type), so the raw `{:?}` would not match across the seam.
fn name_key(
    assembly: &Assembly,
    namespace: &str,
    type_name: &str,
    method: &str,
    params: &[SigType],
) -> String {
    let mut key = alloc::format!("{namespace}.{type_name}.{method}|");
    for param in params {
        key.push_str(&encode_sig_type(assembly, param));
        key.push(',');
    }
    key
}

/// A portable encoding of one parameter [`SigType`]: a token-bearing `Class` / `ValueType`
/// (or an array / pointer / byref of one) resolves to the named type so the encoding is the
/// same across assemblies; every token-free type keeps its stable `{:?}` form (so a
/// primitive-only signature, like `Concat(string, string)`, encodes exactly as before).
fn encode_sig_type(assembly: &Assembly, sig: &SigType) -> String {
    match sig {
        SigType::Class(token) | SigType::ValueType(token) => match assembly.type_token_name(*token) {
            Some(name) => match canonical_sig_type(name.namespace, name.name) {
                Some(canonical) => alloc::format!("{canonical:?}"),
                None => {
                    let kind = if matches!(sig, SigType::Class(_)) {
                        "Class"
                    } else {
                        "ValueType"
                    };
                    alloc::format!("{kind}({}.{})", name.namespace, name.name)
                }
            },
            None => alloc::format!("{sig:?}"),
        },
        SigType::SzArray(element) => {
            alloc::format!("SzArray({})", encode_sig_type(assembly, element))
        }
        SigType::Pointer(pointee) => {
            alloc::format!("Pointer({})", encode_sig_type(assembly, pointee))
        }
        SigType::ByRef(referent) => {
            alloc::format!("ByRef({})", encode_sig_type(assembly, referent))
        }
        other => alloc::format!("{other:?}"),
    }
}

/// The `ELEMENT_TYPE_*` short form a fully-named core `System` type maps to, so a `Class` /
/// `ValueType` reference to it (e.g. a corlib's own `System.Object`, or a `[mscorlib]` ref csc
/// emits) encodes the same as the short form a program uses for the same type.
fn canonical_sig_type(namespace: &str, name: &str) -> Option<SigType> {
    if namespace != "System" {
        return None;
    }
    Some(match name {
        "Object" => SigType::Object,
        "String" => SigType::String,
        "Void" => SigType::Void,
        "Boolean" => SigType::Boolean,
        "Char" => SigType::Char,
        "SByte" => SigType::I1,
        "Byte" => SigType::U1,
        "Int16" => SigType::I2,
        "UInt16" => SigType::U2,
        "Int32" => SigType::I4,
        "UInt32" => SigType::U4,
        "Int64" => SigType::I8,
        "UInt64" => SigType::U8,
        "Single" => SigType::R4,
        "Double" => SigType::R8,
        "IntPtr" => SigType::IntPtr,
        "UIntPtr" => SigType::UIntPtr,
        _ => return None,
    })
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
    if assembly.image().entry_point_token() == 0 {
        return Err(LoadError::NoEntryPoint);
    }
    let mut module = Module::new();
    let mut index = NameIndex::new();
    let entry = load_assembly(&mut module, assembly, 0, &mut index, false);
    let entry = entry.ok_or(LoadError::EntryHasNoBody)?;
    Ok(Program { module, entry })
}

/// Loads a managed corlib (assembly 0) and a program (assembly 1) into one [`Module`],
/// resolving the program's cross-assembly calls to the corlib's methods by name.
///
/// The corlib loads first into assembly slot 0 -- its types take the low [`crate::TypeId`]
/// range and its methods are recorded in a [`NameIndex`]; its own entry-point token (if any)
/// is ignored, since a corlib is a library. The program then loads into slot 1 at a type
/// offset past the corlib's types, and each cross-assembly `MemberRef` it makes is resolved
/// against the index (falling back to a Rust intrinsic only when the index has no match).
///
/// # Errors
/// [`LoadError::NoEntryPoint`] if the program names no entry point, or
/// [`LoadError::EntryHasNoBody`] if the program's entry-point token has no loadable body.
pub fn load_with_corlib(corlib: &Assembly, program: &Assembly) -> Result<Program, LoadError> {
    if program.image().entry_point_token() == 0 {
        return Err(LoadError::NoEntryPoint);
    }
    let mut module = Module::new();
    let mut index = NameIndex::new();
    load_assembly(&mut module, corlib, 0, &mut index, true);
    let entry = load_assembly(&mut module, program, 1, &mut index, true);
    let entry = entry.ok_or(LoadError::EntryHasNoBody)?;
    Ok(Program { module, entry })
}

/// Loads one assembly into `module` under assembly id `asm`, returning the entry-point
/// [`MethodId`] if the assembly's CLI header names one (a corlib library has none, which is
/// not an error here -- the caller decides whether a missing entry point matters).
///
/// `type_offset` is `module.type_count()` captured before this assembly loads: an
/// assembly-local type index `i` becomes the global [`crate::TypeId`] `type_offset + i`. The
/// per-type recursion (field layout / vtable / signature maps) stays on local indices into
/// the collected `extends`/`virtuals`/fields vectors; only the calls that bind into the
/// shared `module` use the global id and the real `asm`. When `resolve_external`, a
/// `MemberRef` is first looked up in `index` (so a call to a corlib-defined method binds to
/// the corlib's [`MethodId`]); only an unindexed member falls through to a Rust intrinsic.
/// Every method this assembly defines (managed-body or `runtime`) is inserted into `index`.
fn load_assembly(
    module: &mut Module,
    assembly: &Assembly,
    asm: u8,
    index: &mut NameIndex,
    resolve_external: bool,
) -> Option<MethodId> {
    let type_offset = module.type_count();
    let entry_token = assembly.image().entry_point_token();

    let mut entry = None;
    let mut string_tokens = BTreeSet::new();
    let mut bcl_call_tokens = BTreeSet::new();
    let mut newarr_tokens = BTreeSet::new();
    let mut callvirt_tokens = BTreeSet::new();
    let mut methoddef_sigs: BTreeMap<u32, (String, Vec<SigType>)> = BTreeMap::new();
    let mut type_extends: Vec<Token> = Vec::new();
    let mut type_virtuals: Vec<Vec<VirtualMethod>> = Vec::new();
    let mut own_fields: Vec<Vec<(Token, Value)>> = Vec::new();
    let mut method_row: u32 = 0;
    let mut field_row: u32 = 0;
    let mut type_row: u32 = 0;
    for type_def in assembly.type_defs() {
        type_row += 1;
        let is_enum = is_enum_type(assembly, type_def.extends());
        let mut own = Vec::new();
        for field in type_def.fields() {
            field_row += 1;
            let token = Token::new(FIELD, field_row);
            if field.is_static() {
                if !field.is_literal() {
                    module.bind_static_field(asm, token, default_field_value(field.signature()));
                } else if is_enum {
                    if let (Some(name), Some(constant)) = (field.name(), field.constant()) {
                        let type_token = Token::new(TYPE_DEF, type_row).0;
                        if matches!(constant, ConstantValue::I8(_) | ConstantValue::U8(_)) {
                            module.set_enum_wide(asm, type_token);
                        }
                        if let Some(value) = constant_as_i64(constant) {
                            module.set_enum_constant(asm, type_token, value, name.into());
                        }
                    }
                }
                continue;
            }
            own.push((token, default_field_value(field.signature())));
        }
        let type_id = module.add_type(Vec::new());
        if module.string_type_id().is_none() {
            if let Some(name) = type_def.name() {
                if name.namespace == "System" && name.name == "String" {
                    module.set_string_type_id(type_id);
                }
            }
        }
        for (token, _) in &own {
            module.bind_field_type(asm, *token, type_id);
        }
        own_fields.push(own);
        type_extends.push(type_def.extends());

        let mut virtuals = Vec::new();
        let is_delegate = is_delegate_type(assembly, type_def.extends());
        for method in type_def.methods() {
            method_row += 1;
            let token = Token::new(METHOD_DEF, method_row);
            let name: String = method.name().unwrap_or("").into();
            let params: Vec<SigType> = method
                .signature()
                .map(|sig| sig.parameters)
                .unwrap_or_default();
            methoddef_sigs.insert(method_row, (name.clone(), params.clone()));
            if is_delegate {
                if name == ".ctor" {
                    module.mark_delegate_ctor(asm, token);
                } else if name == "Invoke" {
                    let count = u16::try_from(params.len()).unwrap_or(u16::MAX);
                    module.mark_delegate_invoke(asm, token, count);
                }
            }
            let Some(body) = method.body() else {
                if method.is_runtime_impl() {
                    let signature = method.signature();
                    let intrinsic = type_def.name().and_then(|declaring| {
                        bcl_intrinsic(
                            declaring.namespace,
                            declaring.name,
                            &name,
                            signature.as_ref(),
                        )
                    });
                    if let Some(func) = intrinsic {
                        let id = module.add_intrinsic(asm, func, arg_count(&method));
                        module.bind_token(asm, token, id);
                        module.set_method_type(id, type_id);
                        if let Some(declaring) = type_def.name() {
                            index.insert(
                                name_key(
                                    assembly,
                                    declaring.namespace,
                                    declaring.name,
                                    &name,
                                    &params,
                                ),
                                id,
                            );
                        }
                        if token.0 == entry_token {
                            entry = Some(id);
                        }
                    }
                }
                continue;
            };
            for instruction in body.code.iter() {
                if let Operand::Token(operand) = &instruction.operand {
                    match instruction.opcode {
                        Opcode::Ldstr => {
                            string_tokens.insert(*operand);
                        }
                        Opcode::Callvirt => {
                            callvirt_tokens.insert(*operand);
                            if operand.table() == MEMBER_REF {
                                bcl_call_tokens.insert(*operand);
                            }
                        }
                        Opcode::Call | Opcode::Newobj if operand.table() == MEMBER_REF => {
                            bcl_call_tokens.insert(*operand);
                        }
                        Opcode::Newarr => {
                            newarr_tokens.insert(*operand);
                        }
                        _ => {}
                    }
                }
            }
            let id = module.add_method(asm, body, arg_count(&method));
            module.bind_token(asm, token, id);
            module.set_method_type(id, type_id);
            if let Some(declaring) = type_def.name() {
                index.insert(
                    name_key(
                        assembly,
                        declaring.namespace,
                        declaring.name,
                        &name,
                        &params,
                    ),
                    id,
                );
            }
            let qualified = match type_def.name() {
                Some(declaring) if !declaring.namespace.is_empty() => {
                    alloc::format!("{}.{}.{}", declaring.namespace, declaring.name, name)
                }
                Some(declaring) => alloc::format!("{}.{}", declaring.name, name),
                None => name.clone(),
            };
            let mut arg_names = Vec::new();
            if !method.is_static() {
                arg_names.push(String::from("this"));
            }
            let mut declared = alloc::vec![String::new(); params.len()];
            for param in method.params() {
                if let Ok(slot) = usize::try_from(param.sequence().wrapping_sub(1)) {
                    if let (Some(entry), Some(param_name)) = (declared.get_mut(slot), param.name())
                    {
                        *entry = String::from(param_name);
                    }
                }
            }
            arg_names.extend(declared);
            module.set_method_debug(id, qualified, arg_names);
            if name == ".cctor" {
                module.add_static_ctor(id);
            }
            if name == "Finalize" && arg_count(&method) == 1 {
                module.set_finalizer(type_id, id);
            }
            if method.flags() & METHOD_VIRTUAL != 0 {
                virtuals.push(VirtualMethod {
                    id,
                    name,
                    params,
                    newslot: method.flags() & METHOD_NEWSLOT != 0,
                });
            }
            if token.0 == entry_token {
                entry = Some(id);
            }
        }
        type_virtuals.push(virtuals);
    }

    bind_strings(assembly, module, asm, &string_tokens);
    bind_bcl_calls(
        assembly,
        module,
        asm,
        index,
        resolve_external,
        &bcl_call_tokens,
    );
    bind_array_defaults(assembly, module, asm, &newarr_tokens);
    build_field_layouts(module, asm, type_offset, &type_extends, &own_fields);
    build_vtables(module, asm, type_offset, &type_extends, &type_virtuals);
    build_sig_methods(module, asm, type_offset, &type_extends, &type_virtuals);
    bind_call_targets(module, assembly, asm, &callvirt_tokens, &methoddef_sigs);
    bind_types(module, asm, type_offset, &type_extends);
    entry
}

/// Binds each `ldstr` token to its `#US` string so the interpreter can materialize
/// it on the heap.
fn bind_strings(assembly: &Assembly, module: &mut Module, asm: u8, tokens: &BTreeSet<Token>) {
    let user_strings = assembly.image().user_strings();
    for token in tokens {
        if let Ok(blob) = user_strings.get(token.row()) {
            module.bind_string(asm, *token, &decode_user_string(blob));
        }
    }
}

/// Binds recognized BCL `call` tokens to runtime intrinsics. When `resolve_external`, a
/// `MemberRef` is first matched against `index` (so a cross-assembly call to a corlib-defined
/// method binds to the corlib's [`MethodId`]) before any intrinsic is considered. Otherwise --
/// or when the index has no match -- a recognized BCL member binds to its Rust intrinsic and
/// anything unrecognized is left unbound (it traps only if executed).
fn bind_bcl_calls(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    index: &NameIndex,
    resolve_external: bool,
    tokens: &BTreeSet<Token>,
) {
    let mut bound: BTreeMap<(usize, u16), MethodId> = BTreeMap::new();
    for token in tokens {
        let Some(member) = assembly.member_ref(token.row()) else {
            continue;
        };
        let Some(method_name) = member.name() else {
            continue;
        };
        let parent = member.parent();
        let signature = member.method_signature();
        let params: &[SigType] = signature.as_ref().map_or(&[], |sig| &sig.parameters);
        let arg_count = u16::try_from(
            signature
                .as_ref()
                .map_or(0, |sig| sig.parameters.len() + usize::from(sig.has_this)),
        )
        .unwrap_or(u16::MAX);

        let function = if parent.table() == TYPE_SPEC {
            match method_name {
                ".ctor" => {
                    let rank = signature.as_ref().map_or(0, |sig| sig.parameters.len());
                    module.mark_md_array_ctor(asm, *token, u16::try_from(rank).unwrap_or(0));
                    continue;
                }
                "Get" => Some(md_array_get as IntrinsicFn),
                "Set" => Some(md_array_set as IntrinsicFn),
                _ => continue,
            }
        } else if parent.table() == TYPE_REF {
            let Some(parent_type) = assembly
                .type_ref(parent.row())
                .and_then(|type_ref| type_ref.name())
            else {
                continue;
            };
            if resolve_external {
                let key = name_key(
                    assembly,
                    parent_type.namespace,
                    parent_type.name,
                    method_name,
                    params,
                );
                if let Some(&target) = index.get(&key) {
                    module.bind_token(asm, *token, target);
                    continue;
                }
            }
            if let Some(params) = string_builder_ctor(
                parent_type.namespace,
                parent_type.name,
                method_name,
                signature.as_ref(),
            ) {
                module.mark_string_builder_ctor(asm, *token, params);
                continue;
            }
            if let Some(params) = list_ctor(
                parent_type.namespace,
                parent_type.name,
                method_name,
                signature.as_ref(),
            ) {
                module.mark_list_ctor(asm, *token, params);
                continue;
            }
            bcl_intrinsic(
                parent_type.namespace,
                parent_type.name,
                method_name,
                signature.as_ref(),
            )
        } else {
            continue;
        };
        let Some(function) = function else {
            continue;
        };
        let id = match bound.get(&(function as usize, arg_count)) {
            Some(&id) => id,
            None => {
                let id = module.add_intrinsic(asm, function, arg_count);
                bound.insert((function as usize, arg_count), id);
                id
            }
        };
        module.bind_token(asm, *token, id);
    }
}

/// The parameter count of a `System.Text.StringBuilder` constructor, if this member is one,
/// so `newobj` can allocate a builder. Always `None` without the NETMF surface that defines it.
#[cfg(feature = "NETMFv4_4")]
fn string_builder_ctor(
    namespace: &str,
    type_name: &str,
    method: &str,
    signature: Option<&MethodSig>,
) -> Option<u16> {
    if namespace == "System.Text" && type_name == "StringBuilder" && method == ".ctor" {
        Some(u16::try_from(signature.map_or(0, |sig| sig.parameters.len())).unwrap_or(0))
    } else {
        None
    }
}

#[cfg(not(feature = "NETMFv4_4"))]
fn string_builder_ctor(
    _namespace: &str,
    _type_name: &str,
    _method: &str,
    _signature: Option<&MethodSig>,
) -> Option<u16> {
    None
}

/// The parameter count of a `System.Collections.ArrayList` constructor, if this member is one,
/// so `newobj` can allocate an empty list. Always `None` without the NETMF surface.
#[cfg(feature = "NETMFv4_4")]
fn list_ctor(
    namespace: &str,
    type_name: &str,
    method: &str,
    signature: Option<&MethodSig>,
) -> Option<u16> {
    if namespace == "System.Collections"
        && matches!(type_name, "ArrayList" | "Hashtable" | "Stack" | "Queue")
        && method == ".ctor"
    {
        Some(u16::try_from(signature.map_or(0, |sig| sig.parameters.len())).unwrap_or(0))
    } else {
        None
    }
}

#[cfg(not(feature = "NETMFv4_4"))]
fn list_ctor(
    _namespace: &str,
    _type_name: &str,
    _method: &str,
    _signature: Option<&MethodSig>,
) -> Option<u16> {
    None
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
    #[cfg(feature = "NETMFv4_4")]
    if namespace == "System.Text" {
        return netmf::text_intrinsic(type_name, method, signature);
    }
    #[cfg(feature = "NETMFv4_4")]
    if namespace == "System.Collections" {
        return netmf::collections_intrinsic(type_name, method, signature);
    }
    if namespace != "System" {
        return None;
    }
    let base: Option<IntrinsicFn> = match (type_name, method) {
        ("Console", "WriteLine") => console_write_line_overload(signature),
        ("Console", "Write") => console_write_overload(signature),
        ("String", "Concat") => string_concat_overload(signature),
        ("String", "get_Length") => string_get_length_overload(signature),
        ("String", "get_Chars") => string_get_chars_overload(signature),
        ("String", "op_Equality") => string_equals_overload(signature),
        ("String", "op_Inequality") => string_not_equals_overload(signature),
        ("String", "IsNullOrEmpty") => string_is_null_or_empty_overload(signature),
        ("String", "Substring") => string_substring_overload(signature),
        ("Object", ".ctor") => object_ctor_overload(signature),
        ("Object", "ReferenceEquals") => match parameters_of(signature) {
            [SigType::Object, SigType::Object] => Some(object_reference_equals),
            _ => None,
        },
        ("Object", "Finalize") => Some(object_ctor),
        ("Exception", ".ctor") => Some(exception_ctor),
        ("Exception", "get_Message") => Some(exception_get_message),
        #[cfg(feature = "finalizers")]
        ("GC", "SuppressFinalize") => Some(suppress_finalize),
        #[cfg(feature = "finalizers")]
        ("GC", "ReRegisterForFinalize") => Some(reregister_finalize),
        #[cfg(feature = "gc")]
        ("GC", "Collect") => Some(gc_collect),
        #[cfg(feature = "finalizers")]
        ("GC", "WaitForPendingFinalizers") => Some(wait_for_pending_finalizers),
        ("Type", "GetTypeFromHandle") => Some(type_from_handle),
        ("Enum", "Parse") => Some(enum_parse),
        ("Enum", "IsDefined") => Some(enum_is_defined),
        ("Array", "get_Length") => Some(md_array_length),
        ("Array", "GetLength") => Some(md_array_get_length),
        ("Array", "GetValue") => match parameters_of(signature) {
            [SigType::I4] => Some(array_get_value),
            _ => None,
        },
        ("Array", "SetValue") => match parameters_of(signature) {
            [SigType::Object, SigType::I4] => Some(array_set_value),
            _ => None,
        },
        ("Int32", "ToString") => to_string_overload(int32_to_string, signature),
        ("Boolean", "ToString") => to_string_overload(boolean_to_string, signature),
        ("Char", "ToString") => to_string_overload(char_to_string, signature),
        ("Int64", "ToString") => to_string_overload(int64_to_string, signature),
        #[cfg(feature = "float")]
        ("Double", "ToString") => to_string_overload(double_to_string, signature),
        ("Object", "ToString") => to_string_overload(object_to_string, signature),
        ("Delegate", "Combine") => Some(delegate_combine),
        ("Delegate", "Remove") => Some(delegate_remove),
        _ => None,
    };
    if base.is_some() {
        return base;
    }
    #[cfg(feature = "NETMFv4_4")]
    {
        netmf::netmf_intrinsic(type_name, method, signature)
    }
    #[cfg(not(feature = "NETMFv4_4"))]
    {
        None
    }
}

/// `System.Object..ctor()` -- the base constructor every constructor chains to; a
/// no-op intrinsic (it takes only `this`).
fn object_ctor_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
    match parameters_of(signature) {
        [] => Some(object_ctor),
        _ => None,
    }
}

/// The zero value a freshly allocated instance field of this signature holds
/// (ECMA-335 III.4.21 zero-initializes instances): the numeric zero of its width,
/// or null for a reference. Value types other than these primitives are not laid out
/// inline yet, so they fall back to null.
fn default_field_value(signature: Option<SigType>) -> Value {
    match signature {
        Some(SigType::I8 | SigType::U8) => Value::Int64(0),
        #[cfg(feature = "float")]
        Some(SigType::R4 | SigType::R8) => Value::Float(0.0),
        Some(
            SigType::Boolean
            | SigType::Char
            | SigType::I1
            | SigType::U1
            | SigType::I2
            | SigType::U2
            | SigType::I4
            | SigType::U4,
        ) => Value::Int32(0),
        _ => Value::Null,
    }
}

/// Binds each `newarr` element-type token to its elements' zero value.
fn bind_array_defaults(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    tokens: &BTreeSet<Token>,
) {
    for token in tokens {
        module.bind_array_default(asm, *token, array_element_default(assembly, *token));
    }
}

/// The zero value an array's elements take (ECMA-335 III.4.20): the numeric zero of a
/// primitive element, or null for a reference element. The `newarr` operand names the
/// element type; a `System` primitive -- whether a program's `TypeRef` or the corlib's own
/// `TypeDef` (it defines `System.Int32` etc.) -- gets its numeric zero; a user `TypeDef`, a
/// `TypeSpec` (array/generic), and unrecognized names are references (value-type array
/// elements are not laid out inline yet).
fn array_element_default(assembly: &Assembly, element_type: Token) -> Value {
    let Some(name) = assembly.type_token_name(element_type) else {
        return Value::Null;
    };
    if name.namespace != "System" {
        return Value::Null;
    }
    match name.name {
        "Int32" | "UInt32" | "Int16" | "UInt16" | "SByte" | "Byte" | "Boolean" | "Char" => {
            Value::Int32(0)
        }
        "Int64" | "UInt64" => Value::Int64(0),
        #[cfg(feature = "float")]
        "Single" | "Double" => Value::Float(0.0),
        "IntPtr" | "UIntPtr" => Value::NativeInt(0),
        _ => Value::Null,
    }
}

/// Computes each type's full instance-field layout (base fields first, then own) and
/// binds each own field token to its cumulative slot, so a derived instance carries
/// its inherited fields at the same slots its base uses.
fn build_field_layouts(
    module: &mut Module,
    asm: u8,
    type_offset: usize,
    extends: &[Token],
    own_fields: &[Vec<(Token, Value)>],
) {
    let mut memo: Vec<Option<Vec<Value>>> = alloc::vec![None; extends.len()];
    for local in 0..extends.len() {
        let full = field_layout(local, extends, own_fields, &mut memo);
        let base_count = full.len() - own_fields[local].len();
        for (index, (token, _)) in own_fields[local].iter().enumerate() {
            module.bind_field(asm, *token, (base_count + index) as u32);
        }
        module.set_type_field_defaults((type_offset + local) as u32, full);
    }
}

/// The memoized full field layout (zero values) of `type_id`: its base's layout
/// followed by its own instance fields.
fn field_layout(
    type_id: usize,
    extends: &[Token],
    own_fields: &[Vec<(Token, Value)>],
    memo: &mut [Option<Vec<Value>>],
) -> Vec<Value> {
    if let Some(layout) = &memo[type_id] {
        return layout.clone();
    }
    let mut layout = match base_type_id(extends[type_id], extends.len()) {
        Some(base) => field_layout(base, extends, own_fields, memo),
        None => Vec::new(),
    };
    layout.extend(
        own_fields[type_id]
            .iter()
            .map(|(_, default)| default.clone()),
    );
    memo[type_id] = Some(layout.clone());
    layout
}

/// A virtual method declared by a type, for vtable construction.
struct VirtualMethod {
    id: MethodId,
    name: String,
    params: Vec<SigType>,
    newslot: bool,
}

/// One slot of a vtable under construction: the virtual method's identity (name +
/// parameter types, to match an override to the slot it overrides) and the current
/// most-derived implementation.
#[derive(Clone)]
struct VtableSlot {
    name: String,
    params: Vec<SigType>,
    method: MethodId,
}

/// Whether a type extends `System.MulticastDelegate` / `System.Delegate` -- i.e. is a
/// delegate type, whose runtime-provided `.ctor` / `Invoke` the loader records.
fn is_delegate_type(assembly: &Assembly, extends: Token) -> bool {
    if extends.table() != TYPE_REF {
        return false;
    }
    assembly
        .type_ref(extends.row())
        .and_then(|type_ref| type_ref.name())
        .is_some_and(|name| matches!(name.name, "MulticastDelegate" | "Delegate"))
}

/// Whether a type extends `System.Enum` -- i.e. is an enum, whose literal constants the
/// loader records (by value) so `Enum.ToString` can name them.
fn is_enum_type(assembly: &Assembly, extends: Token) -> bool {
    if extends.table() != TYPE_REF {
        return false;
    }
    assembly
        .type_ref(extends.row())
        .and_then(|type_ref| type_ref.name())
        .is_some_and(|name| name.name == "Enum")
}

/// An integer constant's value as `i64` (an enum's underlying type is an integer kind).
fn constant_as_i64(value: ConstantValue) -> Option<i64> {
    match value {
        ConstantValue::Char(c) => Some(i64::from(c)),
        ConstantValue::I1(n) => Some(i64::from(n)),
        ConstantValue::U1(n) => Some(i64::from(n)),
        ConstantValue::I2(n) => Some(i64::from(n)),
        ConstantValue::U2(n) => Some(i64::from(n)),
        ConstantValue::I4(n) => Some(i64::from(n)),
        ConstantValue::U4(n) => Some(i64::from(n)),
        ConstantValue::I8(n) => Some(n),
        ConstantValue::U8(n) => i64::try_from(n).ok(),
        _ => None,
    }
}

/// A signature key (method name + parameter types) for interface / abstract dispatch.
/// The same key is computed for a `callvirt` target and for the implementing method,
/// so they match; the `{:?}` of the parameter list is a stable, distinct encoding.
fn sig_encode(name: &str, params: &[SigType]) -> String {
    alloc::format!("{name}|{params:?}")
}

/// Builds each type's signature-keyed method map (its virtual / interface-implementing
/// methods, including inherited, keyed by [`sig_encode`]), for dispatching `callvirt`
/// to an interface or abstract method on a value of that runtime type.
fn build_sig_methods(
    module: &mut Module,
    _asm: u8,
    type_offset: usize,
    extends: &[Token],
    virtuals: &[Vec<VirtualMethod>],
) {
    let mut memo: Vec<Option<BTreeMap<String, MethodId>>> = alloc::vec![None; extends.len()];
    for local in 0..extends.len() {
        let methods = compute_sig_methods(local, extends, virtuals, &mut memo);
        if !methods.is_empty() {
            module.set_sig_methods((type_offset + local) as u32, methods);
        }
    }
}

/// The memoized signature-keyed method map of `type_id`: its base's map plus its own
/// virtual methods (a derived method's key replaces the inherited one).
fn compute_sig_methods(
    type_id: usize,
    extends: &[Token],
    virtuals: &[Vec<VirtualMethod>],
    memo: &mut [Option<BTreeMap<String, MethodId>>],
) -> BTreeMap<String, MethodId> {
    if let Some(methods) = &memo[type_id] {
        return methods.clone();
    }
    let mut methods = match base_type_id(extends[type_id], extends.len()) {
        Some(base) => compute_sig_methods(base, extends, virtuals, memo),
        None => BTreeMap::new(),
    };
    for method in &virtuals[type_id] {
        methods.insert(sig_encode(&method.name, &method.params), method.id);
    }
    memo[type_id] = Some(methods.clone());
    methods
}

/// Records each `callvirt` token's target signature key and argument count, so the
/// interpreter can dispatch interface / abstract methods (whose target may resolve to
/// no body). The target name + signature come from the MethodDef table (collected
/// during loading) or a MemberRef; `callvirt` is always on an instance, so the arg
/// count is the parameters plus `this`.
fn bind_call_targets(
    module: &mut Module,
    assembly: &Assembly,
    asm: u8,
    tokens: &BTreeSet<Token>,
    methoddef_sigs: &BTreeMap<u32, (String, Vec<SigType>)>,
) {
    for token in tokens {
        let (key, param_count) = match token.table() {
            METHOD_DEF => match methoddef_sigs.get(&token.row()) {
                Some((name, params)) => (sig_encode(name, params), params.len()),
                None => continue,
            },
            MEMBER_REF => {
                let Some(member) = assembly.member_ref(token.row()) else {
                    continue;
                };
                let name = member.name().unwrap_or("");
                let params = member
                    .method_signature()
                    .map(|sig| sig.parameters)
                    .unwrap_or_default();
                (sig_encode(name, &params), params.len())
            }
            _ => continue,
        };
        let arg_count = u16::try_from(param_count + 1).unwrap_or(u16::MAX);
        module.bind_call_target(asm, *token, key, arg_count);
    }
}

/// Builds each type's virtual method table and records each virtual method's slot,
/// following single inheritance (II.12.2): a type's table extends its base's, a
/// `newslot` method appends a slot, and an override (matched by name + parameter
/// types) replaces the inherited slot. (Abstract / interface dispatch goes through the
/// signature-keyed map instead; see [`build_sig_methods`].)
fn build_vtables(
    module: &mut Module,
    _asm: u8,
    type_offset: usize,
    extends: &[Token],
    virtuals: &[Vec<VirtualMethod>],
) {
    let mut memo: Vec<Option<Vec<VtableSlot>>> = alloc::vec![None; extends.len()];
    let mut method_slots: BTreeMap<MethodId, u32> = BTreeMap::new();
    for local in 0..extends.len() {
        let table = compute_vtable(local, extends, virtuals, &mut memo, &mut method_slots);
        if !table.is_empty() {
            module.set_vtable(
                (type_offset + local) as u32,
                table.iter().map(|slot| slot.method).collect(),
            );
        }
    }
    for (method, slot) in method_slots {
        module.bind_method_slot(method, slot);
    }
}

/// The memoized vtable of `type_id`, recursing into the base type so a derived table
/// extends its base's. Records each of this type's own virtual methods' slots.
fn compute_vtable(
    type_id: usize,
    extends: &[Token],
    virtuals: &[Vec<VirtualMethod>],
    memo: &mut [Option<Vec<VtableSlot>>],
    method_slots: &mut BTreeMap<MethodId, u32>,
) -> Vec<VtableSlot> {
    if let Some(table) = &memo[type_id] {
        return table.clone();
    }
    let mut table = match base_type_id(extends[type_id], extends.len()) {
        Some(base) => compute_vtable(base, extends, virtuals, memo, method_slots),
        None => Vec::new(),
    };
    for method in &virtuals[type_id] {
        let overridden = (!method.newslot)
            .then(|| {
                table
                    .iter()
                    .position(|slot| slot.name == method.name && slot.params == method.params)
            })
            .flatten();
        let slot = match overridden {
            Some(slot) => {
                table[slot].method = method.id;
                slot as u32
            }
            None => {
                table.push(VtableSlot {
                    name: method.name.clone(),
                    params: method.params.clone(),
                    method: method.id,
                });
                (table.len() - 1) as u32
            }
        };
        method_slots.insert(method.id, slot);
    }
    memo[type_id] = Some(table.clone());
    table
}

/// The base type's id from an `extends` token: a same-assembly `TypeDef` in range
/// (its 1-based row is the type id + 1), or `None` for `System.Object` / an external
/// base (a `TypeRef`) or a nil token.
fn base_type_id(extends: Token, count: usize) -> Option<usize> {
    if extends.table() != TYPE_DEF {
        return None;
    }
    let index = (extends.row() as usize).checked_sub(1)?;
    (index < count).then_some(index)
}

/// Binds each type's `TypeDef` token to its id and records its base, so `castclass`
/// and `isinst` can resolve a target type and test the subtype relation at run time.
fn bind_types(module: &mut Module, asm: u8, type_offset: usize, extends: &[Token]) {
    for local in 0..extends.len() {
        let token = Token::new(TYPE_DEF, (local + 1) as u32);
        module.bind_type_token(asm, token, (type_offset + local) as u32);
        let base =
            base_type_id(extends[local], extends.len()).map(|base| (type_offset + base) as u32);
        module.set_type_base((type_offset + local) as u32, base);
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
        #[cfg(feature = "float")]
        [SigType::R8] => console_write_line_double,
        [SigType::Object] => console_write_line_object,
        _ => return None,
    };
    Some(intrinsic)
}

/// The parameterless `ToString()` overload binds to `intrinsic`; the formatting
/// overloads (`ToString(string)` / `ToString(IFormatProvider)`) are not modeled.
fn to_string_overload(
    intrinsic: IntrinsicFn,
    signature: Option<&MethodSig>,
) -> Option<IntrinsicFn> {
    match parameters_of(signature) {
        [] => Some(intrinsic),
        _ => None,
    }
}

/// Picks the `Console.Write` overload (no line terminator) by its parameter type.
fn console_write_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
    let intrinsic: IntrinsicFn = match parameters_of(signature) {
        [SigType::String] => console_write,
        [SigType::I4] => console_write_int32,
        [SigType::I8] => console_write_int64,
        [SigType::Boolean] => console_write_bool,
        [SigType::Char] => console_write_char,
        #[cfg(feature = "float")]
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
        [SigType::Object, SigType::Object] => Some(string_concat_object2),
        [SigType::Object, SigType::Object, SigType::Object] => Some(string_concat_object3),
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

/// The .NET Micro Framework v4.4 BCL bindings beyond the Kernel Profile, gated by
/// `NETMFv4_4`: the overload pickers plus the `netmf_intrinsic` dispatch `bcl_intrinsic`
/// delegates to.
#[cfg(feature = "NETMFv4_4")]
mod netmf {
    use super::*;

    /// `String.IndexOf(char)` / `IndexOf(string)` -- the ordinal-search overloads.
    fn string_index_of_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::Char] => Some(string_index_of_char),
            [SigType::String] => Some(string_index_of_string),
            _ => None,
        }
    }

    /// `String.LastIndexOf(char)`.
    fn string_last_index_of_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::Char] => Some(string_last_index_of_char),
            _ => None,
        }
    }

    /// A one-string-argument predicate (`StartsWith` / `EndsWith` / `Contains`), ordinal.
    fn string_one_string_predicate(
        intrinsic: IntrinsicFn,
        signature: Option<&MethodSig>,
    ) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::String] => Some(intrinsic),
            _ => None,
        }
    }

    /// A parameterless string-returning transform (`ToUpper` / `ToLower` / `Trim`); the
    /// culture/char-set overloads are not modeled.
    fn string_no_arg_transform(
        intrinsic: IntrinsicFn,
        signature: Option<&MethodSig>,
    ) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [] => Some(intrinsic),
            _ => None,
        }
    }

    /// `String.Replace(char, char)` / `Replace(string, string)`.
    fn string_replace_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::Char, SigType::Char] => Some(string_replace_char),
            [SigType::String, SigType::String] => Some(string_replace_string),
            _ => None,
        }
    }

    /// `Math.Abs(int)` / `Abs(long)` -- the integer overloads (float/double need libm).
    fn math_abs_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::I4] => Some(math_abs_int32),
            [SigType::I8] => Some(math_abs_int64),
            #[cfg(feature = "float")]
            [SigType::R8] => Some(math_abs_f64),
            _ => None,
        }
    }

    /// A unary `double -> double` `Math` overload (`Floor` / `Ceiling` / `Truncate` / `Round`).
    #[cfg(feature = "float")]
    fn math_unary_f64_overload(
        intrinsic: IntrinsicFn,
        signature: Option<&MethodSig>,
    ) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::R8] => Some(intrinsic),
            _ => None,
        }
    }

    /// A binary `Math` overload (`Max` / `Min`) over two ints or two longs.
    fn math_binary_overload(
        int32: IntrinsicFn,
        int64: IntrinsicFn,
        float: Option<IntrinsicFn>,
        signature: Option<&MethodSig>,
    ) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::I4, SigType::I4] => Some(int32),
            [SigType::I8, SigType::I8] => Some(int64),
            [SigType::R8, SigType::R8] => float,
            _ => None,
        }
    }

    /// The double `Math.Max` / `Math.Min` intrinsics, present only with `float`.
    #[cfg(feature = "float")]
    const MATH_MAX_F64: Option<IntrinsicFn> = Some(math_max_f64);
    #[cfg(not(feature = "float"))]
    const MATH_MAX_F64: Option<IntrinsicFn> = None;
    #[cfg(feature = "float")]
    const MATH_MIN_F64: Option<IntrinsicFn> = Some(math_min_f64);
    #[cfg(not(feature = "float"))]
    const MATH_MIN_F64: Option<IntrinsicFn> = None;

    /// `Math.Sign(int)` / `Sign(long)` -- both return an `int`.
    fn math_sign_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::I4] => Some(math_sign_int32),
            [SigType::I8] => Some(math_sign_int64),
            #[cfg(feature = "float")]
            [SigType::R8] => Some(math_sign_f64),
            _ => None,
        }
    }

    /// A one-`char` `System.Char` method (classification or ASCII casing).
    fn char_one_arg_overload(
        intrinsic: IntrinsicFn,
        signature: Option<&MethodSig>,
    ) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::Char] => Some(intrinsic),
            _ => None,
        }
    }

    /// A single-`string`-argument static method (`Int32.Parse`, `Boolean.Parse`, ...). The
    /// format-provider / number-styles overloads are not modeled.
    fn one_string_overload(
        intrinsic: IntrinsicFn,
        signature: Option<&MethodSig>,
    ) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::String] => Some(intrinsic),
            _ => None,
        }
    }

    /// `System.Convert.ToString(value)`: dispatch to the primitive's `ToString` rendering by
    /// the argument type (each is a Kernel/base intrinsic reused for the static conversion).
    fn convert_to_string_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::I4] => Some(int32_to_string),
            [SigType::I8] => Some(int64_to_string),
            [SigType::Boolean] => Some(boolean_to_string),
            #[cfg(feature = "float")]
            [SigType::R8] => Some(double_to_string),
            [SigType::Char] => Some(char_to_string),
            _ => None,
        }
    }

    /// `String.PadLeft(int)` / `PadLeft(int, char)` (and the `PadRight` pair).
    fn string_pad_overload(
        intrinsic: IntrinsicFn,
        signature: Option<&MethodSig>,
    ) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::I4] | [SigType::I4, SigType::Char] => Some(intrinsic),
            _ => None,
        }
    }

    /// `String.Insert(int, string)`.
    fn string_insert_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::I4, SigType::String] => Some(string_insert),
            _ => None,
        }
    }

    /// `String.Remove(int)` / `Remove(int, int)`.
    fn string_remove_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::I4] | [SigType::I4, SigType::I4] => Some(string_remove),
            _ => None,
        }
    }

    /// `System.Text.StringBuilder` instance methods (NMF v4.4). The `.ctor` is handled at
    /// `newobj` (see `string_builder_ctor`); these are the instance calls.
    pub(super) fn text_intrinsic(
        type_name: &str,
        method: &str,
        signature: Option<&MethodSig>,
    ) -> Option<IntrinsicFn> {
        match (type_name, method) {
            ("StringBuilder", "Append") => string_builder_append_overload(signature),
            ("StringBuilder", "ToString") => match parameters_of(signature) {
                [] => Some(string_builder_to_string),
                _ => None,
            },
            ("StringBuilder", "get_Length") => match parameters_of(signature) {
                [] => Some(string_builder_get_length),
                _ => None,
            },
            ("StringBuilder", "Insert") => match parameters_of(signature) {
                [SigType::I4, SigType::String] => Some(string_builder_insert),
                _ => None,
            },
            ("StringBuilder", "Remove") => match parameters_of(signature) {
                [SigType::I4, SigType::I4] => Some(string_builder_remove),
                _ => None,
            },
            ("StringBuilder", "Replace") => match parameters_of(signature) {
                [SigType::Char, SigType::Char] => Some(string_builder_replace_char),
                _ => None,
            },
            _ => None,
        }
    }

    /// A `StringBuilder.Append` overload by argument type (string / char / int).
    fn string_builder_append_overload(signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
        match parameters_of(signature) {
            [SigType::String] => Some(string_builder_append_string),
            [SigType::Char] => Some(string_builder_append_char),
            [SigType::I4] => Some(string_builder_append_int),
            _ => None,
        }
    }

    /// The `get_Count` / `Contains` / `Clear` methods shared by `Stack` and `Queue` (both
    /// are array-backed, so they reuse the list intrinsics).
    fn collection_shared(method: &str, signature: Option<&MethodSig>) -> Option<IntrinsicFn> {
        match (method, parameters_of(signature)) {
            ("get_Count", []) => Some(list_get_count),
            ("Contains", [SigType::Object]) => Some(collection_contains),
            ("Clear", []) => Some(list_clear),
            _ => None,
        }
    }

    /// `System.Collections` instance methods (NMF v4.4): ArrayList, Hashtable, Stack, Queue.
    /// Each `.ctor` is handled at `newobj` (see `list_ctor`); these are the instance calls
    /// (an `Item` indexer is `get_Item` / `set_Item`).
    pub(super) fn collections_intrinsic(
        type_name: &str,
        method: &str,
        signature: Option<&MethodSig>,
    ) -> Option<IntrinsicFn> {
        match (type_name, method) {
            ("ArrayList", "Add") => match parameters_of(signature) {
                [SigType::Object] => Some(list_add),
                _ => None,
            },
            ("ArrayList", "get_Item") => match parameters_of(signature) {
                [SigType::I4] => Some(list_get_item),
                _ => None,
            },
            ("ArrayList", "set_Item") => match parameters_of(signature) {
                [SigType::I4, SigType::Object] => Some(list_set_item),
                _ => None,
            },
            ("ArrayList", "get_Count") => match parameters_of(signature) {
                [] => Some(list_get_count),
                _ => None,
            },
            ("ArrayList", "Clear") => match parameters_of(signature) {
                [] => Some(list_clear),
                _ => None,
            },
            ("ArrayList", "RemoveAt") => match parameters_of(signature) {
                [SigType::I4] => Some(list_remove_at),
                _ => None,
            },
            ("ArrayList", "Insert") => match parameters_of(signature) {
                [SigType::I4, SigType::Object] => Some(list_insert),
                _ => None,
            },
            ("Hashtable", "Add") => match parameters_of(signature) {
                [SigType::Object, SigType::Object] => Some(map_add),
                _ => None,
            },
            ("Hashtable", "get_Item") => match parameters_of(signature) {
                [SigType::Object] => Some(map_get_item),
                _ => None,
            },
            ("Hashtable", "set_Item") => match parameters_of(signature) {
                [SigType::Object, SigType::Object] => Some(map_set_item),
                _ => None,
            },
            ("Hashtable", "get_Count") => match parameters_of(signature) {
                [] => Some(map_get_count),
                _ => None,
            },
            ("Hashtable", "Contains" | "ContainsKey") => match parameters_of(signature) {
                [SigType::Object] => Some(map_contains),
                _ => None,
            },
            ("Hashtable", "Remove") => match parameters_of(signature) {
                [SigType::Object] => Some(map_remove),
                _ => None,
            },
            ("Hashtable", "Clear") => match parameters_of(signature) {
                [] => Some(list_clear),
                _ => None,
            },
            ("Stack", "Push") => match parameters_of(signature) {
                [SigType::Object] => Some(collection_push),
                _ => None,
            },
            ("Stack", "Pop") => match parameters_of(signature) {
                [] => Some(stack_pop),
                _ => None,
            },
            ("Stack", "Peek") => match parameters_of(signature) {
                [] => Some(stack_peek),
                _ => None,
            },
            ("Queue", "Enqueue") => match parameters_of(signature) {
                [SigType::Object] => Some(collection_push),
                _ => None,
            },
            ("Queue", "Dequeue") => match parameters_of(signature) {
                [] => Some(queue_dequeue),
                _ => None,
            },
            ("Queue", "Peek") => match parameters_of(signature) {
                [] => Some(queue_peek),
                _ => None,
            },
            ("Stack" | "Queue", "get_Count" | "Contains" | "Clear") => {
                collection_shared(method, signature)
            }
            _ => None,
        }
    }

    /// Resolves a NETMF v4.4 BCL member (beyond the Kernel set) to its intrinsic. Reached
    /// from `bcl_intrinsic` when the Kernel set has no match.
    pub(super) fn netmf_intrinsic(
        type_name: &str,
        method: &str,
        signature: Option<&MethodSig>,
    ) -> Option<IntrinsicFn> {
        match (type_name, method) {
            ("String", "IndexOf") => string_index_of_overload(signature),
            ("String", "LastIndexOf") => string_last_index_of_overload(signature),
            ("String", "StartsWith") => string_one_string_predicate(string_starts_with, signature),
            ("String", "EndsWith") => string_one_string_predicate(string_ends_with, signature),
            ("String", "Contains") => string_one_string_predicate(string_contains, signature),
            ("String", "ToUpper") => string_no_arg_transform(string_to_upper, signature),
            ("String", "ToLower") => string_no_arg_transform(string_to_lower, signature),
            ("String", "Trim") => string_no_arg_transform(string_trim, signature),
            ("String", "Replace") => string_replace_overload(signature),
            ("String", "PadLeft") => string_pad_overload(string_pad_left, signature),
            ("String", "PadRight") => string_pad_overload(string_pad_right, signature),
            ("String", "Insert") => string_insert_overload(signature),
            ("String", "Remove") => string_remove_overload(signature),
            ("String", "ToCharArray") => string_no_arg_transform(string_to_char_array, signature),
            ("String", "Equals") => string_one_string_predicate(string_equals, signature),
            ("String", "Split") => match parameters_of(signature) {
                [SigType::Char, SigType::ValueType(_)] => Some(string_split_char),
                _ => None,
            },
            ("String", "Join") => match parameters_of(signature) {
                [SigType::String, SigType::SzArray(element)]
                    if matches!(element.as_ref(), SigType::String) =>
                {
                    Some(string_join)
                }
                _ => None,
            },
            ("Math", "Abs") => math_abs_overload(signature),
            ("Math", "Max") => {
                math_binary_overload(math_max_int32, math_max_int64, MATH_MAX_F64, signature)
            }
            ("Math", "Min") => {
                math_binary_overload(math_min_int32, math_min_int64, MATH_MIN_F64, signature)
            }
            ("Math", "Sign") => math_sign_overload(signature),
            #[cfg(feature = "float")]
            ("Math", "Floor") => math_unary_f64_overload(math_floor_f64, signature),
            #[cfg(feature = "float")]
            ("Math", "Ceiling") => math_unary_f64_overload(math_ceiling_f64, signature),
            #[cfg(feature = "float")]
            ("Math", "Truncate") => math_unary_f64_overload(math_truncate_f64, signature),
            #[cfg(feature = "float")]
            ("Math", "Round") => math_unary_f64_overload(math_round_f64, signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Sqrt") => math_unary_f64_overload(math_sqrt_f64, signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Sin") => math_unary_f64_overload(math_sin_f64, signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Cos") => math_unary_f64_overload(math_cos_f64, signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Tan") => math_unary_f64_overload(math_tan_f64, signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Log") => math_unary_f64_overload(math_log_f64, signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Log10") => math_unary_f64_overload(math_log10_f64, signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Exp") => math_unary_f64_overload(math_exp_f64, signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Pow") => match parameters_of(signature) {
                [SigType::R8, SigType::R8] => Some(math_pow_f64),
                _ => None,
            },
            ("Char", "IsDigit") => char_one_arg_overload(char_is_digit, signature),
            ("Char", "IsLetter") => char_one_arg_overload(char_is_letter, signature),
            ("Char", "IsLetterOrDigit") => {
                char_one_arg_overload(char_is_letter_or_digit, signature)
            }
            ("Char", "IsWhiteSpace") => char_one_arg_overload(char_is_white_space, signature),
            ("Char", "IsUpper") => char_one_arg_overload(char_is_upper, signature),
            ("Char", "IsLower") => char_one_arg_overload(char_is_lower, signature),
            ("Char", "ToUpper") => char_one_arg_overload(char_to_upper, signature),
            ("Char", "ToLower") => char_one_arg_overload(char_to_lower, signature),
            ("Int32", "Parse") => one_string_overload(int32_parse, signature),
            ("Int64", "Parse") => one_string_overload(int64_parse, signature),
            ("Boolean", "Parse") => one_string_overload(boolean_parse, signature),
            ("Convert", "ToInt32") => match parameters_of(signature) {
                [SigType::String] => Some(int32_parse),
                #[cfg(feature = "float")]
                [SigType::R8] => Some(convert_to_int32_double),
                _ => None,
            },
            ("Convert", "ToInt64") => one_string_overload(int64_parse, signature),
            ("Convert", "ToBoolean") => match parameters_of(signature) {
                [SigType::String] => Some(boolean_parse),
                [SigType::I4] => Some(convert_to_boolean_int),
                _ => None,
            },
            ("Convert", "ToChar") => match parameters_of(signature) {
                [SigType::I4] => Some(convert_to_char_int),
                _ => None,
            },
            ("Convert", "ToByte") => match parameters_of(signature) {
                [SigType::I4] => Some(convert_to_byte_int),
                _ => None,
            },
            ("Convert", "ToString") => convert_to_string_overload(signature),
            _ => None,
        }
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
