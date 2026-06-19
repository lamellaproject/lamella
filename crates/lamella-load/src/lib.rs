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
    boolean_to_string, char_to_string, console_write, console_write_bool, console_write_char,
    console_write_double, console_write_int32, console_write_int64, console_write_line,
    console_write_line_bool, console_write_line_char, console_write_line_double,
    console_write_line_empty, console_write_line_int32, console_write_line_int64,
    console_write_line_object, delegate_combine, delegate_remove, double_to_string,
    enum_is_defined, enum_parse, exception_ctor, exception_get_message, gc_collect,
    int32_to_string, int64_to_string, object_ctor, object_to_string, reregister_finalize,
    string_concat, string_concat3, string_equals, string_get_chars, string_get_length,
    string_is_null_or_empty, string_not_equals, string_substring, string_substring_len,
    suppress_finalize, type_from_handle, wait_for_pending_finalizers,
};
use lamella_ves::{IntrinsicFn, MethodId, Module, Value};

const TYPE_REF: u8 = 0x01;
const TYPE_DEF: u8 = 0x02;
const FIELD: u8 = 0x04;
const METHOD_DEF: u8 = 0x06;
const MEMBER_REF: u8 = 0x0A;

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
                    module.bind_static_field(token, default_field_value(field.signature()));
                } else if is_enum {
                    if let (Some(name), Some(constant)) = (field.name(), field.constant()) {
                        let type_token = Token::new(TYPE_DEF, type_row).0;
                        if matches!(constant, ConstantValue::I8(_) | ConstantValue::U8(_)) {
                            module.set_enum_wide(type_token);
                        }
                        if let Some(value) = constant_as_i64(constant) {
                            module.set_enum_constant(type_token, value, name.into());
                        }
                    }
                }
                continue;
            }
            own.push((token, default_field_value(field.signature())));
        }
        let type_id = module.add_type(Vec::new());
        for (token, _) in &own {
            module.bind_field_type(*token, type_id);
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
                    module.mark_delegate_ctor(token);
                } else if name == "Invoke" {
                    let count = u16::try_from(params.len()).unwrap_or(u16::MAX);
                    module.mark_delegate_invoke(token, count);
                }
            }
            let Some(body) = method.body() else {
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
            let id = module.add_method(body, arg_count(&method));
            module.bind_token(token, id);
            module.set_method_type(id, type_id);
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

    let entry = entry.ok_or(LoadError::EntryHasNoBody)?;
    bind_strings(assembly, &mut module, &string_tokens);
    bind_bcl_calls(assembly, &mut module, &bcl_call_tokens);
    bind_array_defaults(assembly, &mut module, &newarr_tokens);
    build_field_layouts(&mut module, &type_extends, &own_fields);
    build_vtables(&mut module, &type_extends, &type_virtuals);
    build_sig_methods(&mut module, &type_extends, &type_virtuals);
    bind_call_targets(&mut module, assembly, &callvirt_tokens, &methoddef_sigs);
    bind_types(&mut module, &type_extends);
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
    let mut bound: BTreeMap<(usize, u16), MethodId> = BTreeMap::new();
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
        let id = match bound.get(&(function as usize, arg_count)) {
            Some(&id) => id,
            None => {
                let id = module.add_intrinsic(function, arg_count);
                bound.insert((function as usize, arg_count), id);
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
        ("Object", ".ctor") => object_ctor_overload(signature),
        ("Object", "Finalize") => Some(object_ctor),
        ("Exception", ".ctor") => Some(exception_ctor),
        ("Exception", "get_Message") => Some(exception_get_message),
        ("GC", "SuppressFinalize") => Some(suppress_finalize),
        ("GC", "ReRegisterForFinalize") => Some(reregister_finalize),
        ("GC", "Collect") => Some(gc_collect),
        ("GC", "WaitForPendingFinalizers") => Some(wait_for_pending_finalizers),
        ("Type", "GetTypeFromHandle") => Some(type_from_handle),
        ("Enum", "Parse") => Some(enum_parse),
        ("Enum", "IsDefined") => Some(enum_is_defined),
        ("Int32", "ToString") => to_string_overload(int32_to_string, signature),
        ("Boolean", "ToString") => to_string_overload(boolean_to_string, signature),
        ("Char", "ToString") => to_string_overload(char_to_string, signature),
        ("Int64", "ToString") => to_string_overload(int64_to_string, signature),
        ("Double", "ToString") => to_string_overload(double_to_string, signature),
        ("Object", "ToString") => to_string_overload(object_to_string, signature),
        ("Delegate", "Combine") => Some(delegate_combine),
        ("Delegate", "Remove") => Some(delegate_remove),
        _ => None,
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
fn bind_array_defaults(assembly: &Assembly, module: &mut Module, tokens: &BTreeSet<Token>) {
    for token in tokens {
        module.bind_array_default(*token, array_element_default(assembly, *token));
    }
}

/// The zero value an array's elements take (ECMA-335 III.4.20): the numeric zero of a
/// primitive element, or null for a reference element. The `newarr` operand names the
/// element type; only a `TypeRef` to a `System` primitive is non-null -- a user
/// `TypeDef`, a `TypeSpec` (array/generic), and unrecognized names are references.
fn array_element_default(assembly: &Assembly, element_type: Token) -> Value {
    if element_type.table() != TYPE_REF {
        return Value::Null;
    }
    let Some(name) = assembly
        .type_ref(element_type.row())
        .and_then(|type_ref| type_ref.name())
    else {
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
        "Single" | "Double" => Value::Float(0.0),
        "IntPtr" | "UIntPtr" => Value::NativeInt(0),
        _ => Value::Null,
    }
}

/// Computes each type's full instance-field layout (base fields first, then own) and
/// binds each own field token to its cumulative slot, so a derived instance carries
/// its inherited fields at the same slots its base uses.
fn build_field_layouts(module: &mut Module, extends: &[Token], own_fields: &[Vec<(Token, Value)>]) {
    let mut memo: Vec<Option<Vec<Value>>> = alloc::vec![None; extends.len()];
    for type_id in 0..extends.len() {
        let full = field_layout(type_id, extends, own_fields, &mut memo);
        let base_count = full.len() - own_fields[type_id].len();
        for (index, (token, _)) in own_fields[type_id].iter().enumerate() {
            module.bind_field(*token, (base_count + index) as u32);
        }
        module.set_type_field_defaults(type_id as u32, full);
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
fn build_sig_methods(module: &mut Module, extends: &[Token], virtuals: &[Vec<VirtualMethod>]) {
    let mut memo: Vec<Option<BTreeMap<String, MethodId>>> = alloc::vec![None; extends.len()];
    for type_id in 0..extends.len() {
        let methods = compute_sig_methods(type_id, extends, virtuals, &mut memo);
        if !methods.is_empty() {
            module.set_sig_methods(type_id as u32, methods);
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
        module.bind_call_target(*token, key, arg_count);
    }
}

/// Builds each type's virtual method table and records each virtual method's slot,
/// following single inheritance (II.12.2): a type's table extends its base's, a
/// `newslot` method appends a slot, and an override (matched by name + parameter
/// types) replaces the inherited slot. (Abstract / interface dispatch goes through the
/// signature-keyed map instead; see [`build_sig_methods`].)
fn build_vtables(module: &mut Module, extends: &[Token], virtuals: &[Vec<VirtualMethod>]) {
    let mut memo: Vec<Option<Vec<VtableSlot>>> = alloc::vec![None; extends.len()];
    let mut method_slots: BTreeMap<MethodId, u32> = BTreeMap::new();
    for type_id in 0..extends.len() {
        let table = compute_vtable(type_id, extends, virtuals, &mut memo, &mut method_slots);
        if !table.is_empty() {
            module.set_vtable(
                type_id as u32,
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
fn bind_types(module: &mut Module, extends: &[Token]) {
    for type_id in 0..extends.len() {
        let token = Token::new(TYPE_DEF, (type_id + 1) as u32);
        module.bind_type_token(token, type_id as u32);
        let base = base_type_id(extends[type_id], extends.len()).map(|base| base as u32);
        module.set_type_base(type_id as u32, base);
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
