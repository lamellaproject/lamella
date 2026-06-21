#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Loads an ECMA-335 assembly into a runnable [`lamella_ves`] module.

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use lamella_cil::{Opcode, Operand};
use lamella_metadata::{
    Assembly, ConstantValue, Method, MethodSig, SigType, TargetLayout, TypeName,
};
use lamella_token::Token;
use lamella_ves::intrinsics::{
    array_empty, array_get_value, array_set_value, boolean_to_string, char_to_string, console_write,
    console_write_bool, console_write_char, console_write_int32, console_write_int64,
    console_write_line, console_write_line_bool, console_write_line_char, console_write_line_empty,
    console_write_line_int32, console_write_line_int64, console_write_line_object,
    datetime_now_ticks, delegate_combine, delegate_remove, enum_is_defined, enum_parse, exception_ctor,
    exception_get_message, int32_to_string, int64_to_string, interlocked_compare_exchange,
    md_array_address, md_array_get,
    md_array_get_length, md_array_length, md_array_set, object_ctor, object_get_type,
    object_reference_equals, object_to_string,
    initialize_array, string_concat, string_concat_object2, string_concat_object3, string_concat3,
    string_equals, string_get_chars, string_get_length, string_is_null_or_empty,
    string_not_equals, string_substring, string_substring_len, type_from_handle, type_get_name,
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
    string_builder_get_capacity, string_builder_get_char, string_builder_get_length,
    string_builder_insert, string_builder_remove, string_builder_replace_char,
    string_builder_set_char, string_builder_set_length, string_builder_to_string,
    string_contains, string_ends_with,
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
#[cfg(all(feature = "NETMFv4_4", feature = "float"))]
use lamella_ves::intrinsics::{
    bitconverter_double_to_int64_bits, bitconverter_int32_bits_to_single,
    bitconverter_int64_bits_to_double, bitconverter_single_to_int32_bits,
};
#[cfg(feature = "math-transcendental")]
use lamella_ves::intrinsics::{
    math_cos_f64, math_exp_f64, math_log_f64, math_log10_f64, math_pow_f64, math_sin_f64,
    math_sqrt_f64, math_tan_f64,
};
use lamella_ves::{IntrinsicFn, MethodId, Module, TypeId, Value};

const TYPE_REF: u8 = 0x01;
const TYPE_DEF: u8 = 0x02;
const FIELD: u8 = 0x04;
const METHOD_DEF: u8 = 0x06;
const MEMBER_REF: u8 = 0x0A;
const TYPE_SPEC: u8 = 0x1B;
const METHOD_SPEC: u8 = 0x2B;

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
pub type NameIndex = BTreeMap<String, MethodId>;

/// A type index mapping a type's qualified name (`namespace.name`) to its global
/// [`crate::TypeId`]. Built while loading each assembly, it lets a cross-assembly interface
/// reference -- a `TypeRef` an implementing type names (e.g. a program class implementing
/// `[corlib]System.IComparable`) -- resolve to the defining assembly's `TypeId` by name.
pub type TypeNameIndex = BTreeMap<String, TypeId>;

/// A static-field index mapping a field's qualified name (`namespace.type.field`) to the
/// module storage slot [`Module::bind_static_field`] assigned it. Built while loading each
/// assembly (the corlib first), it lets a cross-assembly `ldsfld`/`stsfld` -- a `MemberRef`
/// a program names (e.g. `[corlib]System.BitConverter::IsLittleEndian`) -- resolve to the
/// defining assembly's storage slot by name, so the program's token and the corlib's own
/// `FieldDef` token share one slot (the corlib `.cctor` writes it, the program reads it).
/// Mirrors [`TypeNameIndex`], keyed by `namespace.type.field` instead of `namespace.type`.
type FieldNameIndex = BTreeMap<String, usize>;

/// The qualified key (`namespace.name`) for a type, matching across assemblies: a program's
/// `TypeRef` to a corlib interface computes the same key the corlib's `TypeDef` did.
fn type_name_key(name: TypeName<'_>) -> String {
    alloc::format!("{}.{}", name.namespace, name.name)
}

/// The qualified key (`namespace.type.field`) for a static field, matching across assemblies:
/// a program's `ldsfld`/`stsfld` `MemberRef` (whose parent `TypeRef` gives the declaring type's
/// name, and whose member name gives the field name) computes the same key the corlib's own
/// `FieldDef` did. Keys [`FieldNameIndex`].
fn field_name_key(declaring: TypeName<'_>, field: &str) -> String {
    alloc::format!("{}.{}.{}", declaring.namespace, declaring.name, field)
}

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
    let mut type_index = TypeNameIndex::new();
    let mut field_index = FieldNameIndex::new();
    let entry = load_assembly(
        &mut module,
        assembly,
        0,
        &mut index,
        &mut type_index,
        &mut field_index,
        false,
    );
    let entry = entry.ok_or(LoadError::EntryHasNoBody)?;
    Ok(Program { module, entry })
}

/// Loads an assembly that declares no entry point (a library) into a [`Module`], binding its
/// types + methods exactly as [`load`] does but WITHOUT requiring -- or running -- an entry point.
/// The REPL emits a `/target:library` session class and invokes a named method by id (never an
/// entry), so this lets it load that image directly instead of carrying an unused dummy `Main`.
pub fn load_library(assembly: &Assembly) -> Result<Module, LoadError> {
    Ok(load_bootstrap(assembly).0)
}

/// Loads the incremental-REPL bootstrap library exactly as [`load_library`] does, but also
/// returns the name indices it built -- the method [`NameIndex`] and type [`TypeNameIndex`], each
/// keyed by qualified name. These seed a [`DeltaContext`] so a later submission delta (loaded
/// through [`load_delta`]) can resolve a cross-assembly reference into the bootstrap BY NAME -- a
/// declared type's base `System.Object::.ctor`, or the `<repl>.__Repl` the delta references. (The
/// static-field index is internal to one assembly's load and not needed across deltas.)
#[must_use]
pub fn load_bootstrap(assembly: &Assembly) -> (Module, NameIndex, TypeNameIndex) {
    let mut module = Module::new();
    let mut index = NameIndex::new();
    let mut type_index = TypeNameIndex::new();
    let mut field_index = FieldNameIndex::new();
    let _ = load_assembly(
        &mut module,
        assembly,
        0,
        &mut index,
        &mut type_index,
        &mut field_index,
        false,
    );
    (module, index, type_index)
}

/// The assembly id of the first incremental-REPL submission delta; the persistent bootstrap
/// module owns asm 0, so deltas start one past it. Each [`load_delta`] takes the NEXT slot
/// ([`DeltaContext::next_asm`]) rather than reusing one, so every loaded delta's token space
/// stays distinct and all live deltas resolve simultaneously -- the corlib/`__Repl`/delta trio
/// (and every later delta) coexist. [`crate::Module::asm_key`] is a u64 key (the assembly in the
/// high 32 bits), so up to 256 (`u8`) assemblies can be resolved at once.
const FIRST_DELTA_ASM: u8 = 1;

/// The persistent state a [`load_delta`] caller threads across submissions: the global
/// [`crate::TypeId`] of the bootstrap's `__Repl`, and the stable `field name -> instance slot`
/// map of the fields added to it so far. A submission delta references a prior field by name,
/// which this maps back to its slot; a name absent here is a NEW field the delta introduces.
pub struct DeltaContext {
    repl_type: TypeId,
    /// `__Repl` field name -> instance slot, in the stable order fields were added. The runtime
    /// contract is the field NAME (per the compiler's incremental-emit design): a delta names a
    /// persistent field by name, and the slot it occupies never moves (fields only append).
    field_slots: BTreeMap<String, u32>,
    /// The persistent method [`NameIndex`], seeded from the bootstrap ([`load_bootstrap`]) and
    /// grown by every delta. It lets a delta resolve a cross-assembly method call by name -- a
    /// declared type's base `System.Object::.ctor`, say -- the same way [`load_with_corlib`]
    /// resolves a program's call into the corlib.
    index: NameIndex,
    /// The persistent type [`TypeNameIndex`], seeded from the bootstrap and grown by every delta.
    /// A type a delta DECLARES (e.g. `Foo`) is indexed here by qualified name, so a LATER delta's
    /// `Foo` TypeRef -- its base reference, an `isinst`, etc. -- resolves to the same [`TypeId`].
    type_index: TypeNameIndex,
    /// The persistent static-field index, threaded so a delta's cross-assembly `ldsfld`/`stsfld`
    /// resolves to a prior assembly's storage slot by name (the same role it plays in
    /// [`load_with_corlib`]).
    static_field_index: FieldNameIndex,
    /// Each declared type's INSTANCE fields by qualified name (`namespace.type.field`) -> instance
    /// slot, recorded as a delta's types load. A later delta's cross-assembly instance FieldRef to
    /// one (e.g. `[decl]Foo::X`, an `ldfld`/`stfld`) resolves to the slot by name -- the instance
    /// analog of `static_field_index`, which the shared loader does not build (it handles
    /// cross-assembly method and static-field references, but not instance FieldRefs).
    instance_field_index: BTreeMap<String, u32>,
    /// The assembly id the NEXT submission delta loads under. Starts at [`FIRST_DELTA_ASM`] (one
    /// past the bootstrap's asm 0) and advances per [`load_delta`], so every delta gets a DISTINCT
    /// token space and all live deltas resolve simultaneously (the cap is 256 assemblies, the
    /// `u8` range -- [`crate::Module::asm_key`] folds the asm id into the high 32 bits of a u64).
    next_delta_asm: u8,
}

impl DeltaContext {
    /// Opens an incremental-REPL context over the bootstrap's `__Repl` type. `repl_type` is the
    /// global [`crate::TypeId`] of the (initially field-less) `<repl>.__Repl` the bootstrap
    /// loaded; the caller finds it the same way [`load`] anchors a session class -- via the
    /// declaring type of `<repl>.__Repl..ctor`. `index` / `type_index` are the bootstrap's name
    /// indices ([`load_bootstrap`]), seeding cross-assembly name resolution so a delta that
    /// declares a type can chain its `.ctor` to `[bootstrap]System.Object` and a later delta can
    /// name that type. The field maps start empty (the bootstrap `__Repl` carries no declared
    /// state, and no delta has declared a type yet); each [`load_delta`] grows them.
    #[must_use]
    pub fn new(repl_type: TypeId, index: NameIndex, type_index: TypeNameIndex) -> DeltaContext {
        DeltaContext {
            repl_type,
            field_slots: BTreeMap::new(),
            index,
            type_index,
            static_field_index: FieldNameIndex::new(),
            instance_field_index: BTreeMap::new(),
            next_delta_asm: FIRST_DELTA_ASM,
        }
    }

    /// The assembly id the NEXT [`load_delta`] will bind its delta under (one past the previous
    /// delta, starting at [`FIRST_DELTA_ASM`]). Exposed so a caller can confirm the slot a
    /// submission landed in -- e.g. asserting later submissions run at asm >= 3, past the
    /// two-assembly cap the old single-bit `asm_key` imposed.
    #[must_use]
    pub fn next_delta_asm(&self) -> u8 {
        self.next_delta_asm
    }

    /// The instance slots of the `__Repl` fields added so far (one per field added by a prior
    /// delta), so the caller can grow the live instance to match the grown type after
    /// [`load_delta`] reports new fields.
    #[must_use]
    pub fn field_count(&self) -> usize {
        self.field_slots.len()
    }
}

/// What loading one submission delta produced: the `MethodId` of its `Submit$N` (to run against
/// the persistent `__Repl` instance) and the zero-default value of each field it ADDED to
/// `__Repl`, in slot order. The caller grows the single live instance by appending these
/// defaults ([`crate::Heap::grow_instance`]) before running the method, so the new fields exist
/// on the instance the submission writes.
pub struct DeltaInfo {
    /// The `Submit$N` method to run with the persistent `__Repl` instance as its sole argument.
    pub submit: MethodId,
    /// The zero defaults of the fields this delta added to `__Repl`, in the order added (each
    /// already appended to the type's layout; the caller appends them to the live instance).
    pub new_field_defaults: Vec<Value>,
}

/// Why [`load_delta`] could not bind a submission delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaError {
    /// The delta assembly defines no `Submit$N` method (a delta must carry exactly one).
    NoSubmitMethod,
    /// The `Submit$N` method has no IL body to run.
    SubmitHasNoBody,
    /// A `__Repl` field reference in the delta could not be typed (its `MemberRef` carried no
    /// field signature), so the runtime cannot size a new field for it.
    UntypedFieldRef,
}

impl fmt::Display for DeltaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            DeltaError::NoSubmitMethod => "delta defines no Submit$N method",
            DeltaError::SubmitHasNoBody => "delta Submit$N method has no body",
            DeltaError::UntypedFieldRef => "delta __Repl field reference has no signature",
        })
    }
}

/// Loads one incremental-REPL submission `delta` into the persistent `module`, resolving its
/// references against the bootstrap's `__Repl` (and any type a prior delta declared, recorded in
/// `context`) and binding its `Submit$N` method so it can be run by id.
///
/// The delta is a standalone assembly (per `docs/repl-incremental-model.md`). At minimum it
/// carries a `Submit$N(__Repl s)` static method whose body reads/writes `__Repl` fields through
/// `MemberRef` FieldRefs and (for an expression submission) boxes its result to `object` and
/// returns it. A submission that DECLARES a type additionally carries a FULL `TypeDef` for it
/// (e.g. `Foo { int32 X; .ctor }`, in the REPL global namespace), which the loader REGISTERS into
/// the persistent module so a LATER delta can name it.
///
/// Loading is two passes:
/// 1. The shared per-assembly loader ([`load_assembly`]) runs over the delta under this delta's
///    own assembly slot ([`DeltaContext::next_delta_asm`], distinct per delta). It registers every
///    type the delta declares (field layout, vtable, `.ctor`/methods, type token, NAME index) and
///    binds `Submit$N` and all its SAME-MODULE tokens -- a `newobj`/`stfld` of a same-delta
///    declared type, and a `ldstr`. Cross-assembly METHOD calls (a declared type's base
///    `System.Object::.ctor`) and static-field references resolve by name through the persistent
///    indices the bootstrap seeded.
/// 2. A FieldRef pass binds the cross-assembly INSTANCE FieldRefs the shared loader does not
///    handle: a FieldRef whose parent names `__Repl` is matched by NAME against `context` -- a
///    known name binds to its slot; an UNKNOWN name is a NEW field, [`crate::Module::add_type_field`]
///    grows `__Repl` by it and the new default is reported in [`DeltaInfo::new_field_defaults`]; a
///    FieldRef to another declared type (e.g. `[decl]Foo::X`) binds to that field's slot by name.
///
/// The handshake "new fields = the `__Repl` references that do not resolve" still falls straight
/// out of pass 2; no side manifest is needed. The caller then grows the live instance by the
/// reported defaults and runs [`DeltaInfo::submit`].
///
/// # Errors
/// [`DeltaError::NoSubmitMethod`] / [`DeltaError::SubmitHasNoBody`] if the delta carries no
/// runnable `Submit$N`; [`DeltaError::UntypedFieldRef`] if a new `__Repl` field's `MemberRef` has
/// no signature to size it from.
pub fn load_delta(
    module: &mut Module,
    context: &mut DeltaContext,
    delta: &Assembly,
) -> Result<DeltaInfo, DeltaError> {
    let mut submit_row: Option<u32> = None;
    let mut method_row: u32 = 0;
    for type_def in delta.type_defs() {
        for method in type_def.methods() {
            method_row += 1;
            if method.name().is_some_and(|name| name.starts_with("Submit$")) {
                submit_row = Some(method_row);
            }
        }
    }
    let submit_row = submit_row.ok_or(DeltaError::NoSubmitMethod)?;

    let delta_asm = context.next_delta_asm;

    load_assembly(
        module,
        delta,
        delta_asm,
        &mut context.index,
        &mut context.type_index,
        &mut context.static_field_index,
        true,
    );

    index_instance_fields(module, delta, delta_asm, &mut context.instance_field_index);

    let submit = module
        .resolve(delta_asm, Token::new(METHOD_DEF, submit_row))
        .ok_or(DeltaError::SubmitHasNoBody)?;

    let new_field_defaults = bind_delta_field_refs(module, context, delta, delta_asm)?;

    context.next_delta_asm = context.next_delta_asm.saturating_add(1);

    Ok(DeltaInfo {
        submit,
        new_field_defaults,
    })
}

/// Indexes every INSTANCE field of every type the delta declares by qualified name
/// (`namespace.type.field`) -> instance slot, reading the slot the shared loader already bound
/// under the field's own `FieldDef` token. A later delta's cross-assembly instance FieldRef to one
/// (e.g. `[decl]Foo::X`) is then resolvable by name in [`bind_delta_field_refs`] -- the instance
/// analog of the static-field-by-name index, which the shared loader builds but the instance one
/// it does not.
fn index_instance_fields(
    module: &Module,
    delta: &Assembly,
    delta_asm: u8,
    instance_field_index: &mut BTreeMap<String, u32>,
) {
    let mut field_row: u32 = 0;
    for type_def in delta.type_defs() {
        let declaring = type_def.name();
        for field in type_def.fields() {
            field_row += 1;
            if field.is_static() {
                continue;
            }
            let token = Token::new(FIELD, field_row);
            if let (Some(declaring), Some(name), Some(slot)) =
                (declaring, field.name(), module.field_slot(delta_asm, token))
            {
                instance_field_index.insert(field_name_key(declaring, name), slot);
            }
        }
    }
}

/// Binds every cross-assembly INSTANCE FieldRef (an `ldfld`/`stfld`/`ldflda` `MemberRef`) across
/// all of the delta's method bodies, returning the zero defaults of the `__Repl` fields the delta
/// ADDED (in the order they were added). A FieldRef to `__Repl` is grown-or-bound; a FieldRef to
/// another declared type binds to that field's slot by name. A same-module FieldDef (a delta's own
/// declared-type field, e.g. `Foo::X` in the declaring delta) is already bound by [`load_assembly`]
/// and so is skipped here (it is not a `MemberRef`).
fn bind_delta_field_refs(
    module: &mut Module,
    context: &mut DeltaContext,
    delta: &Assembly,
    delta_asm: u8,
) -> Result<Vec<Value>, DeltaError> {
    let mut new_field_defaults: Vec<Value> = Vec::new();
    for type_def in delta.type_defs() {
        for method in type_def.methods() {
            let Some(body) = method.body() else {
                continue;
            };
            for instruction in body.code.iter() {
                let Operand::Token(token) = &instruction.operand else {
                    continue;
                };
                if !matches!(
                    instruction.opcode,
                    Opcode::Ldfld | Opcode::Stfld | Opcode::Ldflda
                ) || token.table() != MEMBER_REF
                {
                    continue;
                }
                if let Some(default) = bind_delta_field(module, context, delta, delta_asm, *token)? {
                    new_field_defaults.push(default);
                }
            }
        }
    }
    Ok(new_field_defaults)
}

/// Binds one `__Repl` FieldRef token (an `ldfld`/`stfld`/`ldflda` `MemberRef`) in a delta to an
/// instance slot. A FieldRef to another DECLARED type (e.g. `[decl]Foo::X`) binds to that field's
/// slot, resolved by qualified name through the persistent instance-field index. A FieldRef to
/// `__Repl` binds to its slot by field name, adding a new field to `__Repl` if the name is unknown.
/// Returns the new field's zero default when it grew `__Repl`, or `None` otherwise.
///
/// Order matters: the declared-type lookup (parent-qualified) is tried FIRST, so a `Foo::X`
/// reference never reaches the `__Repl` grow path. `__Repl`'s own fields are never in the
/// declared-type index (no delta DECLARES `__Repl`), so a `__Repl` FieldRef always falls through
/// to the by-bare-name path the incremental model defines.
fn bind_delta_field(
    module: &mut Module,
    context: &mut DeltaContext,
    delta: &Assembly,
    delta_asm: u8,
    token: Token,
) -> Result<Option<Value>, DeltaError> {
    let Some(member) = delta.member_ref(token.row()) else {
        return Ok(None);
    };
    let Some(name) = member.name() else {
        return Ok(None);
    };
    if let Some(declaring) = delta.type_token_name(member.parent()) {
        let key = field_name_key(declaring, name);
        if let Some(&slot) = context.instance_field_index.get(&key) {
            module.bind_field(delta_asm, token, slot);
            return Ok(None);
        }
    }
    if let Some(&slot) = context.field_slots.get(name) {
        module.bind_field(delta_asm, token, slot);
        return Ok(None);
    }
    let signature = member.field_type().ok_or(DeltaError::UntypedFieldRef)?;
    let default = default_field_value(Some(signature));
    let slot = module
        .add_type_field(context.repl_type, default.clone())
        .unwrap_or(0);
    module.bind_field(delta_asm, token, slot);
    context.field_slots.insert(String::from(name), slot);
    Ok(Some(default))
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
    let mut type_index = TypeNameIndex::new();
    let mut field_index = FieldNameIndex::new();
    load_assembly(
        &mut module,
        corlib,
        0,
        &mut index,
        &mut type_index,
        &mut field_index,
        true,
    );
    let entry = load_assembly(
        &mut module,
        program,
        1,
        &mut index,
        &mut type_index,
        &mut field_index,
        true,
    );
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
    type_index: &mut TypeNameIndex,
    field_index: &mut FieldNameIndex,
    resolve_external: bool,
) -> Option<MethodId> {
    let type_offset = module.type_count();
    let entry_token = assembly.image().entry_point_token();

    let mut entry = None;
    let mut string_tokens = BTreeSet::new();
    let mut bcl_call_tokens = BTreeSet::new();
    let mut newarr_tokens = BTreeSet::new();
    let mut callvirt_tokens = BTreeSet::new();
    let mut newobj_tokens = BTreeSet::new();
    let mut ldtoken_field_tokens = BTreeSet::new();
    let mut static_field_ref_tokens = BTreeSet::new();
    let mut ldtoken_type_tokens = BTreeSet::new();
    let mut type_test_tokens = BTreeSet::new();
    let mut generic_call_tokens = BTreeSet::new();
    let mut value_type_method_rows: BTreeSet<u32> = BTreeSet::new();
    let mut string_builder_ctor_rows: BTreeMap<u32, u16> = BTreeMap::new();
    let mut list_ctor_rows: BTreeMap<u32, u16> = BTreeMap::new();
    let mut sizeof_tokens: BTreeSet<Token> = BTreeSet::new();
    let mut value_type_tokens: Vec<Token> = Vec::new();
    let mut methoddef_sigs: BTreeMap<u32, (String, Vec<SigType>)> = BTreeMap::new();
    let mut type_extends: Vec<Token> = Vec::new();
    let mut type_interfaces: Vec<Vec<Token>> = Vec::new();
    let mut type_virtuals: Vec<Vec<VirtualMethod>> = Vec::new();
    let mut type_is_value_type: Vec<bool> = Vec::new();
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
                    if let (Some(declaring), Some(field_name), Some(slot)) = (
                        type_def.name(),
                        field.name(),
                        module.static_field_slot(asm, token),
                    ) {
                        field_index.insert(field_name_key(declaring, field_name), slot);
                    }
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
        if let Some(name) = type_def.name() {
            if module.string_type_id().is_none() && name.namespace == "System" && name.name == "String"
            {
                module.set_string_type_id(type_id);
            }
            type_index.insert(type_name_key(name), type_id);
            module.bind_type_name(asm, Token::new(TYPE_DEF, type_row), name.name.into());
            module.bind_type_full_name(type_id, type_name_key(name));
            if let Some(kind) = primitive_value_kind(name.namespace, name.name) {
                module.set_primitive_type_token(asm, Token::new(TYPE_DEF, type_row), &kind);
            }
        }
        for (token, _) in &own {
            module.bind_field_type(asm, *token, type_id);
        }
        own_fields.push(own);
        type_extends.push(type_def.extends());
        type_interfaces.push(type_def.interfaces().collect());

        let mut virtuals = Vec::new();
        let type_name = type_def.name();
        let is_delegate = is_delegate_type(assembly, type_def.extends());
        let is_value_type = type_def.is_value_type();
        type_is_value_type.push(is_value_type);
        if is_value_type {
            value_type_tokens.push(Token::new(TYPE_DEF, type_row));
        }
        for method in type_def.methods() {
            method_row += 1;
            if is_value_type {
                value_type_method_rows.insert(method_row);
            }
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
            if name == ".ctor" {
                if let Some(name_parts) = type_name {
                    let count = u16::try_from(params.len()).unwrap_or(0);
                    if same_assembly_string_builder_ctor(name_parts.namespace, name_parts.name) {
                        string_builder_ctor_rows.insert(method_row, count);
                    } else if same_assembly_list_ctor(name_parts.namespace, name_parts.name) {
                        list_ctor_rows.insert(method_row, count);
                    }
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
                        if method.flags() & METHOD_VIRTUAL != 0 {
                            virtuals.push(VirtualMethod {
                                id,
                                name: name.clone(),
                                params: params.clone(),
                                newslot: method.flags() & METHOD_NEWSLOT != 0,
                            });
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
                        Opcode::Call if operand.table() == METHOD_SPEC => {
                            generic_call_tokens.insert(*operand);
                        }
                        Opcode::Call if operand.table() == MEMBER_REF => {
                            bcl_call_tokens.insert(*operand);
                        }
                        Opcode::Newobj => {
                            if operand.table() == MEMBER_REF {
                                bcl_call_tokens.insert(*operand);
                            }
                            newobj_tokens.insert(*operand);
                        }
                        Opcode::Newarr => {
                            newarr_tokens.insert(*operand);
                        }
                        Opcode::Ldtoken if operand.table() == FIELD => {
                            ldtoken_field_tokens.insert(*operand);
                        }
                        Opcode::Ldsfld | Opcode::Stsfld | Opcode::Ldsflda
                            if operand.table() == MEMBER_REF =>
                        {
                            static_field_ref_tokens.insert(*operand);
                        }
                        Opcode::Ldtoken | Opcode::Constrained | Opcode::Box
                            if matches!(operand.table(), TYPE_DEF | TYPE_REF | TYPE_SPEC) =>
                        {
                            ldtoken_type_tokens.insert(*operand);
                            if matches!(instruction.opcode, Opcode::Box) {
                                type_test_tokens.insert(*operand);
                            }
                        }
                        Opcode::Castclass | Opcode::Isinst | Opcode::Box => {
                            type_test_tokens.insert(*operand);
                        }
                        Opcode::Sizeof => {
                            sizeof_tokens.insert(*operand);
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
    bind_generic_calls(assembly, module, asm, &generic_call_tokens);
    mark_value_type_ctors(module, asm, &newobj_tokens, &value_type_method_rows);
    mark_same_assembly_ctors(
        module,
        asm,
        &newobj_tokens,
        &string_builder_ctor_rows,
        &list_ctor_rows,
    );
    bind_field_rva_data(assembly, module, asm, &ldtoken_field_tokens);
    bind_static_field_refs(assembly, module, asm, field_index, &static_field_ref_tokens);
    bind_type_names(assembly, module, asm, type_index, &ldtoken_type_tokens);
    classify_type_test_tokens(assembly, module, asm, type_index, &type_test_tokens);
    bind_type_sizes(assembly, module, asm, &value_type_tokens, &sizeof_tokens);
    build_field_layouts(
        module,
        assembly,
        asm,
        type_offset,
        type_index,
        &type_extends,
        &own_fields,
    );
    build_vtables(
        module,
        assembly,
        type_offset,
        type_index,
        &type_extends,
        &type_virtuals,
    );
    build_sig_methods(module, asm, type_offset, &type_extends, &type_virtuals);
    bind_call_targets(module, assembly, asm, &callvirt_tokens, &methoddef_sigs);
    bind_explicit_overrides(module, assembly, asm, type_offset);
    bind_types(module, asm, type_offset, &type_extends, &type_is_value_type);
    bind_interfaces(
        module,
        assembly,
        asm,
        type_offset,
        type_index,
        &type_interfaces,
    );
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
                "Address" => Some(md_array_address as IntrinsicFn),
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

/// Binds recognized instantiated BCL generic-method calls (a `MethodSpec` operand) to their
/// intrinsics. Resolves the `MethodSpec` to its generic definition (a `MemberRef`), and binds
/// the recognized ones -- today `System.Array.Empty<T>()` (a `params T[]` no-argument call).
fn bind_generic_calls(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    tokens: &BTreeSet<Token>,
) {
    for token in tokens {
        let Some(method_token) = assembly.method_spec_method(*token) else {
            continue;
        };
        if method_token.table() != MEMBER_REF {
            continue;
        }
        let Some(member) = assembly.member_ref(method_token.row()) else {
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
        let recognized: Option<(IntrinsicFn, u16)> = match (
            parent_type.namespace,
            parent_type.name,
            member.name(),
        ) {
            ("System", "Array", Some("Empty")) => Some((array_empty as IntrinsicFn, 0)),
            ("System.Threading", "Interlocked", Some("CompareExchange")) => {
                Some((interlocked_compare_exchange as IntrinsicFn, 3))
            }
            _ => None,
        };
        if let Some((function, arg_count)) = recognized {
            let id = module.add_intrinsic(asm, function, arg_count);
            module.bind_token(asm, *token, id);
        }
    }
}

/// The parameter count of a `System.Text.StringBuilder` constructor, if this member is one,
/// so `newobj` can allocate a builder. Always `None` without the NETMFv4_4-profile surface that defines it.
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

/// Whether a type declared in THIS assembly is `System.Text.StringBuilder`, so a same-assembly
/// (corlib-internal) `newobj` of its `.ctor` allocates a builder. Mirrors the type recognition in
/// [`string_builder_ctor`]; the caller supplies the `.ctor` arity. `false` without the surface.
#[cfg(feature = "NETMFv4_4")]
fn same_assembly_string_builder_ctor(namespace: &str, type_name: &str) -> bool {
    namespace == "System.Text" && type_name == "StringBuilder"
}

#[cfg(not(feature = "NETMFv4_4"))]
fn same_assembly_string_builder_ctor(_namespace: &str, _type_name: &str) -> bool {
    false
}

/// The parameter count of a `System.Collections.ArrayList` constructor, if this member is one,
/// so `newobj` can allocate an empty list. Always `None` without the NETMFv4_4-profile surface.
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

/// Whether a type declared in THIS assembly is a `System.Collections` list type, so a
/// same-assembly (corlib-internal) `newobj` of its `.ctor` allocates an empty list. Mirrors the
/// type recognition in [`list_ctor`]; the caller supplies the `.ctor` arity. `false` without the
/// surface.
#[cfg(feature = "NETMFv4_4")]
fn same_assembly_list_ctor(namespace: &str, type_name: &str) -> bool {
    namespace == "System.Collections"
        && matches!(type_name, "ArrayList" | "Hashtable" | "Stack" | "Queue")
}

#[cfg(not(feature = "NETMFv4_4"))]
fn same_assembly_list_ctor(_namespace: &str, _type_name: &str) -> bool {
    false
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
        return extended::text_intrinsic(type_name, method, signature);
    }
    #[cfg(feature = "NETMFv4_4")]
    if namespace == "System.Collections" {
        return extended::collections_intrinsic(type_name, method, signature);
    }
    if namespace == "System.Runtime.CompilerServices"
        && type_name == "RuntimeHelpers"
        && method == "InitializeArray"
    {
        return Some(initialize_array);
    }
    if namespace == "System.Reflection" && type_name == "MemberInfo" && method == "get_Name" {
        return Some(type_get_name);
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
        ("Object", "GetType") => match parameters_of(signature) {
            [] => Some(object_get_type),
            _ => None,
        },
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
        ("Type", "get_Name") => Some(type_get_name),
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
        ("DateTime", "NowTicks") => match parameters_of(signature) {
            [] => Some(datetime_now_ticks),
            _ => None,
        },
        #[cfg(feature = "float")]
        ("BitConverter", "DoubleToInt64Bits") => match parameters_of(signature) {
            [SigType::R8] => Some(bitconverter_double_to_int64_bits),
            _ => None,
        },
        #[cfg(feature = "float")]
        ("BitConverter", "Int64BitsToDouble") => match parameters_of(signature) {
            [SigType::I8] => Some(bitconverter_int64_bits_to_double),
            _ => None,
        },
        #[cfg(feature = "float")]
        ("BitConverter", "SingleToInt32Bits") => match parameters_of(signature) {
            [SigType::R4] => Some(bitconverter_single_to_int32_bits),
            _ => None,
        },
        #[cfg(feature = "float")]
        ("BitConverter", "Int32BitsToSingle") => match parameters_of(signature) {
            [SigType::I4] => Some(bitconverter_int32_bits_to_single),
            _ => None,
        },
        _ => None,
    };
    if base.is_some() {
        return base;
    }
    #[cfg(feature = "NETMFv4_4")]
    {
        extended::extended_intrinsic(type_name, method, signature)
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

/// Marks each `newobj` token whose constructor is declared by a value type, so the
/// interpreter builds a struct value in place rather than a heap instance.
///
/// A same-assembly `MethodDef` token names a value type defined here (its row is in
/// `value_type_method_rows`). A `MemberRef` newobj of a value type defined in ANOTHER loaded
/// assembly (e.g. a program's `new System.DateTime(...)` against the managed corlib) must be
/// marked too -- otherwise the rvalue `new Struct(...)` allocates a heap object and the struct
/// loses value semantics (a chained `new DateTime(..).AddMonths(8).Day` then reads the wrong
/// `this`). `bind_bcl_calls` has already bound such a `MemberRef` to the defining assembly's
/// ctor [`MethodId`], so resolving it here and asking whether that method declares a value type
/// covers the cross-assembly case. A delegate / reference-type `MemberRef` ctor declares no
/// value type, so it is left for the heap path.
fn mark_value_type_ctors(
    module: &mut Module,
    asm: u8,
    newobj_tokens: &BTreeSet<Token>,
    value_type_method_rows: &BTreeSet<u32>,
) {
    for token in newobj_tokens {
        let is_value_type_ctor = if token.table() == METHOD_DEF {
            value_type_method_rows.contains(&token.row())
        } else {
            module
                .resolve(asm, *token)
                .is_some_and(|ctor| module.method_declares_value_type(ctor))
        };
        if is_value_type_ctor {
            module.mark_value_type_ctor(asm, *token);
        }
    }
}

/// Marks a same-assembly (`MethodDef`) `newobj` of a `System.Text.StringBuilder` /
/// `System.Collections` list `.ctor` declared in this assembly, so a corlib-INTERNAL
/// `new StringBuilder()` allocates a builder/list at construction. `bind_bcl_calls` already
/// covers the cross-assembly (`MemberRef`) form for a program's `newobj`; this is the
/// same-assembly analog (modelled on [`mark_value_type_ctors`]). The per-row arity was
/// captured as the type's methods were walked.
fn mark_same_assembly_ctors(
    module: &mut Module,
    asm: u8,
    newobj_tokens: &BTreeSet<Token>,
    string_builder_ctor_rows: &BTreeMap<u32, u16>,
    list_ctor_rows: &BTreeMap<u32, u16>,
) {
    for token in newobj_tokens {
        if token.table() != METHOD_DEF {
            continue;
        }
        if let Some(&params) = string_builder_ctor_rows.get(&token.row()) {
            module.mark_string_builder_ctor(asm, *token, params);
        } else if let Some(&params) = list_ctor_rows.get(&token.row()) {
            module.mark_list_ctor(asm, *token, params);
        }
    }
}

/// Binds each `ldtoken`'d field's RVA initializer bytes into the module, so
/// `RuntimeHelpers.InitializeArray` can fill a constant array literal from them.
fn bind_field_rva_data(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    field_tokens: &BTreeSet<Token>,
) {
    for token in field_tokens {
        if let Some(data) = assembly.field_rva_data(*token) {
            module.bind_field_rva(asm, *token, data);
        }
    }
}

/// Binds each cross-assembly static-field reference (an `ldsfld`/`stsfld`/`ldsflda` whose
/// operand is a `MemberRef` to a static field defined in another loaded assembly) to that
/// assembly's storage slot, resolved by qualified name through `field_index`.
///
/// The declaring type comes from the `MemberRef` parent (a `TypeRef`/`TypeDef`, named via
/// [`lamella_metadata::Assembly::type_token_name`]) and the field from the member name; the
/// pair keys `field_index` (which [`bind_static_field`] populated as the corlib loaded). The
/// program's token then shares the corlib's slot, so a `ldsfld
/// [corlib]System.BitConverter::IsLittleEndian` reads the cell the corlib `.cctor` set. A
/// const corlib field is inlined by csc (never `ldsfld`'d) and so is absent from the index;
/// an already-bound token (a same-assembly `MemberRef`, if one ever arises) is left alone.
/// Only a field `MemberRef` is considered -- a method one would not be a static-field operand.
fn bind_static_field_refs(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    field_index: &FieldNameIndex,
    tokens: &BTreeSet<Token>,
) {
    for token in tokens {
        if module.static_field_slot(asm, *token).is_some() {
            continue;
        }
        let Some(member) = assembly.member_ref(token.row()) else {
            continue;
        };
        if !member.is_field() {
            continue;
        }
        let (Some(declaring), Some(field_name)) =
            (assembly.type_token_name(member.parent()), member.name())
        else {
            continue;
        };
        if let Some(&slot) = field_index.get(&field_name_key(declaring, field_name)) {
            module.bind_static_field_ref(asm, *token, slot);
        }
    }
}

/// Records the simple (unqualified) name of each `ldtoken`'d type, so
/// `System.Type.get_Name` can render it (`typeof(int).Name` -> "Int32"). The handle the
/// intrinsic receives is the asm-folded token, matching the module's name key.
fn bind_type_names(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    type_index: &TypeNameIndex,
    type_tokens: &BTreeSet<Token>,
) {
    for token in type_tokens {
        if let Some(name) = assembly.type_token_name(*token) {
            module.bind_type_name(asm, *token, name.name.into());
            if module.type_id_of(asm, *token).is_none() {
                if let Some(id) = type_index.get(&type_name_key(name)).copied() {
                    module.bind_type_token(asm, *token, id);
                }
            }
        }
    }
}

/// Classifies each `castclass` / `isinst` / `box` type-test operand by its external
/// identity -- `System.Object` (a universal match target) or `System.String` -- so the
/// interpreter's type test on a boxed value or a heap string is precise. A same-assembly
/// declared type already resolves to a `TypeId`; only these unresolvable core targets need
/// recording.
fn classify_type_test_tokens(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    type_index: &TypeNameIndex,
    tokens: &BTreeSet<Token>,
) {
    for token in tokens {
        if let Some(name) = assembly.type_token_name(*token) {
            if name.namespace == "System" {
                match name.name {
                    "Object" => module.mark_object_type_token(asm, *token),
                    "String" => module.mark_string_type_token(asm, *token),
                    _ => {}
                }
            }
            if module.type_id_of(asm, *token).is_none() {
                if let Some(id) = type_index.get(&type_name_key(name)).copied() {
                    module.bind_type_token(asm, *token, id);
                }
            }
        }
    }
}

/// Records the byte size of every type a `sizeof` operand names (III.4.25), and of every
/// value type this assembly declares, so the interpreter's `sizeof` resolves the operand.
///
/// A value type's size is its shared [`lamella_metadata::Assembly::value_type_layout`]
/// (the one computation the AOT stack maps and the GC ref-map also consume) at the
/// 32-bit target ([`TargetLayout::ilp32`] -- our targets use a 4-byte pointer). A `sizeof`
/// operand that names a primitive (a `TypeRef`/`TypeDef` to `System.Int32` etc., which csc
/// emits only in hand-written IL since it constant-folds `sizeof(primitive)`) gets its fixed
/// width; a struct operand is already covered by the value-type pass.
fn bind_type_sizes(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    value_type_tokens: &[Token],
    sizeof_tokens: &BTreeSet<Token>,
) {
    let target = TargetLayout::ilp32();
    for token in value_type_tokens {
        if let Ok(layout) = assembly.value_type_layout(*token, &target) {
            module.set_type_size(asm, *token, layout.size);
        }
    }
    for token in sizeof_tokens {
        if module.type_size(asm, *token).is_some() {
            continue;
        }
        if let Some(size) = assembly
            .type_token_name(*token)
            .and_then(|name| primitive_type_size(name.namespace, name.name, target.pointer_size))
        {
            module.set_type_size(asm, *token, size);
        }
    }
}

/// The fixed byte width of a primitive `System` type named by `sizeof`, or `None` if the
/// name is not a primitive. Mirrors the field widths the shared layout uses, so a
/// `sizeof(int)`-style token (hand-written IL; csc folds the C# form) agrees with .NET.
fn primitive_type_size(namespace: &str, name: &str, pointer_size: u32) -> Option<u32> {
    if namespace != "System" {
        return None;
    }
    Some(match name {
        "Boolean" | "SByte" | "Byte" => 1,
        "Int16" | "UInt16" | "Char" => 2,
        "Int32" | "UInt32" | "Single" => 4,
        "Int64" | "UInt64" | "Double" => 8,
        "IntPtr" | "UIntPtr" => pointer_size,
        _ => return None,
    })
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

/// The representative evaluation-stack [`Value`] kind a `System` primitive value type loads
/// as (a zero of that kind), or `None` for a non-primitive name. Mirrors the widening in
/// [`array_element_default`] (`bool`/`char`/`int16`/... share `Value::Int32`), so the kind a
/// primitive's elements take maps back to the one primitive whose canonical token represents
/// it -- `System.Int32` for the `Int32`-kind family, etc. This keys
/// [`Module::set_primitive_type_token`] so `System.Array.GetValue` can stamp a boxed element
/// with a real value-type identity.
fn primitive_value_kind(namespace: &str, name: &str) -> Option<Value> {
    if namespace != "System" {
        return None;
    }
    Some(match name {
        "Int32" => Value::Int32(0),
        "Int64" => Value::Int64(0),
        #[cfg(feature = "float")]
        "Single" | "Double" => Value::Float(0.0),
        "IntPtr" | "UIntPtr" => Value::NativeInt(0),
        _ => return None,
    })
}

/// How a type's base resolves for instance-field layout: a same-assembly base (a local index,
/// recursed in this pass), a cross-assembly base already loaded (its full field layout inherited
/// directly), or no base. Mirrors [`BaseVtable`] -- the field layout needs the SAME cross-assembly
/// resolution the vtable does, so a program class extending a corlib class (e.g. a user exception
/// extending `System.Exception`) carries the base's instance fields ahead of its own.
enum BaseFields {
    /// A same-assembly base at this local index -- its layout is computed in this same pass.
    Local(usize),
    /// A previously loaded (cross-assembly) base's full field defaults, prepended to this type's
    /// own fields so the derived instance reserves the base's slots first.
    Extern(Vec<Value>),
    /// No (or an unresolvable) base: the layout is this type's own fields only.
    None,
}

/// Resolves type `local`'s base for field layout (see [`BaseFields`]). The same resolution
/// [`resolve_base_vtable`] uses: a same-assembly `TypeDef` base is a local index; a `TypeRef`
/// base resolves by qualified name through `type_index` (to a local index if inside this load's
/// own range, else a previously loaded type whose stored field defaults seed this layout).
fn resolve_base_fields(
    module: &Module,
    assembly: &Assembly,
    type_offset: usize,
    type_index: &TypeNameIndex,
    extends: &[Token],
    local: usize,
) -> BaseFields {
    if let Some(base) = base_type_id(extends[local], extends.len()) {
        return BaseFields::Local(base);
    }
    let extends_token = extends[local];
    if extends_token.table() != TYPE_REF {
        return BaseFields::None;
    }
    let Some(global) = assembly
        .type_token_name(extends_token)
        .and_then(|name| type_index.get(&type_name_key(name)).copied())
    else {
        return BaseFields::None;
    };
    if let Some(base_local) = (global as usize)
        .checked_sub(type_offset)
        .filter(|&i| i < extends.len() && i != local)
    {
        return BaseFields::Local(base_local);
    }
    match module.type_field_defaults(global) {
        Some(defaults) => BaseFields::Extern(defaults.to_vec()),
        None => BaseFields::None,
    }
}

/// Computes each type's full instance-field layout (base fields first, then own) and
/// binds each own field token to its cumulative slot, so a derived instance carries
/// its inherited fields at the same slots its base uses.
fn build_field_layouts(
    module: &mut Module,
    assembly: &Assembly,
    asm: u8,
    type_offset: usize,
    type_index: &TypeNameIndex,
    extends: &[Token],
    own_fields: &[Vec<(Token, Value)>],
) {
    let bases: Vec<BaseFields> = (0..extends.len())
        .map(|local| resolve_base_fields(module, assembly, type_offset, type_index, extends, local))
        .collect();
    let mut memo: Vec<Option<Vec<Value>>> = alloc::vec![None; extends.len()];
    for local in 0..extends.len() {
        let full = field_layout(local, &bases, own_fields, &mut memo);
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
    bases: &[BaseFields],
    own_fields: &[Vec<(Token, Value)>],
    memo: &mut [Option<Vec<Value>>],
) -> Vec<Value> {
    if let Some(layout) = &memo[type_id] {
        return layout.clone();
    }
    let mut layout = match &bases[type_id] {
        BaseFields::Local(base) => field_layout(*base, bases, own_fields, memo),
        BaseFields::Extern(defaults) => defaults.clone(),
        BaseFields::None => Vec::new(),
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

/// One slot of a vtable under construction: the virtual method's signature key
/// ([`sig_encode`] of name + parameter types, to match an override to the slot it overrides)
/// and the current most-derived implementation. The key (rather than name + params) is what a
/// cross-assembly base seeds into a derived vtable, so both same-assembly and cross-assembly
/// override matching compare the one stable key.
#[derive(Clone)]
struct VtableSlot {
    key: String,
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

/// Records each type's explicit interface implementations (II.22.27 `MethodImpl` / the
/// `.override` directive): the `MethodDeclaration` (an interface/virtual method) maps to the
/// `MethodBody` defined in this type. An explicit body (`int IA.Value()`) is private and
/// named after the interface, so a `callvirt` through the interface reference -- which names
/// the interface method -- cannot reach it by signature; this map provides the dispatch.
/// `type_defs()` yields rows in order, so the local index `i` is the global `type_offset + i`.
fn bind_explicit_overrides(module: &mut Module, assembly: &Assembly, asm: u8, type_offset: usize) {
    for (local, type_def) in assembly.type_defs().enumerate() {
        let type_id = (type_offset + local) as TypeId;
        for (body_token, declaration_token) in type_def.method_impls() {
            if let Some(body) = module.resolve(asm, body_token) {
                module.add_explicit_override(asm, type_id, declaration_token, body);
            }
        }
    }
}

/// How a type's base resolves for vtable construction: a same-assembly base (a local index
/// into this load's `extends`/`virtuals`, recursed through `memo`), a cross-assembly base
/// already loaded (its vtable slots inherited directly), or no base.
enum BaseVtable {
    /// A same-assembly base at this local index -- its vtable is computed in this same pass.
    Local(usize),
    /// A previously loaded (cross-assembly) base's vtable slots, to seed this type's table so
    /// its layout is inherited and this type's own newslot virtuals append after it.
    Extern(Vec<VtableSlot>),
    /// No (or an unresolvable) base: the table starts empty.
    None,
}

/// Builds each type's virtual method table and records each virtual method's slot,
/// following single inheritance (II.12.2): a type's table extends its base's, a
/// `newslot` method appends a slot, and an override (matched by signature key) replaces the
/// inherited slot. A base reached by a cross-assembly `TypeRef` (e.g. a corlib type extending
/// a previously loaded `[mscorlib]System.Object`, or a program class extending a corlib class)
/// has its already-built vtable layout inherited, so the derived type's own virtuals start
/// AFTER the base's slots (Object's Equals=0 / GetHashCode=1 / ToString=2). (Abstract /
/// interface dispatch goes through the signature-keyed map instead; see [`build_sig_methods`].)
fn build_vtables(
    module: &mut Module,
    assembly: &Assembly,
    type_offset: usize,
    type_index: &TypeNameIndex,
    extends: &[Token],
    virtuals: &[Vec<VirtualMethod>],
) {
    let bases: Vec<BaseVtable> = (0..extends.len())
        .map(|local| resolve_base_vtable(module, assembly, type_offset, type_index, extends, local))
        .collect();
    let mut memo: Vec<Option<Vec<VtableSlot>>> = alloc::vec![None; extends.len()];
    let mut visiting: Vec<bool> = alloc::vec![false; extends.len()];
    let mut method_slots: BTreeMap<MethodId, u32> = BTreeMap::new();
    for local in 0..extends.len() {
        let table = compute_vtable(local, &bases, virtuals, &mut memo, &mut visiting, &mut method_slots);
        let type_id = (type_offset + local) as u32;
        module.set_vtable_slot_keys(
            type_id,
            table
                .iter()
                .map(|slot| (slot.key.clone(), slot.method))
                .collect(),
        );
        if !table.is_empty() {
            module.set_vtable(type_id, table.iter().map(|slot| slot.method).collect());
        }
    }
    for (method, slot) in method_slots {
        module.bind_method_slot(method, slot);
    }
}

/// Resolves type `local`'s base for vtable seeding (see [`BaseVtable`]). A same-assembly
/// `TypeDef` base is a local index. A `TypeRef` base resolves by qualified name through
/// `type_index` (the same cross-assembly resolution interfaces / `castclass` use): if it lands
/// inside this load's own type range it is a local index (a same-assembly base encoded as a
/// TypeRef), otherwise it is a previously loaded type whose stored vtable slots seed this one.
fn resolve_base_vtable(
    module: &Module,
    assembly: &Assembly,
    type_offset: usize,
    type_index: &TypeNameIndex,
    extends: &[Token],
    local: usize,
) -> BaseVtable {
    if let Some(base) = base_type_id(extends[local], extends.len()) {
        return BaseVtable::Local(base);
    }
    let extends_token = extends[local];
    if extends_token.table() != TYPE_REF {
        return BaseVtable::None;
    }
    let Some(global) = assembly
        .type_token_name(extends_token)
        .and_then(|name| type_index.get(&type_name_key(name)).copied())
    else {
        return BaseVtable::None;
    };
    let local_count = extends.len();
    if let Some(base_local) = (global as usize)
        .checked_sub(type_offset)
        .filter(|&i| i < local_count && i != local)
    {
        return BaseVtable::Local(base_local);
    }
    match module.vtable_slot_keys(global) {
        Some(slots) => BaseVtable::Extern(
            slots
                .iter()
                .map(|(key, method)| VtableSlot {
                    key: key.clone(),
                    method: *method,
                })
                .collect(),
        ),
        None => BaseVtable::None,
    }
}

/// The memoized vtable of `type_id`, seeding from the base type (a same-assembly base recursed
/// here, a cross-assembly base's stored slots inherited) so a derived table extends its base's.
/// Records each of this type's own virtual methods' slots.
fn compute_vtable(
    type_id: usize,
    bases: &[BaseVtable],
    virtuals: &[Vec<VirtualMethod>],
    memo: &mut [Option<Vec<VtableSlot>>],
    visiting: &mut [bool],
    method_slots: &mut BTreeMap<MethodId, u32>,
) -> Vec<VtableSlot> {
    if let Some(table) = &memo[type_id] {
        return table.clone();
    }
    if visiting[type_id] {
        return Vec::new();
    }
    visiting[type_id] = true;
    let mut table = match &bases[type_id] {
        BaseVtable::Local(base) => {
            compute_vtable(*base, bases, virtuals, memo, visiting, method_slots)
        }
        BaseVtable::Extern(slots) => slots.clone(),
        BaseVtable::None => Vec::new(),
    };
    for method in &virtuals[type_id] {
        let key = sig_encode(&method.name, &method.params);
        let overridden = (!method.newslot)
            .then(|| table.iter().position(|slot| slot.key == key))
            .flatten();
        let slot = match overridden {
            Some(slot) => {
                table[slot].method = method.id;
                slot as u32
            }
            None => {
                table.push(VtableSlot {
                    key,
                    method: method.id,
                });
                (table.len() - 1) as u32
            }
        };
        method_slots.insert(method.id, slot);
    }
    visiting[type_id] = false;
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

/// Binds each type's `TypeDef` token to its id and records its base and value-type-ness, so
/// `castclass` / `isinst` can resolve a target type and test the subtype relation at run
/// time, and a `callvirt` to a value type's own method on a box can auto-unbox `this`.
fn bind_types(
    module: &mut Module,
    asm: u8,
    type_offset: usize,
    extends: &[Token],
    is_value_type: &[bool],
) {
    for local in 0..extends.len() {
        let token = Token::new(TYPE_DEF, (local + 1) as u32);
        let type_id = (type_offset + local) as u32;
        module.bind_type_token(asm, token, type_id);
        let base =
            base_type_id(extends[local], extends.len()).map(|base| (type_offset + base) as u32);
        module.set_type_base(type_id, base);
        module.set_type_is_value_type(type_id, is_value_type[local]);
    }
}

/// Resolves each type's implemented-interface tokens to global [`TypeId`]s and records them
/// on the module, so `castclass` / `isinst` to an interface can test the implements relation.
///
/// An interface token is a `TypeDefOrRef`: a same-assembly `TypeDef` resolves directly through
/// the module's token map; a `TypeRef` (a cross-assembly interface such as a program class's
/// `[corlib]System.IComparable`, or a same-assembly forward reference) resolves by qualified name
/// through `type_index`. A `TypeSpec` (a generic interface) has no name and is skipped.
fn bind_interfaces(
    module: &mut Module,
    assembly: &Assembly,
    asm: u8,
    type_offset: usize,
    type_index: &TypeNameIndex,
    type_interfaces: &[Vec<Token>],
) {
    for (local, interface_tokens) in type_interfaces.iter().enumerate() {
        let mut resolved = Vec::new();
        for token in interface_tokens {
            let interface_id = module.type_id_of(asm, *token).or_else(|| {
                assembly
                    .type_token_name(*token)
                    .and_then(|name| type_index.get(&type_name_key(name)).copied())
            });
            if let Some(interface_id) = interface_id {
                resolved.push(interface_id);
            }
        }
        if !resolved.is_empty() {
            module.set_type_interfaces((type_offset + local) as TypeId, resolved);
        }
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

/// The NETMFv4_4-profile BCL bindings beyond the Kernel Profile, gated by
/// `NETMFv4_4`: the overload pickers plus the `extended_intrinsic` dispatch `bcl_intrinsic`
/// delegates to.
#[cfg(feature = "NETMFv4_4")]
mod extended {
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
            ("StringBuilder", "SetLengthCore") => match parameters_of(signature) {
                [SigType::I4] => Some(string_builder_set_length),
                _ => None,
            },
            ("StringBuilder", "get_Capacity") => match parameters_of(signature) {
                [] => Some(string_builder_get_capacity),
                _ => None,
            },
            ("StringBuilder", "get_Chars") => match parameters_of(signature) {
                [SigType::I4] => Some(string_builder_get_char),
                _ => None,
            },
            ("StringBuilder", "SetCharsCore") => match parameters_of(signature) {
                [SigType::I4, SigType::Char] => Some(string_builder_set_char),
                _ => None,
            },
            ("StringBuilder", "InsertCore") => match parameters_of(signature) {
                [SigType::I4, SigType::String] => Some(string_builder_insert),
                _ => None,
            },
            ("StringBuilder", "RemoveCore") => match parameters_of(signature) {
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

    /// Resolves a NETMFv4_4-profile BCL member (beyond the Kernel set) to its intrinsic. Reached
    /// from `bcl_intrinsic` when the Kernel set has no match.
    pub(super) fn extended_intrinsic(
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
