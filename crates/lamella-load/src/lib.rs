#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Loads an ECMA-335 assembly into a runnable [`lamella_cil_runtime`] module.

extern crate alloc;

#[cfg(all(feature = "corlib-lazy", not(feature = "flash-image")))]
compile_error!(
    "corlib_resolution=lazy needs a FLASH-RESIDENT corlib: enable `flash-image` (which `corlib-lazy` \
     forwards) -- resolving out of a RAM-resident corlib costs more than the eager tier it replaces"
);

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use lamella_cil::{Opcode, Operand};
#[cfg(feature = "exceptions")]
use lamella_cil::EhKind;
use lamella_metadata::{
    Assembly, AttrArg, ConstantValue, Method, MethodSig, SigType, TargetLayout, TypeDef, TypeName,
    decode_custom_attribute,
};
#[cfg(feature = "exceptions")]
use lamella_metadata::exception_tag_for_name;
use lamella_token::Token;

/// Pair an intrinsic fn with its stable registry id (FNV-1a-32 of the fn's own name, computed at
/// compile time) so binding carries the id the bake needs -- NO fn-pointer comparison, which is
/// unreliable on wasm32. Invoke with the BARE fn name that matches the registry (so `stringify!`
/// yields that name; a qualified path would hash wrong).
macro_rules! intrinsic {
    ($f:ident) => {
        (
            $f as lamella_cil_runtime::module::IntrinsicFn,
            lamella_cil_runtime::intrinsic_registry::intrinsic_id(stringify!($f)),
        )
    };
}
use lamella_cil_runtime::intrinsics::{
    array_clear_range, array_clone, array_copy_range, array_empty, array_get_value, array_rank,
    array_set_value,
    boolean_to_string,
    buffer_block_copy, buffer_byte_length, char_to_string, console_write,
    decimal_add, decimal_compare, decimal_divide, decimal_multiply, decimal_remainder,
    decimal_subtract,
    console_write_bool, console_write_char, console_write_int32, console_write_int64,
    console_write_uint32, console_write_uint64,
    console_write_line, console_write_line_bool, console_write_line_char, console_write_line_empty,
    console_write_line_int32, console_write_line_int64, console_write_line_object,
    console_write_line_uint32, console_write_line_uint64, debug_write,
    clock_is_set, clock_set_ticks,
    datetime_now_ticks, delegate_combine, delegate_equals, delegate_not_equals, delegate_remove,
    environment_get_variable, environment_processor_count, environment_tick_count,
    enum_format, enum_get_name,
    enum_get_names, enum_get_values, enum_has_flag, enum_is_defined, enum_parse,
    enum_to_string_format, exception_ctor,
    exception_get_message, exception_runtime_message, int32_to_string, int64_to_string,
    interlocked_compare_exchange,
    md_array_address, md_array_get,
    md_array_get_length, md_array_length, md_array_set, object_ctor, object_get_type,
    object_reference_equals, object_to_string,
    initialize_array, get_custom_attributes, string_concat, string_concat_object2,
    string_ctor_char_array, string_ctor_char_array_range, string_ctor_char_ptr,
    string_ctor_char_ptr_range, string_ctor_char_repeat, string_get_pinnable_reference,
    string_concat_object3, string_concat3,
    string_equals, string_get_chars, string_get_length, string_is_null_or_empty,
    string_intern, string_is_interned,
    string_create_from_chars, string_not_equals, string_substring, string_substring_len,
    type_from_handle, type_get_name,
    thread_start, thread_join, thread_yield, thread_sleep, monitor_enter, monitor_exit,
    monitor_try_enter, monitor_try_enter_timeout, monitor_wait, monitor_wait_timeout,
    monitor_wait_timed_out, monitor_pulse,
    monitor_pulse_all,
    socket_connect_start, socket_connect_poll, socket_listen, socket_accept,
    socket_send, socket_recv, socket_set_recv_timeout, socket_local_port, socket_close,
    socket_udp_bind, socket_udp_send_to, socket_udp_recv_from, dns_resolve_host,
    net_is_available, net_iface_count, net_iface_oper_status, net_iface_type, net_iface_ipv4,
    net_iface_subnet, net_iface_gateway, net_iface_flags,
    tls_client_config, tls_server_config, tls_client_new, tls_server_new, tls_process,
    tls_wants_write, tls_write_tls, tls_read_tls, tls_read_plain, tls_write_plain, tls_peer_cert,
    tls_session_flags, tls_close, tls_default_stack, tls_client_config_alpn, tls_alpn_is,
    tls_exporter_key, tls_drop_key, aead_siv_encrypt, aead_siv_decrypt, aead_import_key,
    fs_open, fs_read, fs_write, fs_seek, fs_length, fs_set_length, fs_flush, fs_close,
    fs_file_exists, fs_dir_exists, fs_delete_file, fs_create_dir, fs_delete_dir, fs_move, fs_list,
    drive_names, drive_kind, drive_total_size, drive_format, drive_filesystems, drive_mount_removable,
    storage_mount_ram, storage_mount_sd_over_spi, storage_mount_sd_over_spi_bus, storage_unmount,
    storage_is_mounted,
    serial_open, serial_read, serial_write, serial_bytes_to_read, serial_bytes_to_write,
    serial_flush, serial_discard_in, serial_discard_out, serial_close,
    marshal_alloc_hglobal, marshal_free_hglobal, marshal_read_byte, marshal_read_int16,
    marshal_read_int32, marshal_read_int64, marshal_write_byte, marshal_write_int16,
    marshal_write_int32, marshal_write_int64, marshal_size_of,
    intptr_from_raw_value, intptr_to_raw_value,
    mmio_read32, mmio_write32, mmio_read8, mmio_write8, mmio_read16, mmio_write16,
};
#[cfg(feature = "gc")]
use lamella_cil_runtime::intrinsics::{gc_collect, weak_make_cell, weak_read_cell, weak_write_cell};
#[cfg(feature = "varargs")]
use lamella_cil_runtime::intrinsics::{arg_iterator_cookie, arg_iterator_get, arg_iterator_remaining};
#[cfg(feature = "finalizers")]
use lamella_cil_runtime::intrinsics::{
    reregister_finalize, suppress_finalize, wait_for_pending_finalizers,
};
#[cfg(feature = "NETMFv4_4")]
use lamella_cil_runtime::intrinsics::{
    boolean_parse, char_is_digit, char_is_letter, char_is_letter_or_digit, char_is_lower,
    activator_create_instance, assembly_full_name, assembly_get_type, assembly_get_types,
    field_get_value, field_set_value, member_get_type,
    method_invoke, method_is_abstract, method_is_final, method_is_public, method_is_static,
    method_parameter_count, method_parameter_custom_attributes, method_parameter_name,
    method_parameter_type,
    constructor_invoke, field_get_raw_constant, field_is_literal, field_is_static,
    method_is_virtual, reflect_handle_equals, reflect_handle_not_equals,
    type_get_base_type, type_get_constructor,
    char_is_upper, char_is_white_space, char_to_lower, char_to_upper, collection_contains,
    collection_push, convert_to_boolean_int, convert_to_byte_int, convert_to_char_int, int32_parse,
    int64_parse, list_add, list_clear, list_get_count, list_get_item, list_insert, list_remove_at,
    list_set_item, map_add, map_contains, map_get_count, map_get_item, map_remove, map_set_item,
    math_abs_int32, math_abs_int64, math_max_int32, math_max_int64, math_min_int32, math_min_int64,
    math_sign_int32, math_sign_int64, queue_dequeue, queue_peek, stack_peek, stack_pop,
    string_contains, string_ends_with,
    string_index_of_char, string_index_of_string, string_insert, string_join,
    string_last_index_of_char, string_pad_left, string_pad_right, string_remove,
    string_replace_char, string_replace_string, string_split_char, string_starts_with,
    string_to_char_array, string_to_lower, string_to_upper, string_trim, type_get_assembly,
    type_get_field, type_get_fields, type_get_full_name, type_get_method, type_get_methods,
    type_get_namespace, type_get_property, type_is_abstract,
    type_is_array, type_is_class,
    type_is_enum, type_is_interface, type_is_not_public, type_is_public, type_is_value_type,
};
#[cfg(feature = "NETMFv4_4")]
use lamella_cil_runtime::intrinsics::type_property_custom_attributes;
#[cfg(feature = "float")]
use lamella_cil_runtime::intrinsics::{
    console_write_double, console_write_line_double, console_write_line_single, console_write_single,
    decimal_from_double, decimal_to_double, double_parse, double_to_exponential, double_to_fixed,
    double_to_string, single_parse, single_to_exponential, single_to_fixed, single_to_string,
};
#[cfg(all(feature = "NETMFv4_4", feature = "float"))]
use lamella_cil_runtime::intrinsics::{
    bitconverter_double_to_int64_bits, bitconverter_int32_bits_to_single,
    bitconverter_int64_bits_to_double, bitconverter_single_to_int32_bits, convert_to_int32_double,
    math_abs_f64, math_ceiling_f64, math_floor_f64, math_max_f64, math_min_f64, math_round_f64,
    math_sign_f64, math_truncate_f64,
};
#[cfg(feature = "math-transcendental")]
use lamella_cil_runtime::intrinsics::{
    math_acos_f64, math_asin_f64, math_atan2_f64, math_atan_f64, math_cos_f64, math_cosh_f64,
    math_exp_f64, math_ieee_remainder_f64, math_log10_f64, math_log_base_f64, math_log_f64,
    math_pow_f64, math_sin_f64, math_sinh_f64, math_sqrt_f64, math_tan_f64, math_tanh_f64,
};
use lamella_cil_runtime::module::{
    AttrValue, BoxedPrimitive, LoadedAttribute, RawCil, VarargSite, asm_key,
};
#[cfg(feature = "NETMFv4_4")]
use lamella_cil_runtime::module::{
    MethodParam, ReflectField, ReflectMethod, ReflectType, param_attr_key,
};
use lamella_cil_runtime::{
    CastElem, CastPrim, IntrinsicFn, MethodId, Module, PInvokeParam, PInvokeReturn, PInvokeTarget,
    PrimKind, TypeId, Value,
};

pub mod monomorphize;

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
/// Cross-assembly field resolution by qualified name, for BOTH storage kinds.
///
/// A program's `MemberRef` to another assembly's field carries the program's own token, which the
/// declaring assembly never bound -- so the two are matched by NAME, exactly as methods are. Statics
/// and instance fields need separate maps because their slots mean different things (a static
/// storage cell versus an offset within an instance) and a single map would let one answer for the
/// other.
#[derive(Default)]
struct FieldNameIndex {
    /// Qualified name -> static storage slot.
    statics: BTreeMap<String, usize>,
    /// Qualified name -> instance-field slot within its declaring type's layout.
    instances: BTreeMap<String, u32>,
    /// Qualified ENUM type name -> the zero its storage takes, which is its UNDERLYING type's zero
    /// and not a null reference (ECMA-335 II.14.3: an enum IS its underlying integral type).
    ///
    /// It rides here because it answers a question about fields -- what one of this type holds
    /// before anything assigns it -- and because it has to cross assemblies for the same reason the
    /// slot maps do: a program's field can be typed with the corlib's enum, and the reference
    /// carries a name rather than anything the declaring assembly bound. Each assembly indexes its
    /// own enums before its fields are laid out, so a type declared later in the same assembly is
    /// still known when a field names it.
    enum_zeros: BTreeMap<String, Value>,
}

impl FieldNameIndex {
    fn new() -> FieldNameIndex {
        FieldNameIndex::default()
    }
}

/// The qualified key (`namespace.name`) for a type, matching across assemblies: a program's
/// `TypeRef` to a corlib interface computes the same key the corlib's `TypeDef` did.
fn type_name_key(name: TypeName<'_>) -> String {
    type_key(name.namespace, name.name)
}

/// [`type_name_key`] over an already-resolved pair, which is the form
/// [`Assembly::type_token_full_name`] answers in: a NESTED type's key carries its enclosing chain
/// where its namespace would be, and that chain is an owned `String` rather than a borrow of the
/// row. One formatter for both, so a nested key and a top-level one cannot drift apart.
fn type_key(namespace: &str, name: &str) -> String {
    alloc::format!("{namespace}.{name}")
}

/// A type's canonical FULL name -- `namespace.name`, or the BARE `name` in the global namespace
/// (no leading `.`). This is the form the exception TAG model hashes
/// (`lamella_metadata::exception_tag_for_name` omits the `.` when the namespace is empty), so a
/// type's recorded full-name tag agrees with the tag a `catch` of it computes from the same name.
/// Distinct from [`type_name_key`], which always prefixes the `.` for its self-consistent index.
fn full_type_name(name: TypeName<'_>) -> String {
    if name.namespace.is_empty() {
        name.name.into()
    } else {
        alloc::format!("{}.{}", name.namespace, name.name)
    }
}

/// The `(namespace, name)` a CROSS-ASSEMBLY reference to `type_def` arrives under.
///
/// For an ordinary type this is just its own pair. For a NESTED type it is not: a nested
/// `TypeDef`'s metadata namespace is EMPTY, because its enclosing type lives in `NestedClass`
/// (II.22.32) rather than in its name. Keying one by that empty namespace makes every same-named
/// nested type in the assembly share a key -- `Widget.Nested` and `Gadget.Nested` both reduce to
/// `.Nested` -- so the index silently keeps whichever was walked last.
///
/// A reference to a nested type arrives carrying the enclosing chain, so the enclosing type's FULL
/// name stands in for the namespace here. `Widget.Nested` keys as `Lamella.Checks.Widget` +
/// `Nested`, which both MATCHES the incoming reference and separates the two -- the collision is
/// answered by construction rather than by a rule anyone has to remember.
///
/// The walk is bounded: a malformed cyclic `NestedClass` cannot spin here.
/// It DELEGATES, and that is the point of it now. This walk existed here and again in the binder,
/// and the `TypeRef` half -- which a reference to a nested type needs and a definition never does --
/// existed in neither. One function in `lamella-metadata` answers for both tables, so the index this
/// builds from a `TypeDef` and the lookup another assembly makes through a `TypeRef` cannot spell
/// the same type two ways.
fn key_type_name(assembly: &Assembly<'_>, type_def: &TypeDef<'_>) -> Option<(String, String)> {
    assembly.type_token_full_name(type_def.token())
}

/// [`field_name_key`] over an already-resolved `(namespace, type)` pair -- the form
/// [`key_type_name`] produces for a nested type, whose namespace is not its own.
fn field_key(namespace: &str, type_name: &str, field: &str) -> String {
    alloc::format!("{namespace}.{type_name}.{field}")
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
    return_type: Option<&SigType>,
) -> String {
    let mut key = alloc::format!("{namespace}.{type_name}.{method}|");
    for param in params {
        key.push_str(&encode_sig_type(assembly, param));
        key.push(',');
    }
    if method == "op_Explicit" || method == "op_Implicit" {
        if let Some(ret) = return_type {
            key.push_str("->");
            key.push_str(&encode_sig_type(assembly, ret));
        }
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
        SigType::GenericInst {
            definition,
            arguments,
        } => {
            let mut key = alloc::format!("GenericInst({}", encode_sig_type(assembly, definition));
            for argument in arguments {
                key.push(',');
                key.push_str(&encode_sig_type(assembly, argument));
            }
            key.push(')');
            key
        }
        other => alloc::format!("{other:?}"),
    }
}

#[cfg(test)]
mod encode_sig_type_tests {
    use super::{Assembly, SigType, encode_sig_type};

    /// A dispatch key must not carry a metadata TOKEN, because the two sides of a cross-assembly
    /// dispatch name the same type through different ones.
    ///
    /// THIS IS THE PROPERTY, NOT THE SPELLING. Asserting the exact string would freeze an encoding
    /// that is still open; asserting token-freedom is the thing that actually has to hold, and it
    /// is what `GenericInst` violated by falling through to the `{:?}` fallback.
    ///
    /// **IT REFUSES TO PASS VACUOUSLY.** If no fixture contains a generic instantiation in any
    /// signature, the property is trivially true and the test would be measuring nothing -- so
    /// finding zero is a FAILURE, not a skip.
    #[test]
    fn a_generic_instantiation_encodes_without_a_token() {
        let dir = alloc::format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("fixtures absent; skipping");
            return;
        };
        let mut seen = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("dll") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(assembly) = Assembly::read(&bytes) else {
                continue;
            };
            seen += generic_instantiations_encode_by_name(&assembly);
        }
        assert!(
            seen > 0,
            "no fixture carries a generic instantiation in any signature, so this test asserted \
             nothing -- add one rather than letting it pass vacuously"
        );
    }

    /// Encodes every generic instantiation in `assembly`'s signatures and asserts each is
    /// token-free, returning how many it checked.
    fn generic_instantiations_encode_by_name(assembly: &Assembly<'_>) -> usize {
        let mut seen = 0usize;
        for type_def in assembly.type_defs() {
            for method in type_def.methods() {
                let Some(signature) = method.signature() else {
                    continue;
                };
                for parameter in signature
                    .parameters
                    .iter()
                    .chain(core::iter::once(&signature.return_type))
                {
                    if !matches!(parameter, SigType::GenericInst { .. }) {
                        continue;
                    }
                    seen += 1;
                    let key = encode_sig_type(assembly, parameter);
                    assert!(
                        !key.contains("Token"),
                        "a generic instantiation must encode by NAME, not by token -- the two sides \
                         of a cross-assembly dispatch spell the same type through different tokens \
                         and would key differently; got `{key}`"
                    );
                }
            }
        }
        seen
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

/// The assembly a RESIDENT load entry point reads from -- one whose PE outlives the [`Module`] built
/// from it, so method CIL can be borrowed in place. By default any borrow will do, but under
/// `flash-image` (XIP) the bytes must be `'static` -- borrowed straight from the flash-resident image
/// (device flash ROM, or a host `Box::leak`) so the raw CIL is never copied into RAM (see [`RawCil`]).
/// Baking the `'static` into this one alias keeps the resident signatures uniform across both builds;
/// only this definition changes, so a non-XIP build is byte-identical and `'pe` simply goes unused
/// under XIP.
///
/// A load whose PE does NOT outlive the module -- a REPL delta or bootstrap arriving over the wire --
/// takes a plain [`Assembly`] and a copying [`CilMaterializer`] instead, which is what lets those
/// bytes be freed after the load. See [`load_delta`].
#[cfg(not(feature = "flash-image"))]
pub type SourceAssembly<'pe> = Assembly<'pe>;
/// The `flash-image` (XIP) form: the PE bytes are `'static` so method CIL can be borrowed from flash.
/// See the default form above for the full rationale.
#[cfg(feature = "flash-image")]
pub type SourceAssembly<'pe> = Assembly<'static>;

/// Decides where one method's raw CIL LIVES, for a load reading a PE that is valid for `'pe`.
///
/// Residence is a property of the VALUE, not of the build ([`RawCil`]), so it cannot be settled by a
/// `cfg` -- one `flash-image` binary loads flash-resident assemblies AND wire-delivered deltas in the
/// same session. Threading the decision as a function tied to the PE's own lifetime puts the choice
/// where the evidence is: [`flash_cil`] only type-checks where `'pe` is genuinely `'static`, because a
/// `fn(&'static [u8]) -> RawCil` cannot be passed where a `fn(&'pe [u8]) -> RawCil` is wanted for a
/// shorter `'pe`. The compiler, not a comment, is what stops a borrow of freed delta bytes.
pub type CilMaterializer<'pe> = fn(&'pe [u8]) -> RawCil;

/// Materializes one method's raw CIL for [`Module::add_method`] by BORROWING the PE in place: an
/// owned `Box` copy by default (no XIP to borrow from), or -- under `flash-image` -- a zero-copy
/// `&'static` borrow of the flash-resident PE.
///
/// The `'static` on the XIP form is what makes this a FLASH residence: it is the proof that the PE
/// outlives the module, and it can only be supplied by a caller that genuinely holds flash-resident
/// bytes.
#[cfg(not(feature = "flash-image"))]
fn flash_cil(bytes: &[u8]) -> RawCil {
    bytes.to_vec().into_boxed_slice()
}
#[cfg(feature = "flash-image")]
fn flash_cil(bytes: &'static [u8]) -> RawCil {
    RawCil::Flash(bytes)
}

/// Materializes one method's raw CIL by COPYING it out of the PE, so the module owns its bodies and
/// the PE can be dropped the moment the load returns. Accepts a borrow of ANY lifetime -- that is the
/// whole point, and the reason a REPL submission need not leak its delta.
///
/// This is what the default (non-XIP) build has always done; under `flash-image` it is the [`RawCil`]
/// arm that keeps a RAM-delivered body out of the flash-borrow discipline.
#[cfg(not(feature = "flash-image"))]
fn ram_cil(bytes: &[u8]) -> RawCil {
    bytes.to_vec().into_boxed_slice()
}
#[cfg(feature = "flash-image")]
fn ram_cil(bytes: &[u8]) -> RawCil {
    RawCil::Ram(bytes.to_vec().into_boxed_slice())
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
pub fn load<'pe>(assembly: &SourceAssembly<'pe>) -> Result<Program, LoadError> {
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
        flash_cil,
        0,
        &mut index,
        &mut type_index,
        &mut field_index,
        false,
    );
    let entry = entry.ok_or(LoadError::EntryHasNoBody)?;
    module.freeze();
    Ok(Program { module, entry })
}

/// [`load`] WITHOUT the final freeze -- the bake pipeline's entry: the reachability trim
/// must scrub the still-mutable builders (so every frozen table and pool shrinks), and
/// [`Module::write_baked`] freezes afterwards. A module this returns is NOT ready to run.
///
/// # Errors
/// As [`load`].
pub fn load_unfrozen<'pe>(assembly: &SourceAssembly<'pe>) -> Result<Program, LoadError> {
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
        flash_cil,
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
pub fn load_library<'pe>(assembly: &SourceAssembly<'pe>) -> Result<Module, LoadError> {
    let mut module = Module::new();
    let mut index = NameIndex::new();
    let mut type_index = TypeNameIndex::new();
    let mut field_index = FieldNameIndex::new();
    let _ = load_assembly(
        &mut module,
        assembly,
        flash_cil,
        0,
        &mut index,
        &mut type_index,
        &mut field_index,
        false,
    );
    module.freeze();
    Ok(module)
}

/// Loads the incremental-REPL bootstrap library exactly as [`load_library`] does, but also
/// returns the name indices it built -- the method [`NameIndex`] and type [`TypeNameIndex`], each
/// keyed by qualified name. These seed a [`DeltaContext`] so a later submission delta (loaded
/// through [`load_delta`]) can resolve a cross-assembly reference into the bootstrap BY NAME -- a
/// declared type's base `System.Object::.ctor`, or the `<repl>.__Repl` the delta references. (The
/// static-field index is internal to one assembly's load and not needed across deltas.)
///
/// The bootstrap PE is the one the host emits over the wire, so it is NOT required to outlive the
/// module: its bodies are copied out ([`ram_cil`]) and the caller may drop the bytes on return.
#[must_use]
pub fn load_bootstrap<'pe>(assembly: &Assembly<'pe>) -> (Module, NameIndex, TypeNameIndex) {
    let mut module = Module::new();
    let mut index = NameIndex::new();
    let mut type_index = TypeNameIndex::new();
    let mut field_index = FieldNameIndex::new();
    let _ = load_assembly(
        &mut module,
        assembly,
        ram_cil,
        0,
        &mut index,
        &mut type_index,
        &mut field_index,
        false,
    );
    module.freeze();
    (module, index, type_index)
}

/// Like [`load_bootstrap`], but loads a resident `corlib` beneath the bootstrap first, so the
/// returned name indices resolve the FULL managed BCL surface by name: a later submission delta's
/// cross-assembly `MemberRef` to a MANAGED corlib method (e.g. `System.String::IsNullOrEmpty` -- a
/// real IL body, not a `[RuntimeProvided]` intrinsic the [`bind_bcl_calls`] fallback recognizes)
/// binds to the corlib's [`MethodId`] instead of trapping. This is the EAGER corlib-resolution
/// tier: the whole corlib is resident in RAM (~1 MiB), which desktop / WASM / roomy MCUs afford; a
/// constrained tier resolves the same members lazily from a flash-resident corlib instead. corlib
/// takes asm 0 and the bootstrap asm 1, so submissions start at asm 2 ([`DeltaContext::new_at`]).
#[must_use]
pub fn load_bootstrap_with_corlib<'c, 'b>(
    corlib: &SourceAssembly<'c>,
    bootstrap: &Assembly<'b>,
) -> (Module, NameIndex, TypeNameIndex) {
    let mut module = Module::new();
    let mut index = NameIndex::new();
    let mut type_index = TypeNameIndex::new();
    let mut field_index = FieldNameIndex::new();
    load_assembly(
        &mut module,
        corlib,
        flash_cil,
        0,
        &mut index,
        &mut type_index,
        &mut field_index,
        true,
    );
    load_assembly(
        &mut module,
        bootstrap,
        ram_cil,
        1,
        &mut index,
        &mut type_index,
        &mut field_index,
        true,
    );
    module.freeze();
    (module, index, type_index)
}

/// The assembly id of the first incremental-REPL submission delta; the persistent bootstrap
/// module owns asm 0, so deltas start one past it. Each [`load_delta`] takes the NEXT slot
/// ([`DeltaContext::next_asm`]) rather than reusing one, so every loaded delta's token space
/// stays distinct and all live deltas resolve simultaneously -- the corlib/`__Repl`/delta trio
/// (and every later delta) coexist. [`crate::Module::asm_key`] is a u64 key (the assembly in the
/// high 32 bits), so up to 256 (`u8`) assemblies can be resolved at once.
const FIRST_DELTA_ASM: u8 = 1;

/// The name-keyed state a resident corlib is resolved INTO, plus the record of what has been
/// materialized out of it so far.
///
/// It is a structure of its own because the lazy materialization walk has TWO drivers -- an
/// incremental-REPL submission ([`load_delta_with_corlib`]) and a whole program
/// ([`load_program_lazy_corlib`]) -- and only one of them has a `__Repl` type to hang session state
/// on. Everything the walk reads or writes lives here; everything specific to a persistent REPL
/// session stays in [`DeltaContext`], which owns one of these. The two drivers therefore share ONE
/// walk instead of keeping two that would have to agree.
struct CorlibResolution {
    /// The method [`NameIndex`] every loaded assembly contributes to and resolves against, so a
    /// cross-assembly call binds by name -- a declared type's base `System.Object::.ctor`, a
    /// program's call into the corlib. A lazily materialized corlib member is inserted here as it
    /// is pulled in, which is what lets the referencing assembly bind it exactly as it would bind a
    /// member of an eagerly-loaded corlib.
    index: NameIndex,
    /// The type [`TypeNameIndex`], same role for types: a type a delta DECLARES (e.g. `Foo`) is
    /// indexed here by qualified name so a LATER delta's `Foo` TypeRef -- its base reference, an
    /// `isinst` -- resolves to the same [`TypeId`], and a materialized corlib type is interned here
    /// so it is ONE session identity across every assembly that names it.
    type_index: TypeNameIndex,
    /// The cross-assembly field index, for BOTH storage kinds: a delta's or program's `ldsfld` /
    /// `ldfld` into another assembly resolves to that assembly's slot by qualified name. A
    /// materialized corlib type registers its own fields here for the same reason it registers its
    /// methods -- the referencing assembly's token and the corlib's own token are unrelated, so
    /// only the name can match them.
    field_index: FieldNameIndex,
    /// The corlib types materialized so far, by (namespace, name). When a driver introduces a
    /// dispatch key never seen before, these are the maps that must be TOPPED UP with it -- a type
    /// materialized earlier would otherwise keep the map it was built with and silently miss.
    corlib_types: Vec<(String, String)>,
    /// The type keys whose materialization is on the stack RIGHT NOW -- pushed on entry and popped
    /// on the way out, so it is empty between walks. A type is not in `type_index` until it has
    /// inherited its base's field layout, which is what makes the base recursion re-entrant, so this
    /// is what ends an `extends` chain that comes back around on itself.
    materializing: Vec<String>,
    /// Every signature key dispatched VIRTUALLY so far -- from the loaded assemblies and from the
    /// corlib bodies they pulled in. A lazily materialized type's dispatch map is populated for
    /// these keys ONLY: building the full map instead means materializing every virtual body of the
    /// type and its base chain, so merely boxing an int would drag in `ToString` and the whole
    /// format machinery behind it, which costs seconds on a constrained target. It accumulates
    /// across REPL submissions, because a type materialized by one submission may not be dispatched
    /// on until a later one.
    dispatch_keys: BTreeSet<String>,
    /// Whether the resident corlib's ENUM zeroes have been indexed into `field_index` yet.
    ///
    /// This tier never runs the eager assembly walk over the corlib, so nothing else records them,
    /// and a field typed with a corlib enum would zero to null here and to its underlying zero on
    /// the eager tier. That is a TIER DIVERGENCE, which is the one thing the two resolutions must
    /// never have -- they are meant to differ in RAM and in timing, never in an answer.
    corlib_enums_indexed: bool,
}

impl CorlibResolution {
    /// An empty resolution state -- no assembly loaded, nothing materialized.
    fn new() -> CorlibResolution {
        CorlibResolution {
            index: NameIndex::new(),
            type_index: TypeNameIndex::new(),
            field_index: FieldNameIndex::new(),
            corlib_types: Vec::new(),
            materializing: Vec::new(),
            dispatch_keys: BTreeSet::new(),
            corlib_enums_indexed: false,
        }
    }

    /// Whether `name`'s materialization is already on the stack -- the test that keeps a cyclic
    /// `extends` chain from recursing until the stack runs out.
    fn is_materializing(&self, name: TypeName<'_>) -> bool {
        self.materializing.contains(&type_name_key(name))
    }

    /// A resolution state seeded with an already-loaded assembly's name indices -- the REPL's
    /// bootstrap ([`load_bootstrap`]), whose types and methods a later delta must be able to name.
    fn seeded(index: NameIndex, type_index: TypeNameIndex) -> CorlibResolution {
        CorlibResolution { index, type_index, ..CorlibResolution::new() }
    }
}

/// The persistent state a [`load_delta`] caller threads across submissions: the global
/// [`crate::TypeId`] of the bootstrap's `__Repl`, the stable `field name -> instance slot` map of
/// the fields added to it so far, and the [`CorlibResolution`] every submission resolves through. A
/// submission delta references a prior field by name, which this maps back to its slot; a name
/// absent here is a NEW field the delta introduces.
pub struct DeltaContext {
    repl_type: TypeId,
    /// `__Repl` field name -> instance slot, in the stable order fields were added. The runtime
    /// contract is the field NAME (per the compiler's incremental-emit design): a delta names a
    /// persistent field by name, and the slot it occupies never moves (fields only append).
    field_slots: BTreeMap<String, u32>,
    /// The name resolution this session binds through, seeded from the bootstrap and grown by every
    /// delta -- and, on the lazy tier, by every corlib member a delta reaches.
    resolution: CorlibResolution,
    /// Each declared type's INSTANCE fields by qualified name (`namespace.type.field`) -> instance
    /// slot, recorded as a delta's types load. A later delta's cross-assembly instance FieldRef to
    /// one (e.g. `[decl]Foo::X`, an `ldfld`/`stfld`) resolves to the slot by name. Distinct from
    /// `resolution.field_index`, which the shared loader fills from the fields an assembly DEFINES:
    /// this one is filled from the slots the module actually bound, which is the only place a
    /// delta's own declared-type fields appear.
    instance_field_index: BTreeMap<String, u32>,
    /// The assembly id the NEXT submission delta loads under. Starts at [`FIRST_DELTA_ASM`] (one
    /// past the bootstrap's asm 0) and advances per [`load_delta`], so every delta gets a DISTINCT
    /// token space and all live deltas resolve simultaneously (the cap is 256 assemblies, the
    /// `u8` range -- [`crate::Module::asm_key`] folds the asm id into the high 32 bits of a u64).
    next_delta_asm: u8,
}

/// The dispatch keys in play during one materialization pass, threaded alongside the worklist.
/// Separate from [`DeltaContext`] so the recursive materializers can hold `&mut` on the context and
/// still read the key set (and so a re-entrant call sees the same one).
struct DispatchKeys {
    /// Every key wanted so far -- the filter a type's dispatch map is built against.
    wanted: BTreeSet<String>,
    /// Keys added since the last top-up. A corlib body materialized during this pass can introduce
    /// a key of its own (`ToUpper`'s internal `sb.ToString()`), so the pass iterates to a fixpoint.
    fresh: Vec<String>,
}

impl DispatchKeys {
    /// Records `key` as dispatched, returning whether it was new.
    fn want(&mut self, key: String) -> bool {
        if self.wanted.contains(&key) {
            return false;
        }
        self.fresh.push(key.clone());
        self.wanted.insert(key);
        true
    }
}

/// The tokens the materialized corlib bodies use, gathered so the SHARED per-assembly binder passes
/// can be run over them.
///
/// The eager loader gathers exactly these sets while walking a whole assembly and then hands them to
/// [`bind_array_defaults`], [`classify_type_test_tokens`] and the rest. The lazy tier materializes a
/// SUBSET of the corlib's bodies, so it gathers the same sets over that subset and calls the very
/// same functions. Reimplementing any of them here instead would be a second copy of a rule that has
/// to agree with the first, and the classifications are exactly the kind that fail QUIETLY -- a
/// `newarr` with no element kind recorded builds an array whose elements read back as the wrong sort
/// of value, several operations away from the array that caused it.
#[derive(Default)]
struct BodyTokens {
    /// `ldstr` user strings, whose literals are bound out of the corlib's own heap.
    strings: BTreeSet<Token>,
    /// `newarr` element types (the array's element kind, and the covariance identity of the element).
    newarr: BTreeSet<Token>,
    /// `box` types, for the primitive kind a boxed value carries.
    boxes: BTreeSet<Token>,
    /// `call` through a `MethodSpec` -- a generic instantiation.
    generic_calls: BTreeSet<Token>,
    /// `newobj` targets, for the value-type and collection-ctor markings.
    newobj: BTreeSet<Token>,
    /// `ldtoken` of a TYPE, so `Type.Name` can render it.
    ldtoken_types: BTreeSet<Token>,
    /// `ldtoken` of a FIELD, which is how an array initializer reaches its RVA data blob.
    ldtoken_fields: BTreeSet<Token>,
    /// `castclass` / `isinst` / `box` / `unbox` / `unbox.any` type identities.
    type_tests: BTreeSet<Token>,
    /// `sizeof` operands.
    sizeofs: BTreeSet<Token>,
    /// The `TypeDef` token of every materialized VALUE type, which is what gives `sizeof` a size.
    value_types: Vec<Token>,
    /// The global `MethodDef` rows a value type declares, so a `newobj` of one is marked as
    /// constructing a value rather than allocating an object.
    value_type_methods: BTreeSet<u32>,
}

/// Everything one materialization pass carries: what is left to do, and what the bodies it brings in
/// will need bound once the pass settles. Bundled rather than passed as separate arguments because
/// every step of the walk threads all of it.
struct CorlibWalk {
    /// Corlib `MethodDef` rows still to materialize.
    worklist: Vec<u32>,
    /// `MemberRef`s in materialized corlib bodies, bound against the materialized index at the end
    /// of the pass -- by then the members they name are in it.
    memberrefs: BTreeSet<Token>,
    /// The dispatch keys in play during this pass.
    dispatch: DispatchKeys,
    /// The tokens the shared binder passes consume.
    tokens: BodyTokens,
}

impl DeltaContext {
    /// The resident RAM (bytes) this context's own name indices hold, per structure -- the
    /// session-level bookkeeping that is NOT part of the module and therefore does NOT appear in
    /// [`crate::Module::heap_report`].
    ///
    /// Every submission grows these: a delta's `Submit$N` and any type it declares are recorded by
    /// QUALIFIED NAME so a later delta can resolve them, and names are the persistent contract, so
    /// nothing here is reclaimable while the session lives. Measuring a live session's footprint
    /// from the module alone therefore UNDER-REPORTS it, which is why this exists.
    ///
    /// Sizes use the same convention as `heap_report`: a `String` key costs its bytes plus a flat
    /// per-entry allowance for the allocation and the map node.
    #[must_use]
    pub fn heap_report(&self) -> Vec<(&'static str, usize)> {
        let entry = |key: &String| key.len() + 40;
        alloc::vec![
            ("session method index", self.resolution.index.keys().map(entry).sum::<usize>()),
            ("session type index", self.resolution.type_index.keys().map(entry).sum::<usize>()),
            (
                "session field indexes",
                self.field_slots.keys().map(entry).sum::<usize>()
                    + self.resolution.field_index.statics.keys().map(entry).sum::<usize>()
                    + self.resolution.field_index.instances.keys().map(entry).sum::<usize>()
                    + self.instance_field_index.keys().map(entry).sum::<usize>(),
            ),
        ]
    }

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
        DeltaContext::new_at(repl_type, index, type_index, FIRST_DELTA_ASM)
    }

    /// Like [`DeltaContext::new`], but starts the FIRST submission delta at `first_delta_asm`
    /// rather than [`FIRST_DELTA_ASM`]. A bootstrap loaded ALONE owns asm 0, so its deltas start
    /// at 1; a session opened over a resident corlib ([`load_bootstrap_with_corlib`]) puts corlib
    /// at asm 0 and the bootstrap at asm 1, so its deltas start at 2. The caller passes the
    /// assembly id one past the last resident image so every delta still gets a distinct token
    /// space (the cap is 256 assemblies, the `u8` range).
    #[must_use]
    pub fn new_at(
        repl_type: TypeId,
        index: NameIndex,
        type_index: TypeNameIndex,
        first_delta_asm: u8,
    ) -> DeltaContext {
        DeltaContext {
            repl_type,
            field_slots: BTreeMap::new(),
            resolution: CorlibResolution::seeded(index, type_index),
            instance_field_index: BTreeMap::new(),
            next_delta_asm: first_delta_asm,
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
    /// A `call` / `callvirt` / `newobj` in the delta names a member -- typically a corlib member
    /// gated out of a constrained tier's resident corlib -- that neither the lazy resolver could
    /// materialize nor an intrinsic provides. Left unbound it would trap at RUN as an opaque
    /// `UnresolvedCall`; caught at load, it is a clean REPL error carrying the qualified member name.
    UnresolvedMember(String),
}

impl fmt::Display for DeltaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeltaError::NoSubmitMethod => formatter.write_str("delta defines no Submit$N method"),
            DeltaError::SubmitHasNoBody => formatter.write_str("delta Submit$N method has no body"),
            DeltaError::UntypedFieldRef => {
                formatter.write_str("delta __Repl field reference has no signature")
            }
            DeltaError::UnresolvedMember(name) => write!(
                formatter,
                "cannot resolve {name} -- no such member in the resident corlib, and no intrinsic provides it"
            ),
        }
    }
}

/// Loads one incremental-REPL submission `delta` into the persistent `module`, resolving its
/// references against the bootstrap's `__Repl` (and any type a prior delta declared, recorded in
/// `context`) and binding its `Submit$N` method so it can be run by id.
///
/// The delta is a standalone assembly. At minimum it
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
pub fn load_delta<'pe>(
    module: &mut Module,
    context: &mut DeltaContext,
    delta: &Assembly<'pe>,
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
        ram_cil,
        delta_asm,
        &mut context.resolution.index,
        &mut context.resolution.type_index,
        &mut context.resolution.field_index,
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
    let default =
        default_field_value_of(delta, Some(signature), &context.resolution.field_index.enum_zeros);
    let slot = module
        .add_type_field(context.repl_type, default.clone())
        .unwrap_or(0);
    module.bind_field(delta_asm, token, slot);
    context.field_slots.insert(String::from(name), slot);
    Ok(Some(default))
}


/// The assembly id a lazily-materialized corlib occupies -- 0, exactly the slot the EAGER tier gives
/// corlib, so the bootstrap (asm 1) and submissions (asm 2+) lay out identically either way and a
/// delta resolves to the same ids under lazy as under eager.
const LAZY_CORLIB_ASM: u8 = 0;

/// What one instruction REACHES, for the purposes of lazy materialization.
///
/// There are two walks -- over the bodies of the assembly that REFERENCES the corlib, and over each
/// corlib body materialized in turn -- and they resolve their operands differently (by name across
/// the assembly seam, by row within the corlib). What must NOT differ is the rule for what counts as
/// reaching something, so both consult this one classification rather than each carrying a list of
/// opcodes that would have to be kept in step by hand.
enum Reaches {
    /// A `call` / `callvirt` / `newobj` target: the member itself has to resolve.
    Member,
    /// A type named directly. A reached TYPE is as much a reference as a reached member:
    /// `box System.Int32` then a `callvirt` on the box resolves its receiver type THROUGH that
    /// token, so an unmaterialized type leaves dispatch with no runtime type and the call silently
    /// keeps the base method.
    Type,
    /// A field. Its DECLARING type is what has to be materialized -- the slot the reference binds to
    /// does not exist until the type's layout does, and a static's cell is filled by the type's
    /// `.cctor`.
    Field,
    /// A `ldstr` user string, which needs its literal bound and `System.String` present.
    Text,
    /// Nothing that has to be materialized.
    Nothing,
}

/// The single classification both materialization walks consult ([`Reaches`]).
fn reaches(opcode: Opcode) -> Reaches {
    match opcode {
        Opcode::Call | Opcode::Callvirt | Opcode::Newobj => Reaches::Member,
        Opcode::Box
        | Opcode::Unbox
        | Opcode::UnboxAny
        | Opcode::Castclass
        | Opcode::Isinst
        | Opcode::Newarr
        | Opcode::Ldtoken
        | Opcode::Constrained
        | Opcode::Initobj
        | Opcode::Sizeof
        | Opcode::Ldelem
        | Opcode::Stelem
        | Opcode::Ldobj
        | Opcode::Stobj
        | Opcode::Cpobj
        | Opcode::Mkrefany
        | Opcode::Refanyval => Reaches::Type,
        Opcode::Ldsfld
        | Opcode::Stsfld
        | Opcode::Ldsflda
        | Opcode::Ldfld
        | Opcode::Stfld
        | Opcode::Ldflda => Reaches::Field,
        Opcode::Ldstr => Reaches::Text,
        _ => Reaches::Nothing,
    }
}

/// One corlib field's materialization inputs: the corlib's own metadata token for it, its NAME, and
/// its zero default value. The name is not decoration -- a referencing assembly's `MemberRef` to
/// this field carries that assembly's own token, which the corlib never bound, so the two are
/// matched by qualified NAME through [`FieldNameIndex`] exactly as methods are.
struct CorlibField {
    token: Token,
    name: String,
    default: Value,
}

/// What materializing one corlib type needs from its `TypeDef`.
struct CorlibTypeDef {
    /// The `extends` token -- row 0 when the type has no base.
    extends: Token,
    /// The interfaces it declares, as tokens to be resolved once each interface type exists.
    interfaces: Vec<Token>,
    /// Its own instance fields, in declaration order (they follow the base's layout).
    instance_fields: Vec<CorlibField>,
    /// Its own non-literal static fields.
    static_fields: Vec<CorlibField>,
}

/// Loads the incremental-REPL bootstrap for a LAZY corlib session: the bootstrap takes asm 1,
/// reserving asm 0 for corlib members materialized on demand ([`load_delta_with_corlib`]). NO corlib
/// is loaded into RAM here -- only the members a submission actually references get pulled in later,
/// from a flash-resident corlib. Submissions start at asm 2 ([`DeltaContext::new_at`]).
#[must_use]
pub fn load_bootstrap_lazy_corlib<'pe>(
    bootstrap: &Assembly<'pe>,
) -> (Module, NameIndex, TypeNameIndex) {
    let mut module = Module::new();
    let mut index = NameIndex::new();
    let mut type_index = TypeNameIndex::new();
    let mut field_index = FieldNameIndex::new();
    load_assembly(
        &mut module,
        bootstrap,
        ram_cil,
        1,
        &mut index,
        &mut type_index,
        &mut field_index,
        true,
    );
    module.freeze();
    (module, index, type_index)
}

/// The assembly id a PROGRAM takes under lazy resolution -- 1, one past [`LAZY_CORLIB_ASM`], which
/// is the same pair the eager [`load_with_corlib`] uses, so a program's tokens are keyed identically
/// whichever tier loaded it.
const LAZY_PROGRAM_ASM: u8 = 1;

/// Why a program could not be loaded against a lazily-resolved resident corlib.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LazyLoadError {
    /// The program itself is not runnable -- it names no entry point, or its entry point has no
    /// body. Nothing to do with the corlib.
    Load(LoadError),
    /// A `call` / `callvirt` / `newobj` in the program names a member -- typically a corlib member
    /// gated out of a constrained tier's resident corlib -- that neither the lazy resolver could
    /// materialize nor an intrinsic provides. Left unbound it would trap at RUN as an opaque
    /// `UnresolvedCall`, on a device with nobody watching; caught at load, it names the member while
    /// the deploying host is still listening.
    UnresolvedMember(String),
}

impl From<LoadError> for LazyLoadError {
    fn from(error: LoadError) -> LazyLoadError {
        LazyLoadError::Load(error)
    }
}

impl fmt::Display for LazyLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LazyLoadError::Load(error) => error.fmt(formatter),
            LazyLoadError::UnresolvedMember(name) => write!(
                formatter,
                "cannot resolve {name} -- no such member in the resident corlib, and no intrinsic provides it"
            ),
        }
    }
}

/// Loads a whole PROGRAM against a flash-resident `corlib`, materializing only the corlib members
/// the program reaches (and their transitive closure) -- [`load_with_corlib`]'s constrained-tier
/// twin.
///
/// The eager [`load_with_corlib`] loads the corlib whole, which costs about a megabyte of RAM and is
/// the right answer wherever RAM is ample. A 256 KB part cannot pay it, so this path keeps the corlib
/// in flash and pulls in a working set instead: the members the program's own IL names, everything
/// those bodies call in turn, the types they box or cast to, the fields they read, and each reached
/// type's `.cctor`.
///
/// It is a STRATEGY and never a semantic. The corlib takes the same assembly slot
/// ([`LAZY_CORLIB_ASM`]) and the program the same one ([`LAZY_PROGRAM_ASM`]) as under the eager
/// tier, every member is resolved by the same name key, and a materialized type is registered with
/// the same base link, value-type flag, field layout, static slot range and dispatch map the eager
/// loader would give it -- so the program must produce the same output and the same exit value
/// either way. That equivalence is what lets the roomy tier stand as a preview of the constrained
/// one.
///
/// The same walk serves an incremental-REPL submission ([`load_delta_with_corlib`]); only the
/// subject differs.
///
/// # Errors
/// [`LazyLoadError::Load`] if the program names no entry point or its entry point has no body;
/// [`LazyLoadError::UnresolvedMember`] naming the member if the program calls something the resident
/// corlib does not carry and no intrinsic provides.
pub fn load_program_lazy_corlib<'c, 'p>(
    corlib: &SourceAssembly<'c>,
    program: &SourceAssembly<'p>,
) -> Result<Program, LazyLoadError> {
    if program.image().entry_point_token() == 0 {
        return Err(LoadError::NoEntryPoint.into());
    }
    let mut module = Module::new();
    let mut resolution = CorlibResolution::new();
    #[cfg(feature = "generics")]
    let instantiations =
        monomorphize::collect_instantiations(program, core::slice::from_ref(&corlib.clone()));
    #[cfg(feature = "generics")]
    let generic_definitions: Vec<String> = {
        let mut names: Vec<String> = instantiations
            .iter()
            .map(|want| want.definition.clone())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    };
    #[cfg(not(feature = "generics"))]
    let generic_definitions: Vec<String> = Vec::new();
    #[cfg(feature = "generics")]
    let boxed_type_arguments: Vec<String> = {
        let mut names: Vec<String> = instantiations
            .iter()
            .flat_map(|want| want.arguments.iter())
            .map(monomorphize::primitive_display_name)
            .filter(|name| !name.is_empty())
            .map(|name| String::from(name))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    };
    #[cfg(not(feature = "generics"))]
    let boxed_type_arguments: Vec<String> = Vec::new();
    materialize_corlib_refs(
        &mut module,
        &mut resolution,
        program,
        corlib,
        &generic_definitions,
        &boxed_type_arguments,
    );
    let program_type_offset = module.type_count();
    let entry = load_assembly(
        &mut module,
        program,
        flash_cil,
        LAZY_PROGRAM_ASM,
        &mut resolution.index,
        &mut resolution.type_index,
        &mut resolution.field_index,
        true,
    );
    #[cfg(feature = "generics")]
    {
        let sources = alloc::vec![
            monomorphize::DefinitionSource {
                assembly: program.clone(),
                asm: LAZY_PROGRAM_ASM,
                type_offset: Some(program_type_offset),
            },
            monomorphize::DefinitionSource {
                assembly: corlib.clone(),
                asm: LAZY_CORLIB_ASM,
                type_offset: None,
            },
        ];
        monomorphize::monomorphize(
            &mut module,
            program,
            LAZY_PROGRAM_ASM,
            &sources,
            &resolution.type_index,
            &resolution.field_index,
            flash_cil,
            &instantiations,
        );
    }
    #[cfg(not(feature = "generics"))]
    let _ = program_type_offset;
    let entry = entry.ok_or(LoadError::EntryHasNoBody)?;
    if let Some(name) = first_unresolved_call(&module, program, LAZY_PROGRAM_ASM, corlib) {
        return Err(LazyLoadError::UnresolvedMember(name));
    }
    module.freeze();
    Ok(Program { module, entry })
}

/// Like [`load_delta`], but resolves the submission's references to a MANAGED corlib member (one
/// with a real IL body -- not a `[RuntimeProvided]` intrinsic the [`bind_bcl_calls`] fallback already
/// recognizes) against a flash-resident `corlib`, materializing only the members the delta reaches
/// (and their transitive closure) into the session module first. This is the LAZY corlib-resolution
/// tier: RAM tracks the working set, not the whole corlib.
///
/// # Errors
/// As [`load_delta`].
pub fn load_delta_with_corlib<'d, 'c>(
    module: &mut Module,
    context: &mut DeltaContext,
    delta: &Assembly<'d>,
    corlib: &SourceAssembly<'c>,
) -> Result<DeltaInfo, DeltaError> {
    materialize_corlib_refs(module, &mut context.resolution, delta, corlib, &[], &[]);
    let delta_asm = context.next_delta_asm;
    let info = load_delta(module, context, delta)?;
    if let Some(name) = first_unresolved_call(module, delta, delta_asm, corlib) {
        return Err(DeltaError::UnresolvedMember(name));
    }
    Ok(info)
}

/// The first `call` / `callvirt` / `newobj` MemberRef in any of `assembly`'s method bodies that
/// resolves to NOTHING in `module` after loading -- a member the lazy resolver could not materialize
/// and no intrinsic provides (a corlib member gated out of a constrained tier is the common case).
/// Returns its qualified name so the caller can reject the load LOUDLY, rather than let it trap at
/// run as an opaque `UnresolvedCall`. Only `MemberRef` targets are checked -- a same-assembly
/// `MethodDef` is bound by [`load_assembly`] and never misses -- and a `[DllImport]` P/Invoke target
/// is excluded, its absence on a device being a distinct expected trap, not a corlib miss. Nor is a
/// reference the corlib declares ABSTRACTLY ([`corlib_declares_abstract`]): that token is unbound
/// under both tiers by design, so reporting it would refuse a program the eager tier runs.
fn first_unresolved_call(
    module: &Module,
    assembly: &Assembly,
    asm: u8,
    corlib: &Assembly,
) -> Option<String> {
    for type_def in assembly.type_defs() {
        for method in type_def.methods() {
            let Some(body) = method.body() else {
                continue;
            };
            for instruction in body.code.iter() {
                if !matches!(
                    instruction.opcode,
                    Opcode::Call | Opcode::Callvirt | Opcode::Newobj
                ) {
                    continue;
                }
                let Operand::Token(token) = &instruction.operand else {
                    continue;
                };
                if token.table() != MEMBER_REF {
                    continue;
                }
                if assembly
                    .member_ref(token.row())
                    .is_some_and(|member| member.parent().table() == TYPE_SPEC)
                {
                    continue;
                }
                if module.resolve(asm, *token).is_some()
                    || module.is_delegate_ctor(asm, *token)
                    || module.delegate_invoke(asm, *token).is_some()
                    || module.pinvoke_target(asm, token.0).is_some()
                    || corlib_declares_abstract(corlib, assembly, *token)
                {
                    continue;
                }
                return Some(member_ref_display_name(assembly, *token));
            }
        }
    }
    None
}

/// A `MemberRef`'s qualified `Namespace.Type::method` name (best-effort) for a load diagnostic.
fn member_ref_display_name(assembly: &Assembly, token: Token) -> String {
    let member = assembly.member_ref(token.row());
    let name = member.as_ref().and_then(|member| member.name()).unwrap_or("<unknown>");
    match member.as_ref().and_then(|member| assembly.type_token_name(member.parent())) {
        Some(parent) if !parent.namespace.is_empty() => {
            alloc::format!("{}.{}::{}", parent.namespace, parent.name, name)
        }
        Some(parent) => alloc::format!("{}::{}", parent.name, name),
        None => String::from(name),
    }
}

/// Seeds `resolution`'s name indices with every managed corlib member `assembly` references (and
/// their transitive corlib closure), pulling each from `corlib` into the module under
/// [`LAZY_CORLIB_ASM`].
///
/// This is the ONE materialization walk, and it is driven by both lazy entry points -- a REPL
/// submission ([`load_delta_with_corlib`]) and a whole program ([`load_program_lazy_corlib`]) --
/// which is why it names its subject `assembly` rather than either shape. After it runs, the
/// subject's own [`load_assembly`] binds those references by name exactly as it would against an
/// eagerly-loaded corlib. A reference already materialized (its key is in `resolution.index`) or
/// naming a NON-corlib type is skipped, left to the ordinary load / the intrinsic fallback.
///
/// # `generic_definitions` -- the references this walk cannot reach on its own
///
/// The walk follows what the subject's IL NAMES. A generic definition the subject instantiates is
/// named through a `MemberRef` whose parent is a `TypeSpec`, which resolves to no corlib type by
/// name, so the definition is invisible here -- and the monomorphizer then COPIES that definition's
/// bodies out of `corlib` directly, bringing tokens for callees nothing materialized.
///
/// **MEASURED, by the tier-equivalence gate rather than by inspection**: with `List<T>` in the
/// corlib, the eager tier answered 42 and this one answered `UnresolvedCall`, because
/// `ArgumentOutOfRangeException::.ctor` and `ObjectArrayEnumerator::.ctor` are named only by
/// `List<T>`'s own bodies. Seeding each definition's methods here is what puts their transitive
/// closure in -- `materialize_corlib_method_row` already pushes a body's callees, so nothing new
/// walks; the set just starts in the right place.
fn materialize_corlib_refs<'c>(
    module: &mut Module,
    resolution: &mut CorlibResolution,
    assembly: &Assembly,
    corlib: &SourceAssembly<'c>,
    generic_definitions: &[String],
    boxed_type_arguments: &[String],
) {
    let mut seen: BTreeSet<Token> = BTreeSet::new();
    let mut type_tokens: Vec<Token> = Vec::new();
    let mut needs_string_type = false;
    let mut walk = CorlibWalk {
        worklist: Vec::new(),
        memberrefs: BTreeSet::new(),
        dispatch: DispatchKeys {
            wanted: core::mem::take(&mut resolution.dispatch_keys),
            fresh: Vec::new(),
        },
        tokens: BodyTokens::default(),
    };
    for type_def in assembly.type_defs() {
        for method in type_def.methods() {
            let Some(body) = method.body() else {
                continue;
            };
            for instruction in body.code.iter() {
                let Operand::Token(token) = &instruction.operand else {
                    continue;
                };
                match reaches(instruction.opcode) {
                    Reaches::Member => {
                        if token.table() == MEMBER_REF && seen.insert(*token) {
                            enqueue_corlib_ref(resolution, assembly, corlib, *token, &mut walk.worklist);
                        }
                    }
                    Reaches::Type => {
                        if seen.insert(*token) {
                            type_tokens.push(*token);
                        }
                    }
                    Reaches::Field => {
                        if token.table() == MEMBER_REF && seen.insert(*token) {
                            if let Some(parent) =
                                assembly.member_ref(token.row()).map(|member| member.parent())
                            {
                                if seen.insert(parent) {
                                    type_tokens.push(parent);
                                }
                            }
                        }
                    }
                    Reaches::Text => needs_string_type = true,
                    Reaches::Nothing => {}
                }
                if matches!(instruction.opcode, Opcode::Callvirt | Opcode::Ldvirtftn) {
                    if let Some(key) = callvirt_key(assembly, *token) {
                        walk.dispatch.want(key);
                    }
                }
            }
        }
    }
    if needs_string_type {
        materialize_string_type(
            module,
            resolution,
            corlib,
            &mut walk,
        );
    }
    for token in type_tokens {
        let Some(name) = assembly.type_token_name(token) else {
            continue;
        };
        if corlib_defines_type(corlib, name) {
            materialize_corlib_type(
                module,
                resolution,
                corlib,
                name,
                &mut walk,
            );
        }
    }
    for name in boxed_type_arguments {
        let name = TypeName { namespace: "System", name };
        if corlib_defines_type(corlib, name) {
            materialize_corlib_type(module, resolution, corlib, name, &mut walk);
        }
    }
    for definition in generic_definitions {
        let mut type_row = 0u32;
        for type_def in corlib.type_defs() {
            type_row += 1;
            let Some(name) = type_def.name() else {
                continue;
            };
            if full_type_name(name).as_str() != definition.as_str() {
                continue;
            }
            for method in type_def.methods() {
                walk.worklist.push(method.rid());
            }
            break;
        }
        let _ = type_row;
    }
    let mut cursor = 0;
    loop {
        while cursor < walk.worklist.len() {
            let row = walk.worklist[cursor];
            cursor += 1;
            materialize_corlib_method_row(
                module,
                resolution,
                corlib,
                row,
                &mut walk,
            );
        }
        let fresh = core::mem::take(&mut walk.dispatch.fresh);
        if fresh.is_empty() {
            break;
        }
        top_up_corlib_dispatch_maps(
            module,
            resolution,
            corlib,
            &fresh,
            &mut walk,
        );
        if cursor >= walk.worklist.len() && walk.dispatch.fresh.is_empty() {
            break;
        }
    }
    resolution.dispatch_keys = core::mem::take(&mut walk.dispatch.wanted);
    bind_bcl_calls(
        corlib,
        module,
        LAZY_CORLIB_ASM,
        &resolution.index,
        &resolution.type_index,
        true,
        &walk.memberrefs,
    );
    bind_materialized_body_tokens(module, resolution, corlib, &walk.tokens);
}

/// Runs the SHARED per-assembly binder passes over the tokens the materialized corlib bodies use.
///
/// These are the same functions [`load_assembly`] calls after walking an assembly whole; the only
/// difference is that the sets cover the bodies this pass brought in rather than every body in the
/// corlib. Each pass answers a question the interpreter asks at RUN time about a token -- what
/// element kind this `newarr` produces, what primitive that `box` carries, whether this `isinst`
/// names a value type, where an array initializer's bytes live -- and an unanswered one does not
/// trap. It yields a plausible wrong answer some distance from its cause, which is why these are run
/// rather than reimplemented.
fn bind_materialized_body_tokens<'c>(
    module: &mut Module,
    resolution: &CorlibResolution,
    corlib: &SourceAssembly<'c>,
    tokens: &BodyTokens,
) {
    bind_strings(corlib, module, LAZY_CORLIB_ASM, &tokens.strings);
    bind_array_defaults(
        corlib,
        module,
        LAZY_CORLIB_ASM,
        &resolution.type_index,
        &resolution.field_index.enum_zeros,
        &tokens.newarr,
    );
    bind_box_primitives(corlib, module, LAZY_CORLIB_ASM, &tokens.boxes);
    bind_generic_calls(corlib, module, LAZY_CORLIB_ASM, &tokens.generic_calls);
    mark_value_type_ctors(module, LAZY_CORLIB_ASM, &tokens.newobj, &tokens.value_type_methods);
    bind_field_rva_data(corlib, module, LAZY_CORLIB_ASM, &tokens.ldtoken_fields);
    bind_type_names(
        corlib,
        module,
        LAZY_CORLIB_ASM,
        &resolution.type_index,
        &tokens.ldtoken_types,
    );
    classify_type_test_tokens(
        corlib,
        module,
        LAZY_CORLIB_ASM,
        &resolution.type_index,
        &tokens.type_tests,
    );
    bind_type_sizes(corlib, module, LAZY_CORLIB_ASM, &tokens.value_types, &tokens.sizeofs);
}

/// The dispatch signature key of a `callvirt` / `ldvirtftn` site in `assembly` -- the same encoding
/// [`bind_call_targets`] binds to the site and [`resolve_callvirt`](lamella_cil_runtime) looks up in
/// the runtime type's map, so the two agree by construction.
fn callvirt_key(assembly: &Assembly, token: Token) -> Option<String> {
    let (name, params, arity) = match token.table() {
        MEMBER_REF => {
            let member = assembly.member_ref(token.row())?;
            let name: String = member.name()?.into();
            let signature = member.method_signature();
            let arity = signature
                .as_ref()
                .map_or(0, |signature| signature.generic_param_count);
            let params = signature.map(|signature| signature.parameters).unwrap_or_default();
            (name, params, arity)
        }
        _ => return None,
    };
    Some(sig_encode(assembly, &name, &params, arity, &[]))
}

/// Adds `fresh` keys to the dispatch map of every corlib type materialized so far. A type is
/// materialized with the keys known AT THAT MOMENT, so without this a submission that first
/// dispatches `Equals` on an `object` boxed several submissions ago would find the box's type
/// carrying the map it was born with -- and miss, silently, exactly as if the type had never been
/// materialized at all.
fn top_up_corlib_dispatch_maps<'c>(
    module: &mut Module,
    resolution: &mut CorlibResolution,
    corlib: &SourceAssembly<'c>,
    fresh: &[String],
    walk: &mut CorlibWalk,
) {
    let wanted: BTreeSet<String> = fresh.iter().cloned().collect();
    let types = resolution.corlib_types.clone();
    for (namespace, type_name) in types {
        let name = TypeName { namespace: &namespace, name: &type_name };
        let Some(&type_id) = resolution.type_index.get(&type_name_key(name)) else {
            continue;
        };
        let added = materialize_corlib_sig_methods(
            module,
            resolution,
            corlib,
            name,
            walk,
            &wanted,
        );
        for (key, method) in added {
            module.add_sig_method(type_id, &key, method);
        }
    }
}

/// Whether `corlib` DEFINES a type by this name -- the guard on materializing a type the delta names
/// by token, so a delta's own declared type (which resolves in the delta, not the corlib) is not
/// registered as a corlib type and indexed under its name.
fn corlib_defines_type(corlib: &Assembly, name: TypeName<'_>) -> bool {
    corlib.type_defs().any(|type_def| {
        type_def
            .name()
            .is_some_and(|n| n.namespace == name.namespace && n.name == name.name)
    })
}

/// If `token` (a `MemberRef` in `assembly`) names a corlib method not yet materialized, finds its
/// `MethodDef` row in `corlib` and pushes it onto `worklist`. A reference whose declaring type is not
/// in corlib, or already materialized, is ignored.
fn enqueue_corlib_ref(
    resolution: &CorlibResolution,
    assembly: &Assembly,
    corlib: &Assembly,
    token: Token,
    worklist: &mut Vec<u32>,
) {
    let Some((parent, method_name, key)) = member_ref_identity(assembly, token) else {
        return;
    };
    if resolution.index.contains_key(&key) {
        return;
    }
    if let Some(row) = find_corlib_method_row(corlib, parent.namespace, parent.name, method_name, &key)
    {
        worklist.push(row);
    }
}

/// The declaring type, method name, and cross-assembly [`name_key`] of a method `MemberRef` in
/// `assembly` -- the identity by which the DEFINING assembly's `MethodDef` is matched, since the two
/// assemblies' tokens are unrelated. Shared by the walk that materializes such a reference and the
/// guardrail that reports one it could not, so the two ask about the same member.
fn member_ref_identity<'a>(
    assembly: &Assembly<'a>,
    token: Token,
) -> Option<(TypeName<'a>, &'a str, String)> {
    let member = assembly.member_ref(token.row())?;
    let method_name = member.name()?;
    let parent = assembly.type_token_name(member.parent())?;
    let signature = member.method_signature();
    let params: Vec<SigType> = signature
        .as_ref()
        .map(|sig| sig.parameters.clone())
        .unwrap_or_default();
    let key = name_key(
        assembly,
        parent.namespace,
        parent.name,
        method_name,
        &params,
        signature.as_ref().map(|sig| &sig.return_type),
    );
    Some((parent, method_name, key))
}

/// Whether `corlib` declares the member this `MemberRef` names as an ABSTRACT or INTERFACE method:
/// present, but with no IL body and no `[RuntimeProvided]` marking either.
///
/// Such a token is unbound after loading and that is CORRECT -- there is nothing to bind. The eager
/// loader leaves it unbound too, and a `callvirt` on it reaches the implementation through the
/// receiver's runtime type (`System.IDisposable::Dispose` at the end of a `foreach` is the everyday
/// case). It has to be told apart from a member the resident corlib genuinely LACKS, and from a
/// `[RuntimeProvided]` seam whose intrinsic this build gated out -- both of those are real misses
/// that will trap at run, and both keep a body-less declaration or none at all.
fn corlib_declares_abstract(corlib: &Assembly, assembly: &Assembly, token: Token) -> bool {
    let Some((parent, method_name, key)) = member_ref_identity(assembly, token) else {
        return false;
    };
    let Some(row) = find_corlib_method_row(corlib, parent.namespace, parent.name, method_name, &key)
    else {
        return false;
    };
    let mut method_row: u32 = 0;
    for type_def in corlib.type_defs() {
        for method in type_def.methods() {
            method_row += 1;
            if method_row != row {
                continue;
            }
            let runtime_supplied = method.is_runtime_impl()
                || has_runtime_provided_attribute(corlib, Token::new(METHOD_DEF, row));
            return method.body().is_none() && !runtime_supplied;
        }
    }
    false
}

/// Scans `corlib` for the `MethodDef` row of `namespace.type_name::method` whose stable [`name_key`]
/// (encoded against corlib) equals `key` (encoded against the referencing assembly) -- an O(n) scan,
/// human-REPL-paced-fine, that a baked sorted name index later turns into a binary search. Returns
/// the global `MethodDef` row (the token row) so the caller can materialize it.
fn find_corlib_method_row(
    corlib: &Assembly,
    namespace: &str,
    type_name: &str,
    method: &str,
    key: &str,
) -> Option<u32> {
    let mut method_row: u32 = 0;
    for type_def in corlib.type_defs() {
        let matches_type = type_def
            .name()
            .is_some_and(|name| name.namespace == namespace && name.name == type_name);
        for candidate in type_def.methods() {
            method_row += 1;
            if !matches_type || candidate.name() != Some(method) {
                continue;
            }
            let signature = candidate.signature();
            let params: Vec<SigType> = signature
                .as_ref()
                .map(|sig| sig.parameters.clone())
                .unwrap_or_default();
            let candidate_key = name_key(
                corlib,
                namespace,
                type_name,
                method,
                &params,
                signature.as_ref().map(|sig| &sig.return_type),
            );
            if candidate_key == *key {
                return Some(method_row);
            }
        }
    }
    None
}

/// Resolves a MemberRef in a materialized corlib body to the corlib `MethodDef` row it names, so a
/// managed sibling accessor / helper materializes in turn. `None` when the target is not a corlib
/// method -- a `[RuntimeProvided]` accessor still binds via the intrinsic fallback in
/// [`bind_bcl_calls`], so it needs no materialized row.
fn corlib_memberref_target_row(corlib: &Assembly, token: Token) -> Option<u32> {
    let member = corlib.member_ref(token.row())?;
    let method_name = member.name()?;
    let parent = corlib.type_token_name(member.parent())?;
    let signature = member.method_signature();
    let params: Vec<SigType> = signature
        .as_ref()
        .map(|sig| sig.parameters.clone())
        .unwrap_or_default();
    let key = name_key(
        corlib,
        parent.namespace,
        parent.name,
        method_name,
        &params,
        signature.as_ref().map(|sig| &sig.return_type),
    );
    find_corlib_method_row(corlib, parent.namespace, parent.name, method_name, &key)
}

/// Materializes the corlib `MethodDef` at `method_row` into the module under [`LAZY_CORLIB_ASM`]:
/// its declaring type, then the method itself (an intrinsic if `[RuntimeProvided]`, else its managed
/// IL body borrowed from `corlib`), binding its def token and recording it in `resolution.index` so
/// both its own call sites and the referencing assembly resolve to it. A managed body's own corlib
/// callees (further `MethodDef` calls) are pushed onto `worklist` for transitive materialization.
/// Idempotent: a row whose token is already bound returns immediately.
fn materialize_corlib_method_row<'c>(
    module: &mut Module,
    resolution: &mut CorlibResolution,
    corlib: &SourceAssembly<'c>,
    method_row: u32,
    walk: &mut CorlibWalk,
) {
    let token = Token::new(METHOD_DEF, method_row);
    if module.resolve(LAZY_CORLIB_ASM, token).is_some() {
        return;
    }
    let mut row: u32 = 0;
    for type_def in corlib.type_defs() {
        let declaring = type_def.name();
        for method in type_def.methods() {
            row += 1;
            if row != method_row {
                continue;
            }
            let Some(declaring) = declaring else {
                return;
            };
            if type_def.is_value_type() && !is_special_reference_base(Some(declaring)) {
                walk.tokens.value_type_methods.insert(method_row);
            }
            let name: String = method.name().unwrap_or("").into();
            let signature = method.signature();
            let return_type = signature.as_ref().map(|sig| sig.return_type.clone());
            let params: Vec<SigType> = signature
                .as_ref()
                .map(|sig| sig.parameters.clone())
                .unwrap_or_default();
            let key = name_key(
                corlib,
                declaring.namespace,
                declaring.name,
                &name,
                &params,
                return_type.as_ref(),
            );
            let type_id = materialize_corlib_type(
                module,
                resolution,
                corlib,
                declaring,
                walk,
            );

            if is_delegate_type(corlib, type_def.extends()) {
                if name == ".ctor" {
                    module.mark_delegate_ctor(LAZY_CORLIB_ASM, token);
                } else if name == "Invoke" {
                    let count = u16::try_from(params.len()).unwrap_or(u16::MAX);
                    module.mark_delegate_invoke(LAZY_CORLIB_ASM, token, count);
                }
            }

            let string_ctor =
                name == ".ctor" && declaring.namespace == "System" && declaring.name == "String";
            let runtime_supplied = method.is_runtime_impl()
                || has_runtime_provided_attribute(corlib, token)
                || string_ctor;
            let intrinsic = runtime_supplied
                .then(|| {
                    bcl_intrinsic(declaring.namespace, declaring.name, &name, signature.as_ref())
                })
                .flatten();

            if let Some((func, intr_id)) = intrinsic {
                let id = module.add_intrinsic(LAZY_CORLIB_ASM, func, intr_id, arg_count(&method));
                module.bind_token(LAZY_CORLIB_ASM, token, id);
                module.set_method_type(id, type_id);
                resolution.index.insert(key, id);
                return;
            }

            let Some((_, raw_il)) = method.body_and_bytes() else {
                return;
            };
            let id = module.add_method(LAZY_CORLIB_ASM, flash_cil(raw_il), arg_count(&method));
            module.bind_token(LAZY_CORLIB_ASM, token, id);
            module.set_method_type(id, type_id);
            resolution.index.insert(key, id);

            if let Some(body) = method.body() {
                for instruction in body.code.iter() {
                    let Operand::Token(operand) = &instruction.operand else {
                        continue;
                    };
                    collect_body_tokens(&mut walk.tokens, instruction.opcode, *operand);
                    match reaches(instruction.opcode) {
                        Reaches::Member => {}
                        Reaches::Type => {
                            materialize_corlib_type_token(
                                module,
                                resolution,
                                corlib,
                                *operand,
                                walk,
                            );
                            continue;
                        }
                        Reaches::Field => {
                            if let Some(declaring) = corlib_type_of_field_row(corlib, *operand) {
                                let declaring =
                                    TypeName { namespace: &declaring.0, name: &declaring.1 };
                                materialize_corlib_type(
                                    module,
                                    resolution,
                                    corlib,
                                    declaring,
                                    walk,
                                );
                            }
                            continue;
                        }
                        Reaches::Text => {
                            materialize_string_type(module, resolution, corlib, walk);
                            continue;
                        }
                        Reaches::Nothing => continue,
                    }
                    match operand.table() {
                        METHOD_DEF => walk.worklist.push(operand.row()),
                        MEMBER_REF => {
                            walk.memberrefs.insert(*operand);
                            if let Some(row) = corlib_memberref_target_row(corlib, *operand) {
                                walk.worklist.push(row);
                            }
                        }
                        _ => {}
                    }
                    if matches!(instruction.opcode, Opcode::Callvirt) {
                        let target = match operand.table() {
                            MEMBER_REF => corlib.member_ref(operand.row()).map(|member| {
                                let target_name: String = member.name().unwrap_or("").into();
                                let signature = member.method_signature();
                                let arity =
                                    signature.as_ref().map_or(0, |sig| sig.generic_param_count);
                                let params =
                                    signature.map(|sig| sig.parameters).unwrap_or_default();
                                (target_name, params, arity)
                            }),
                            METHOD_DEF => corlib_method_name_params(corlib, operand.row()),
                            _ => None,
                        };
                        if let Some((target_name, params, arity)) = target {
                            let key = sig_encode(corlib, &target_name, &params, arity, &[]);
                            let argc = u16::try_from(params.len() + 1).unwrap_or(u16::MAX);
                            module.bind_call_target(LAZY_CORLIB_ASM, *operand, key.clone(), argc);
                            walk.dispatch.want(key);
                        }
                    }
                }
            }
            #[cfg(feature = "exceptions")]
            if let Some(body) = method.body() {
                for clause in body.handlers.iter() {
                    if let EhKind::Catch(catch_token) = clause.kind {
                        if let Some(catch_name) = corlib.type_token_name(catch_token) {
                            let tag =
                                exception_tag_for_name(catch_name.namespace, catch_name.name);
                            module.bind_catch_type_tag(LAZY_CORLIB_ASM, catch_token, tag);
                        }
                    }
                }
            }
            return;
        }
    }
}

/// Records what one instruction of a materialized corlib body will need bound, into the sets the
/// shared binder passes consume. This mirrors the collection the eager loader does over a whole
/// assembly, opcode for opcode, and exists so the two cannot drift.
fn collect_body_tokens(tokens: &mut BodyTokens, opcode: Opcode, operand: Token) {
    match opcode {
        Opcode::Ldstr => {
            tokens.strings.insert(operand);
        }
        Opcode::Call | Opcode::Callvirt if operand.table() == METHOD_SPEC => {
            tokens.generic_calls.insert(operand);
        }
        Opcode::Newobj => {
            tokens.newobj.insert(operand);
        }
        Opcode::Newarr => {
            tokens.newarr.insert(operand);
            tokens.type_tests.insert(operand);
        }
        Opcode::Ldtoken if operand.table() == FIELD => {
            tokens.ldtoken_fields.insert(operand);
        }
        Opcode::Ldtoken | Opcode::Constrained | Opcode::Initobj | Opcode::Mkrefany
        | Opcode::Refanyval
            if matches!(operand.table(), TYPE_DEF | TYPE_REF | TYPE_SPEC) =>
        {
            tokens.ldtoken_types.insert(operand);
        }
        Opcode::Box => {
            tokens.ldtoken_types.insert(operand);
            tokens.type_tests.insert(operand);
            tokens.boxes.insert(operand);
        }
        Opcode::Castclass | Opcode::Isinst | Opcode::Unbox | Opcode::UnboxAny => {
            tokens.type_tests.insert(operand);
        }
        Opcode::Sizeof => {
            tokens.sizeofs.insert(operand);
        }
        _ => {}
    }
}

/// Materializes the corlib type a TYPE token in a corlib body names, if this corlib defines it. A
/// `TypeSpec` (a constructed array type, say) names no single `TypeDef` and is skipped; so is a name
/// this corlib does not carry, exactly as on the referencing side.
fn materialize_corlib_type_token<'c>(
    module: &mut Module,
    resolution: &mut CorlibResolution,
    corlib: &SourceAssembly<'c>,
    token: Token,
    walk: &mut CorlibWalk,
) {
    let Some(name) = corlib.type_token_name(token) else {
        return;
    };
    if resolution.type_index.contains_key(&type_name_key(name)) {
        return;
    }
    if corlib_defines_type(corlib, name) {
        materialize_corlib_type(module, resolution, corlib, name, walk);
    }
}

/// Materializes `System.String`, so the module has the canonical string type id `ldstr` and every
/// string cast is decided against. The eager tier always has it because it loads the corlib whole;
/// the lazy tier has to be told, because a string literal names no type token in the IL.
fn materialize_string_type<'c>(
    module: &mut Module,
    resolution: &mut CorlibResolution,
    corlib: &SourceAssembly<'c>,
    walk: &mut CorlibWalk,
) {
    let name = TypeName { namespace: "System", name: "String" };
    if resolution.type_index.contains_key(&type_name_key(name)) {
        return;
    }
    if corlib_defines_type(corlib, name) {
        materialize_corlib_type(module, resolution, corlib, name, walk);
    }
}

/// The (namespace, name) of the corlib type declaring the field `token` names -- a `FieldDef` by
/// global row, or a `MemberRef`'s parent. `None` when this corlib declares no such field.
fn corlib_type_of_field_row(corlib: &Assembly, token: Token) -> Option<(String, String)> {
    if token.table() == MEMBER_REF {
        let parent = corlib.member_ref(token.row())?.parent();
        let name = corlib.type_token_name(parent)?;
        return Some((name.namespace.into(), name.name.into()));
    }
    if token.table() != FIELD {
        return None;
    }
    let mut field_row: u32 = 0;
    for type_def in corlib.type_defs() {
        let declaring = type_def.name();
        for _ in type_def.fields() {
            field_row += 1;
            if field_row == token.row() {
                let declaring = declaring?;
                return Some((declaring.namespace.into(), declaring.name.into()));
            }
        }
    }
    None
}

/// Registers a corlib type in the session module the first time it is reached, returning its
/// [`crate::TypeId`]; a type already in `resolution.type_index` returns its existing id (interning
/// by name, so a corlib type is ONE session identity across every assembly that names it -- a
/// `string` from delta A and one from delta B share it). Records the canonical `System.String` id so
/// `ldstr` works.
fn materialize_corlib_type<'c>(
    module: &mut Module,
    resolution: &mut CorlibResolution,
    corlib: &SourceAssembly<'c>,
    name: TypeName<'_>,
    walk: &mut CorlibWalk,
) -> TypeId {
    let key = type_name_key(name);
    if let Some(&type_id) = resolution.type_index.get(&key) {
        return type_id;
    }
    if !resolution.corlib_enums_indexed {
        resolution.corlib_enums_indexed = true;
        index_enum_zeros(corlib, &mut resolution.field_index.enum_zeros);
    }

    let mut field_row: u32 = 0;
    let mut type_row: u32 = 0;
    let mut found: Option<CorlibTypeDef> = None;
    for type_def in corlib.type_defs() {
        type_row += 1;
        let is_target = type_def
            .name()
            .is_some_and(|n| n.namespace == name.namespace && n.name == name.name);
        let mut own_instance = Vec::new();
        let mut own_static = Vec::new();
        let is_enum = is_target && is_enum_type(corlib, type_def.extends());
        if is_enum && has_flags_attribute(corlib, Token::new(TYPE_DEF, type_row)) {
            module.set_enum_flags(LAZY_CORLIB_ASM, Token::new(TYPE_DEF, type_row).0);
        }
        for field in type_def.fields() {
            field_row += 1;
            if !is_target {
                continue;
            }
            let token = Token::new(FIELD, field_row);
            if field.is_static() && field.is_literal() {
                if let (true, Some(member), Some(constant)) = (is_enum, field.name(), field.constant())
                {
                    let type_token = Token::new(TYPE_DEF, type_row).0;
                    if matches!(constant, ConstantValue::I8(_) | ConstantValue::U8(_)) {
                        module.set_enum_wide(LAZY_CORLIB_ASM, type_token);
                    }
                    if matches!(
                        constant,
                        ConstantValue::U1(_)
                            | ConstantValue::U2(_)
                            | ConstantValue::U4(_)
                            | ConstantValue::U8(_)
                    ) {
                        module.set_enum_unsigned(LAZY_CORLIB_ASM, type_token);
                    }
                    module.set_enum_width(
                        LAZY_CORLIB_ASM,
                        type_token,
                        enum_constant_width(&constant),
                    );
                    if let Some(value) = constant_as_i64(constant) {
                        module.set_enum_constant(LAZY_CORLIB_ASM, type_token, value, member.into());
                    }
                }
                continue;
            }
            let entry = CorlibField {
                token,
                name: field.name().unwrap_or("").into(),
                default: default_field_value_of(
                    corlib,
                    field.signature(),
                    &resolution.field_index.enum_zeros,
                ),
            };
            if field.is_static() {
                own_static.push(entry);
            } else {
                own_instance.push(entry);
            }
        }
        if is_target {
            found = Some(CorlibTypeDef {
                extends: type_def.extends(),
                interfaces: type_def.interfaces().collect(),
                instance_fields: own_instance,
                static_fields: own_static,
            });
            break;
        }
    }

    let CorlibTypeDef {
        extends,
        interfaces,
        instance_fields: own_instance,
        static_fields: own_static,
    } = match found {
        Some(found) => found,
        None => {
            let type_id = module.add_type(Vec::new());
            module.bind_type_full_name(type_id, full_type_name(name));
            resolution.type_index.insert(key, type_id);
            return type_id;
        }
    };

    resolution.materializing.push(key.clone());

    let mut base_type: Option<TypeId> = None;
    let base_defaults: Vec<Value> = if extends.row() != 0 {
        if let Some(base) =
            corlib.type_token_name(extends).filter(|base| !resolution.is_materializing(*base))
        {
            let base_id = materialize_corlib_type(
                module,
                resolution,
                corlib,
                base,
                walk,
            );
            base_type = Some(base_id);
            module.type_field_defaults(base_id).unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let base_count = base_defaults.len();
    let mut full = base_defaults;
    full.extend(own_instance.iter().map(|field| field.default.clone()));

    let type_id = module.add_type(full);
    if module.string_type_id().is_none() && name.namespace == "System" && name.name == "String" {
        module.set_string_type_id(type_id);
    }
    module.bind_type_full_name(type_id, full_type_name(name));
    let own_token = Token::new(TYPE_DEF, type_row);
    module.bind_type_token(LAZY_CORLIB_ASM, own_token, type_id);
    module.bind_type_name(LAZY_CORLIB_ASM, own_token, name.name.into());
    if name.namespace == "System" {
        match name.name {
            "Object" => module.mark_object_type_token(LAZY_CORLIB_ASM, own_token),
            "String" => module.mark_string_type_token(LAZY_CORLIB_ASM, own_token),
            _ => {}
        }
    }
    if let Some(kind) = primitive_value_kind(name.namespace, name.name) {
        module.set_primitive_type_token(LAZY_CORLIB_ASM, own_token, &kind);
    }
    module.set_type_base(type_id, base_type);
    let is_value_type = corlib.type_token_name(extends).is_some_and(|base| {
        base.namespace == "System" && (base.name == "ValueType" || base.name == "Enum")
    });
    module.set_type_is_value_type(type_id, is_value_type);
    if is_value_type {
        walk.tokens.value_types.push(own_token);
    }
    resolution.type_index.insert(key, type_id);
    for (index, field) in own_instance.iter().enumerate() {
        let slot = (base_count + index) as u32;
        module.bind_field(LAZY_CORLIB_ASM, field.token, slot);
        module.bind_field_type(LAZY_CORLIB_ASM, field.token, type_id);
        resolution
            .field_index
            .instances
            .insert(field_name_key(name, &field.name), slot);
    }
    let statics_start = module.static_field_count() as u32;
    for field in &own_static {
        module.bind_static_field(LAZY_CORLIB_ASM, field.token, field.default.clone());
        if let Some(slot) = module.static_field_slot(LAZY_CORLIB_ASM, field.token) {
            resolution
                .field_index
                .statics
                .insert(field_name_key(name, &field.name), slot);
        }
    }
    module.bind_static_slot_range(statics_start, module.static_field_count() as u32, type_id);
    resolution.corlib_types.push((name.namespace.into(), name.name.into()));
    let implemented: Vec<TypeId> = interfaces
        .iter()
        .filter_map(|token| corlib.type_token_name(*token))
        .filter(|interface| corlib_defines_type(corlib, *interface))
        .map(|interface| {
            materialize_corlib_type(module, resolution, corlib, interface, walk)
        })
        .collect();
    if !implemented.is_empty() {
        module.set_type_interfaces(type_id, implemented);
    }
    if let Some(row) = corlib_named_method_row(corlib, name, ".cctor") {
        materialize_corlib_method_row(
            module,
            resolution,
            corlib,
            row,
            walk,
        );
        if let Some(id) = module.resolve(LAZY_CORLIB_ASM, Token::new(METHOD_DEF, row)) {
            module.add_static_ctor(id);
        }
    }
    let wanted = walk.dispatch.wanted.clone();
    let sig_methods = materialize_corlib_sig_methods(
        module,
        resolution,
        corlib,
        name,
        walk,
        &wanted,
    );
    if !sig_methods.is_empty() {
        module.set_sig_methods(type_id, sig_methods);
    }
    resolution.materializing.pop();
    type_id
}

/// The global `MethodDef` row of `type_name`'s method called `method` (the same numbering
/// [`materialize_corlib_method_row`] uses), or `None` if the type declares none. Used for the
/// members no call site names -- a `.cctor`, which the runtime reaches through its type rather than
/// through a token in anybody's IL.
fn corlib_named_method_row(corlib: &Assembly, type_name: TypeName<'_>, method: &str) -> Option<u32> {
    let mut method_row: u32 = 0;
    for type_def in corlib.type_defs() {
        let is_target = type_def
            .name()
            .is_some_and(|n| n.namespace == type_name.namespace && n.name == type_name.name);
        for candidate in type_def.methods() {
            method_row += 1;
            if is_target && candidate.name() == Some(method) {
                return Some(method_row);
            }
        }
        if is_target {
            return None;
        }
    }
    None
}

/// The declared name + parameter types of the corlib method at global `row` (the same numbering
/// [`materialize_corlib_method_row`] uses), for building a `callvirt` target's signature key.
fn corlib_method_name_params(corlib: &Assembly, row: u32) -> Option<(String, Vec<SigType>, u32)> {
    let mut method_row: u32 = 0;
    for type_def in corlib.type_defs() {
        for method in type_def.methods() {
            method_row += 1;
            if method_row != row {
                continue;
            }
            let name: String = method.name().unwrap_or("").into();
            let signature = method.signature();
            let params: Vec<SigType> = signature
                .as_ref()
                .map(|signature| signature.parameters.clone())
                .unwrap_or_default();
            let arity = signature.as_ref().map_or(0, |signature| signature.generic_param_count);
            return Some((name, params, arity));
        }
    }
    None
}

/// The virtual-dispatch signature map for a materialized corlib type: every virtual method reachable
/// on it, walking `name`'s base chain with the MOST-DERIVED declaration winning each signature.
///
/// `callvirt` needs this because a call site compiled against a BASE declaration must still reach the
/// derived override at run time. C# lowers `sb.ToString()` inside a corlib body to a call on
/// `System.Object::ToString`, and [`resolve_callvirt`](lamella_cil_runtime) tries, in order: an
/// explicit `MethodImpl`, then the static target's VTABLE SLOT, then this signature map. The lazy
/// tier binds no vtable slots (only the eager `build_vtables` calls `bind_method_slot`), so the slot
/// branch is skipped and the signature map is the path taken. Without it the call falls through to
/// the static target -- the `Object::ToString` intrinsic, which renders the declaring type's NAME --
/// so `"hello".ToUpper()` returned `"System.Text.StringBuilder"` instead of the built string. That
/// made LAZY disagree with EAGER (which builds real vtables), breaking the observational invariant,
/// and it failed SILENTLY rather than trapping.
fn materialize_corlib_sig_methods<'c>(
    module: &mut Module,
    resolution: &mut CorlibResolution,
    corlib: &SourceAssembly<'c>,
    name: TypeName<'_>,
    walk: &mut CorlibWalk,
    wanted: &BTreeSet<String>,
) -> BTreeMap<String, MethodId> {
    const MAX_BASE_DEPTH: usize = 16;
    let mut sig: BTreeMap<String, MethodId> = BTreeMap::new();
    if wanted.is_empty() {
        return sig;
    }
    let mut current: Option<(String, String)> =
        Some((name.namespace.into(), name.name.into()));
    for _ in 0..MAX_BASE_DEPTH {
        let Some((namespace, type_name)) = current.take() else {
            break;
        };
        let mut method_row: u32 = 0;
        let mut own: Vec<(u32, String, Vec<SigType>, u32)> = Vec::new();
        let mut extends: Option<Token> = None;
        for type_def in corlib.type_defs() {
            let is_target = type_def
                .name()
                .is_some_and(|n| n.namespace == namespace && n.name == type_name);
            for method in type_def.methods() {
                method_row += 1;
                if !is_target || method.flags() & METHOD_VIRTUAL == 0 {
                    continue;
                }
                let method_name: String = method.name().unwrap_or("").into();
                let signature = method.signature();
                let params: Vec<SigType> = signature
                    .as_ref()
                    .map(|signature| signature.parameters.clone())
                    .unwrap_or_default();
                let arity = signature.as_ref().map_or(0, |sig| sig.generic_param_count);
                own.push((method_row, method_name, params, arity));
            }
            if is_target {
                extends = Some(type_def.extends());
                break;
            }
        }
        if extends.is_none() {
            break;
        }
        for (row, method_name, params, arity) in own {
            let key = sig_encode(corlib, &method_name, &params, arity, &[]);
            if sig.contains_key(&key) {
                continue;
            }
            if !wanted.contains(&key) {
                continue;
            }
            materialize_corlib_method_row(
                module,
                resolution,
                corlib,
                row,
                walk,
            );
            if let Some(id) = module.resolve(LAZY_CORLIB_ASM, Token::new(METHOD_DEF, row)) {
                sig.insert(key, id);
            }
        }
        current = extends
            .filter(|token| token.row() != 0)
            .and_then(|token| corlib.type_token_name(token))
            .map(|base| (base.namespace.into(), base.name.into()));
    }
    sig
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
pub fn load_with_corlib<'c, 'p>(
    corlib: &SourceAssembly<'c>,
    program: &SourceAssembly<'p>,
) -> Result<Program, LoadError> {
    load_with_corlib_and_libraries(corlib, &[], program)
}

/// [`load_with_corlib_and_libraries`] for exactly one library (e.g. `System.Device.Gpio`):
/// corlib -> asm 0, library -> asm 1, program -> asm 2.
///
/// # Errors
/// As [`load_with_corlib_and_libraries`].
pub fn load_with_corlib_and_library<'c, 'l, 'p>(
    corlib: &SourceAssembly<'c>,
    library: &SourceAssembly<'l>,
    program: &SourceAssembly<'p>,
) -> Result<Program, LoadError> {
    load_with_corlib_and_libraries(corlib, core::slice::from_ref(library), program)
}

/// Like [`load_with_corlib`] but loads any number of LIBRARY assemblies (e.g.
/// `System.Device.Gpio`, `Lamella.Net.Time`, `System.Net.NetworkInformation`) between corlib
/// and the program: corlib -> asm 0, the libraries -> asm 1..=N in the given order, program ->
/// asm N+1. Every cross-assembly reference -- a program `newobj` of a library type, a virtual
/// call into a library base, a static call, a library's own call into corlib or a sibling
/// library -- resolves by name through the shared indices, exactly as a two-assembly load
/// resolves a program's references to corlib. Resolution is name-keyed, so the library ORDER
/// never changes what binds -- only which assembly id each image gets. This is the deploy
/// shape for a driver or protocol stack: the corlib, the device/net API assemblies, the app.
///
/// # Errors
/// [`LoadError::NoEntryPoint`] if the program names no entry point;
/// [`LoadError::EntryHasNoBody`] if the entry-point token has no loadable body.
///
/// # Panics
/// If given more than 253 libraries -- assembly ids are 8-bit (corlib 0, libraries 1..=N,
/// program N+1), and no deploy tier approaches that.
pub fn load_with_corlib_and_libraries<'c, 'l, 'p>(
    corlib: &SourceAssembly<'c>,
    libraries: &[SourceAssembly<'l>],
    program: &SourceAssembly<'p>,
) -> Result<Program, LoadError> {
    let mut loaded = load_with_corlib_and_libraries_unfrozen(corlib, libraries, program)?;
    loaded.module.freeze();
    Ok(loaded)
}

/// [`load_with_corlib_and_library`] WITHOUT the final freeze -- for baking (`write_baked`), which
/// needs the unfrozen module. See [`load_unfrozen`].
///
/// # Errors
/// As [`load_with_corlib_and_library`].
pub fn load_with_corlib_and_library_unfrozen<'c, 'l, 'p>(
    corlib: &SourceAssembly<'c>,
    library: &SourceAssembly<'l>,
    program: &SourceAssembly<'p>,
) -> Result<Program, LoadError> {
    load_with_corlib_and_libraries_unfrozen(corlib, core::slice::from_ref(library), program)
}

/// [`load_with_corlib_and_libraries`] WITHOUT the final freeze -- for baking (`write_baked`),
/// which needs the unfrozen module. See [`load_unfrozen`]. This is the one loading core every
/// `load_with_corlib*` variant delegates to.
///
/// # Errors
/// As [`load_with_corlib_and_libraries`].
///
/// # Panics
/// As [`load_with_corlib_and_libraries`].
pub fn load_with_corlib_and_libraries_unfrozen<'c, 'l, 'p>(
    corlib: &SourceAssembly<'c>,
    libraries: &[SourceAssembly<'l>],
    program: &SourceAssembly<'p>,
) -> Result<Program, LoadError> {
    #[cfg(feature = "generics")]
    let instantiations = {
        let mut references: Vec<Assembly<'_>> = Vec::with_capacity(1 + libraries.len());
        references.push(corlib.clone());
        references.extend(libraries.iter().cloned());
        monomorphize::collect_instantiations(program, &references)
    };
    #[cfg(not(feature = "generics"))]
    let instantiations: Vec<monomorphize::Instantiation> = Vec::new();
    load_with_corlib_and_libraries_lowered(corlib, libraries, program, &instantiations)
        .map(|(program, _)| program)
}

/// [`load_with_corlib_unfrozen`] with the program's closed generic instantiations MONOMORPHIZED:
/// each one becomes its own type identity, and the call sites that reach it bind to members of that
/// identity instead of leaving the `UnloweredGeneric` mark the bake refuses on.
///
/// **THE SET IS A PARAMETER, AND SO IS EACH INSTANTIATION'S CANONICAL NAME.** This crate
/// collects nothing and spells nothing: both already exist in the AOT tier, and a second collector
/// or a second spelling would be a source of drift rather than a second opinion. See
/// [`monomorphize`] for the whole argument. Passing an EMPTY set is exactly what every other
/// `load_with_corlib*` entry point does, which is why they still refuse a generic program.
///
/// Returns the loaded program together with what the pass lowered and REFUSED. A refusal leaves its
/// call sites marked, so `Module::validate_profile` still reports them and the bake still stops.
///
/// # Errors
/// As [`load_with_corlib_and_libraries`].
///
/// # Panics
/// As [`load_with_corlib_and_libraries`].
pub fn load_with_corlib_monomorphized<'c, 'p>(
    corlib: &SourceAssembly<'c>,
    program: &SourceAssembly<'p>,
    instantiations: &[monomorphize::Instantiation],
) -> Result<(Program, monomorphize::Lowering), LoadError> {
    load_with_corlib_and_libraries_lowered(corlib, &[], program, instantiations)
}

/// The one loading core. `instantiations` are the PROGRAM's closed generic instantiations; an empty
/// slice is the ordinary (non-monomorphizing) load.
///
/// # Errors
/// As [`load_with_corlib_and_libraries`].
///
/// # Panics
/// As [`load_with_corlib_and_libraries`].
fn load_with_corlib_and_libraries_lowered<'c, 'l, 'p>(
    corlib: &SourceAssembly<'c>,
    libraries: &[SourceAssembly<'l>],
    program: &SourceAssembly<'p>,
    instantiations: &[monomorphize::Instantiation],
) -> Result<(Program, monomorphize::Lowering), LoadError> {
    assert!(
        libraries.len() <= usize::from(u8::MAX) - 2,
        "assembly ids are 8-bit: corlib + at most {} libraries + the program",
        usize::from(u8::MAX) - 2
    );
    if program.image().entry_point_token() == 0 {
        return Err(LoadError::NoEntryPoint);
    }
    let mut module = Module::new();
    let mut index = NameIndex::new();
    let mut type_index = TypeNameIndex::new();
    let mut field_index = FieldNameIndex::new();
    let corlib_type_offset = module.type_count();
    load_assembly(
        &mut module,
        corlib,
        flash_cil,
        0,
        &mut index,
        &mut type_index,
        &mut field_index,
        true,
    );
    let mut library_type_offsets: Vec<usize> = Vec::with_capacity(libraries.len());
    for (position, library) in libraries.iter().enumerate() {
        library_type_offsets.push(module.type_count());
        load_assembly(
            &mut module,
            library,
            flash_cil,
            1 + position as u8,
            &mut index,
            &mut type_index,
            &mut field_index,
            true,
        );
    }
    let program_asm = 1 + libraries.len() as u8;
    let program_type_offset = module.type_count();
    let entry = load_assembly(
        &mut module,
        program,
        flash_cil,
        program_asm,
        &mut index,
        &mut type_index,
        &mut field_index,
        true,
    );
    #[cfg(feature = "generics")]
    let sources: Vec<monomorphize::DefinitionSource<'_>> = {
        let mut sources = Vec::with_capacity(2 + libraries.len());
        sources.push(monomorphize::DefinitionSource {
            assembly: program.clone(),
            asm: program_asm,
            type_offset: Some(program_type_offset),
        });
        sources.push(monomorphize::DefinitionSource {
            assembly: corlib.clone(),
            asm: 0,
            type_offset: Some(corlib_type_offset),
        });
        for (position, library) in libraries.iter().enumerate() {
            sources.push(monomorphize::DefinitionSource {
                assembly: library.clone(),
                asm: 1 + position as u8,
                type_offset: Some(library_type_offsets[position]),
            });
        }
        sources
    };
    #[cfg(not(feature = "generics"))]
    let sources: Vec<monomorphize::DefinitionSource<'_>> = {
        let _ = (corlib_type_offset, &library_type_offsets);
        alloc::vec![monomorphize::DefinitionSource {
            assembly: program.clone(),
            asm: program_asm,
            type_offset: Some(program_type_offset),
        }]
    };
    let lowering = monomorphize::monomorphize(
        &mut module,
        program,
        program_asm,
        &sources,
        &type_index,
        &field_index,
        flash_cil,
        instantiations,
    );
    let entry = entry.ok_or(LoadError::EntryHasNoBody)?;
    Ok((Program { module, entry }, lowering))
}

/// [`load_with_corlib`] WITHOUT the final freeze -- see [`load_unfrozen`].
///
/// # Errors
/// As [`load_with_corlib`].
pub fn load_with_corlib_unfrozen<'c, 'p>(
    corlib: &SourceAssembly<'c>,
    program: &SourceAssembly<'p>,
) -> Result<Program, LoadError> {
    load_with_corlib_and_libraries_unfrozen(corlib, &[], program)
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
///
/// `materialize` decides where each method's CIL lives ([`CilMaterializer`]). Taking a plain
/// [`Assembly`] rather than a [`SourceAssembly`] is what lets a caller load a PE that does NOT
/// outlive the module -- it must then pass [`ram_cil`], since [`flash_cil`] will not type-check
/// against a non-`'static` `'pe`.
fn load_assembly<'pe>(
    module: &mut Module,
    assembly: &Assembly<'pe>,
    materialize: CilMaterializer<'pe>,
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
    let mut instance_field_ref_tokens = BTreeSet::new();
    let mut ldtoken_type_tokens = BTreeSet::new();
    let mut type_test_tokens = BTreeSet::new();
    let mut box_tokens = BTreeSet::new();
    let mut generic_call_tokens = BTreeSet::new();
    let mut value_type_method_rows: BTreeSet<u32> = BTreeSet::new();
    let mut list_ctor_rows: BTreeMap<u32, u16> = BTreeMap::new();
    let mut sizeof_tokens: BTreeSet<Token> = BTreeSet::new();
    let mut value_type_tokens: Vec<Token> = Vec::new();
    let mut methoddef_sigs: BTreeMap<u32, (String, Vec<SigType>, u32)> = BTreeMap::new();
    let mut type_extends: Vec<Token> = Vec::new();
    let mut type_interfaces: Vec<Vec<Token>> = Vec::new();
    let mut type_virtuals: Vec<Vec<VirtualMethod>> = Vec::new();
    let mut type_nonvirtuals: Vec<Vec<VirtualMethod>> = Vec::new();
    let mut type_is_value_type: Vec<bool> = Vec::new();
    let mut own_fields: Vec<Vec<(Token, Value)>> = Vec::new();
    let mut instance_field_keys: BTreeMap<u32, String> = BTreeMap::new();
    let mut method_row: u32 = 0;
    let mut field_row: u32 = 0;
    let mut type_row: u32 = 0;
    index_enum_zeros(assembly, &mut field_index.enum_zeros);
    let generic_definitions: BTreeSet<u32> = assembly
        .generic_params()
        .filter(|&(_, _, owner, _)| owner & 1 == 0)
        .map(|(_, _, owner, _)| owner >> 1)
        .collect();
    for type_def in assembly.type_defs() {
        type_row += 1;
        let is_generic_definition = generic_definitions.contains(&type_row);
        let is_enum = is_enum_type(assembly, type_def.extends());
        if is_enum && has_flags_attribute(assembly, Token::new(TYPE_DEF, type_row)) {
            module.set_enum_flags(asm, Token::new(TYPE_DEF, type_row).0);
        }
        let mut own = Vec::new();
        let statics_start = module.static_field_count();
        for field in type_def.fields() {
            field_row += 1;
            let token = Token::new(FIELD, field_row);
            if field.is_static() {
                if !field.is_literal() {
                    let default =
                        default_field_value_of(assembly, field.signature(), &field_index.enum_zeros);
                    module.bind_static_field(asm, token, default);
                    if let (Some(field_name), Some(slot)) =
                        (field.name(), module.static_field_slot(asm, token))
                    {
                        if let Some((ns, tn)) = key_type_name(assembly, &type_def) {
                            field_index.statics.insert(field_key(&ns, &tn, field_name), slot);
                        }
                    }
                } else if is_enum {
                    if let (Some(name), Some(constant)) = (field.name(), field.constant()) {
                        let type_token = Token::new(TYPE_DEF, type_row).0;
                        if matches!(constant, ConstantValue::I8(_) | ConstantValue::U8(_)) {
                            module.set_enum_wide(asm, type_token);
                        }
                        if matches!(
                            constant,
                            ConstantValue::U1(_)
                                | ConstantValue::U2(_)
                                | ConstantValue::U4(_)
                                | ConstantValue::U8(_)
                        ) {
                            module.set_enum_unsigned(asm, type_token);
                        }
                        module.set_enum_width(asm, type_token, enum_constant_width(&constant));
                        if let Some(value) = constant_as_i64(constant) {
                            module.set_enum_constant(asm, type_token, value, name.into());
                        }
                    }
                }
                continue;
            }
            if let (Some((ns, tn)), Some(field_name)) = (key_type_name(assembly, &type_def), field.name()) {
                instance_field_keys.insert(token.0, field_key(&ns, &tn, field_name));
            }
            own.push((
                token,
                default_field_value_of(assembly, field.signature(), &field_index.enum_zeros),
            ));
        }
        let type_id = module.add_type(Vec::new());
        module.bind_static_slot_range(
            statics_start as u32,
            module.static_field_count() as u32,
            type_id,
        );
        if let Some(name) = type_def.name() {
            if module.string_type_id().is_none() && name.namespace == "System" && name.name == "String"
            {
                module.set_string_type_id(type_id);
            }
            if let Some((ns, tn)) = key_type_name(assembly, &type_def) {
                type_index.insert(type_key(&ns, &tn), type_id);
            }
            module.bind_type_name(asm, Token::new(TYPE_DEF, type_row), name.name.into());
            module.bind_type_full_name(type_id, full_type_name(name));
            #[cfg(feature = "NETMFv4_4")]
            {
                if name.name != "<Module>" {
                    module.add_assembly_type(asm, asm_key(asm, Token::new(TYPE_DEF, type_row).0));
                }
                if module.assembly_name(asm).is_none() {
                    let simple = assembly.assembly_name().unwrap_or("");
                    module.bind_assembly_name(
                        asm,
                        alloc::format!(
                            "{simple}, Version=0.0.0.0, Culture=neutral, PublicKeyToken=null"
                        ),
                    );
                }
            }
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
        let mut nonvirtuals = Vec::new();
        let type_name = type_def.name();
        let is_delegate = is_delegate_type(assembly, type_def.extends());
        let is_value_type = type_def.is_value_type() && !is_special_reference_base(type_def.name());
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
            let method_sig = method.signature();
            let params: Vec<SigType> = method_sig
                .as_ref()
                .map(|sig| sig.parameters.clone())
                .unwrap_or_default();
            let return_type: Option<SigType> = method_sig.as_ref().map(|sig| sig.return_type.clone());
            let generic_arity = method_sig.as_ref().map_or(0, |sig| sig.generic_param_count);
            methoddef_sigs.insert(method_row, (name.clone(), params.clone(), generic_arity));
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
                    if same_assembly_list_ctor(name_parts.namespace, name_parts.name) {
                        list_ctor_rows.insert(method_row, count);
                    }
                }
            }
            let string_ctor = name == ".ctor"
                && type_def
                    .name()
                    .is_some_and(|declaring| declaring.namespace == "System" && declaring.name == "String");
            let runtime_supplied = method.is_runtime_impl()
                || has_runtime_provided_attribute(assembly, token)
                || string_ctor;
            let intrinsic = runtime_supplied
                .then(|| {
                    let signature = method.signature();
                    type_def.name().and_then(|declaring| {
                        bcl_intrinsic(declaring.namespace, declaring.name, &name, signature.as_ref())
                    })
                })
                .flatten();
            if let Some((func, intr_id)) = intrinsic {
                let id = module.add_intrinsic(asm, func, intr_id, arg_count(&method));
                module.bind_token(asm, token, id);
                module.set_method_type(id, type_id);
                if let Some((ns, tn)) = key_type_name(assembly, &type_def) {
                    index.insert(
                        name_key(assembly, &ns, &tn, &name, &params, return_type.as_ref()),
                        id,
                    );
                }
                if method.flags() & METHOD_VIRTUAL != 0 {
                    virtuals.push(VirtualMethod {
                        id,
                        name: name.clone(),
                        params: params.clone(),
                        newslot: method.flags() & METHOD_NEWSLOT != 0,
                        generic_arity,
                    });
                }
                if token.0 == entry_token {
                    entry = Some(id);
                }
                continue;
            }
            let Some((body, raw_il)) = method.body_and_bytes() else {
                bind_pinvoke_target(assembly, module, asm, token, &method);
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
                            if operand.table() == METHOD_SPEC {
                                generic_call_tokens.insert(*operand);
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
                            type_test_tokens.insert(*operand);
                        }
                        Opcode::Ldtoken if operand.table() == FIELD => {
                            ldtoken_field_tokens.insert(*operand);
                        }
                        Opcode::Ldfld | Opcode::Stfld | Opcode::Ldflda
                            if operand.table() == MEMBER_REF =>
                        {
                            instance_field_ref_tokens.insert(*operand);
                        }
                        Opcode::Ldsfld | Opcode::Stsfld | Opcode::Ldsflda
                            if operand.table() == MEMBER_REF =>
                        {
                            static_field_ref_tokens.insert(*operand);
                        }
                        Opcode::Ldtoken
                        | Opcode::Constrained
                        | Opcode::Box
                        | Opcode::Initobj
                        | Opcode::Mkrefany
                        | Opcode::Refanyval
                            if matches!(operand.table(), TYPE_DEF | TYPE_REF | TYPE_SPEC) =>
                        {
                            ldtoken_type_tokens.insert(*operand);
                            if matches!(instruction.opcode, Opcode::Box) {
                                type_test_tokens.insert(*operand);
                                box_tokens.insert(*operand);
                            }
                        }
                        Opcode::Castclass
                        | Opcode::Isinst
                        | Opcode::Box
                        | Opcode::Unbox
                        | Opcode::UnboxAny => {
                            type_test_tokens.insert(*operand);
                        }
                        Opcode::Sizeof => {
                            sizeof_tokens.insert(*operand);
                        }
                        _ => {}
                    }
                }
            }
            #[cfg(feature = "exceptions")]
            for clause in body.handlers.iter() {
                if let EhKind::Catch(catch_token) = clause.kind {
                    if let Some(catch_name) = assembly.type_token_name(catch_token) {
                        let tag =
                            exception_tag_for_name(catch_name.namespace, catch_name.name);
                        module.bind_catch_type_tag(asm, catch_token, tag);
                    } else if matches!(
                        assembly.type_spec_signature(catch_token),
                        Some(lamella_metadata::SigType::GenericInst { .. })
                    ) {
                        module.mark_unlowered_generic(asm, catch_token);
                    }
                }
            }
            let id = module.add_method(asm, materialize(raw_il), arg_count(&method));
            module.bind_token(asm, token, id);
            module.set_method_type(id, type_id);
            if let Some((ns, tn)) = key_type_name(assembly, &type_def) {
                index.insert(
                    name_key(assembly, &ns, &tn, &name, &params, return_type.as_ref()),
                    id,
                );
            }
            #[cfg(feature = "debug-names")]
            {
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
            }
            if name == ".cctor" && !is_generic_definition {
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
                    generic_arity,
                });
            } else if !method.is_static() && name != ".ctor" {
                nonvirtuals.push(VirtualMethod {
                    id,
                    name,
                    params,
                    newslot: false,
                    generic_arity,
                });
            }
            if token.0 == entry_token {
                entry = Some(id);
            }
        }
        type_virtuals.push(virtuals);
        type_nonvirtuals.push(nonvirtuals);
    }

    for ctor in assembly.custom_attribute_ctors() {
        if ctor.table() == MEMBER_REF {
            bcl_call_tokens.insert(ctor);
        }
    }

    bind_strings(assembly, module, asm, &string_tokens);
    bind_bcl_calls(
        assembly,
        module,
        asm,
        index,
        type_index,
        resolve_external,
        &bcl_call_tokens,
    );
    bind_array_defaults(assembly, module, asm, type_index, &field_index.enum_zeros, &newarr_tokens);
    bind_box_primitives(assembly, module, asm, &box_tokens);
    bind_generic_calls(assembly, module, asm, &generic_call_tokens);
    mark_value_type_ctors(module, asm, &newobj_tokens, &value_type_method_rows);
    mark_same_assembly_ctors(
        module,
        asm,
        &newobj_tokens,
        &list_ctor_rows,
    );
    bind_field_rva_data(assembly, module, asm, &ldtoken_field_tokens);
    bind_static_field_refs(assembly, module, asm, field_index, &static_field_ref_tokens);
    bind_instance_field_refs(
        assembly,
        module,
        asm,
        field_index,
        type_index,
        &instance_field_ref_tokens,
    );
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
        field_index,
        &instance_field_keys,
    );
    build_vtables(
        module,
        assembly,
        type_offset,
        type_index,
        &type_extends,
        &type_virtuals,
    );
    build_sig_methods(
        module,
        assembly,
        asm,
        type_offset,
        &type_extends,
        &type_virtuals,
        &type_nonvirtuals,
    );
    bind_call_targets(module, assembly, asm, &callvirt_tokens, &methoddef_sigs);
    bind_explicit_overrides(module, assembly, asm, type_offset);
    bind_types(
        assembly,
        module,
        asm,
        type_offset,
        &type_extends,
        &type_is_value_type,
        type_index,
    );
    bind_interfaces(
        module,
        assembly,
        asm,
        type_offset,
        type_index,
        &type_interfaces,
    );
    record_custom_attributes(assembly, module, asm, type_index);
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
    type_index: &TypeNameIndex,
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

        if let Some(sig) = signature.as_ref() {
            if let (true, Some(fixed)) = (sig.is_vararg, sig.sentinel_index) {
                let target = match parent.table() {
                    METHOD_DEF => module.resolve(asm, parent),
                    TYPE_DEF | TYPE_REF => {
                        assembly.type_token_full_name(parent).and_then(|(namespace, type_name)| {
                            let key = name_key(
                                assembly,
                                &namespace,
                                &type_name,
                                method_name,
                                &params[..fixed],
                                Some(&sig.return_type),
                            );
                            index.get(&key).copied()
                        })
                    }
                    _ => None,
                };
                if let Some(target) = target {
                    let var_types: Vec<u64> = params[fixed..]
                        .iter()
                        .map(|param| {
                            sigtype_to_type_handle(assembly, module, asm, param, type_index)
                                .unwrap_or(0)
                        })
                        .collect();
                    let total = usize::from(sig.has_this) + params.len();
                    let vararg_start = usize::from(sig.has_this) + fixed;
                    module.bind_token(asm, *token, target);
                    module.bind_vararg_site(
                        asm,
                        *token,
                        VarargSite {
                            total_args: u16::try_from(total).unwrap_or(u16::MAX),
                            vararg_start: u16::try_from(vararg_start).unwrap_or(0),
                            var_types,
                        },
                    );
                    continue;
                }
            }
        }

        if method_name == ".ctor" {
            if let [SigType::Object, SigType::IntPtr] = params {
                module.mark_delegate_ctor(asm, *token);
                continue;
            }
        }

        if method_name == "Invoke" {
            let declared_by_a_delegate = assembly
                .type_token_full_name(parent)
                .and_then(|(namespace, name)| type_index.get(&type_key(&namespace, &name)).copied())
                .and_then(|type_id| module.type_base(type_id))
                .is_some_and(|base| {
                    ["System.MulticastDelegate", "System.Delegate"]
                        .iter()
                        .any(|name| type_index.get(*name) == Some(&base))
                });
            if declared_by_a_delegate {
                let count = u16::try_from(params.len()).unwrap_or(u16::MAX);
                module.mark_delegate_invoke(asm, *token, count);
                continue;
            }
        }

        let function = if parent.table() == TYPE_SPEC {
            let spec = assembly.type_spec_signature(parent);
            if matches!(spec, Some(lamella_metadata::SigType::GenericInst { .. })) {
                if spec.as_ref().is_some_and(monomorphize::mentions_parameter) {
                    bind_own_generic_member(
                        assembly,
                        module,
                        asm,
                        index,
                        spec.as_ref(),
                        method_name,
                        params,
                        signature.as_ref().map(|sig| &sig.return_type),
                        *token,
                    );
                } else {
                    module.mark_unlowered_generic(asm, *token);
                }
                continue;
            }
            match method_name {
                ".ctor" => {
                    let rank = signature.as_ref().map_or(0, |sig| sig.parameters.len());
                    module.mark_md_array_ctor(asm, *token, u16::try_from(rank).unwrap_or(0));
                    continue;
                }
                "Get" => Some(intrinsic!(md_array_get)),
                "Set" => Some(intrinsic!(md_array_set)),
                "Address" => Some(intrinsic!(md_array_address)),
                _ => continue,
            }
        } else if parent.table() == TYPE_REF {
            let Some((parent_namespace, parent_name)) = assembly.type_token_full_name(parent) else {
                continue;
            };
            let parent_type = TypeName {
                namespace: &parent_namespace,
                name: &parent_name,
            };
            if resolve_external {
                let key = name_key(
                    assembly,
                    parent_type.namespace,
                    parent_type.name,
                    method_name,
                    params,
                    signature.as_ref().map(|sig| &sig.return_type),
                );
                if let Some(&target) = index.get(&key) {
                    module.bind_token(asm, *token, target);
                    continue;
                }
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
        let Some((function, intr_id)) = function else {
            continue;
        };
        let id = match bound.get(&(function as usize, arg_count)) {
            Some(&id) => id,
            None => {
                let id = module.add_intrinsic(asm, function, intr_id, arg_count);
                bound.insert((function as usize, arg_count), id);
                id
            }
        };
        module.bind_token(asm, *token, id);
    }
}

/// Binds a `MemberRef` reached through the declaring type's OWN OPEN instantiation -- `List<!0>`
/// named from inside `List<T>` -- to that definition's own member.
///
/// # Why this is a lookup through the SAME index the `TypeRef` arm uses
///
/// The member being named is an ordinary `MethodDef` of an ordinary `TypeDef`, already loaded and
/// already in `index` -- method loading inserts every method under `name_key` before this pass runs.
/// The only thing the `TypeSpec` spelling changes is where the DECLARING TYPE's name comes from: the
/// `GenericInst`'s definition rather than the parent token itself. Everything after that is the
/// existing rule, so it is called rather than restated.
///
/// # What it deliberately does NOT do
///
/// It binds only what it can name. A definition token that is not a `TypeDef`/`TypeRef` this
/// assembly can spell, or a member the index does not hold, leaves the token UNBOUND -- and an
/// unbound call token is refused by the bake as an `UnresolvedCall`. That is the safe direction:
/// the failure of a missed bind here is loud, exactly as [`monomorphize`]'s synthetic tokens are.
fn bind_own_generic_member(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    index: &NameIndex,
    spec: Option<&lamella_metadata::SigType>,
    method_name: &str,
    params: &[SigType],
    return_type: Option<&SigType>,
    token: Token,
) {
    let Some(lamella_metadata::SigType::GenericInst { definition, .. }) = spec else {
        return;
    };
    let definition_token = match definition.as_ref() {
        lamella_metadata::SigType::Class(token) | lamella_metadata::SigType::ValueType(token) => {
            *token
        }
        _ => return,
    };
    let Some((namespace, type_name)) = assembly.type_token_full_name(definition_token) else {
        return;
    };
    let key = name_key(assembly, &namespace, &type_name, method_name, params, return_type);
    if let Some(&target) = index.get(&key) {
        module.bind_token(asm, token, target);
    }
}

/// Binds recognized instantiated BCL generic-method calls (a `MethodSpec` operand) to their
/// intrinsics, and MARKS every other one as an unlowered generic.
///
/// # The recognized ones, and why dropping their type arguments is sound
///
/// `Array.Empty<T>()` and `Interlocked.CompareExchange<T>(..)` are bound to intrinsics that never
/// see `T`. That is correct for these two rather than correct in general: an empty array is the
/// same object whatever its element type, and the interchange is over a reference slot. **The list
/// is the statement that these are the ones where `T` does not reach a value**, which is the same
/// property that makes `Echo<T>(T) -> T` a vacuous test fixture, and it does not generalize to a
/// method whose body names `!!0`.
///
/// # Why everything else is MARKED rather than left to refuse itself
///
/// An unrecognized `MethodSpec` binds to nothing, so without the mark the bake refuses its call
/// site as an ordinary `UnresolvedCall` -- **the same violation a call to a method that does not
/// exist produces.** The program is safe, but by an ABSENCE OF A BINDING rather than by a refusal
/// anyone wrote, and the distinction between "this generic call is not lowered" and "this method is
/// missing" is then visible only to a reader who decodes the token's table byte.
///
/// That arrangement holds exactly as long as no code path can bind a `MethodSpec`, and
/// [`monomorphize::lower_method_pairs`] is one. So the mark is written here, at the one place that
/// knows the token is a generic call, and withdrawn per token AFTER the bind -- the same contract
/// the type axis has, so a PARTIAL lowering stops the bake by name on either axis.
fn bind_generic_calls(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    tokens: &BTreeSet<Token>,
) {
    for token in tokens {
        if bind_recognized_generic_call(assembly, module, asm, *token) {
            continue;
        }
        if module.resolve(asm, *token).is_some() {
            continue;
        }
        module.mark_unlowered_generic(asm, *token);
    }
}

/// Binds one `MethodSpec` if it names a generic BCL method with an intrinsic. `true` when it did.
fn bind_recognized_generic_call(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    token: Token,
) -> bool {
    let Some(method_token) = assembly.method_spec_method(token) else {
        return false;
    };
    if method_token.table() != MEMBER_REF {
        return false;
    }
    let Some(member) = assembly.member_ref(method_token.row()) else {
        return false;
    };
    let parent = member.parent();
    if parent.table() != TYPE_REF {
        return false;
    }
    let Some((parent_namespace, parent_name)) = assembly.type_token_full_name(parent) else {
        return false;
    };
    let recognized: Option<((IntrinsicFn, u32), u16)> =
        match (parent_namespace.as_str(), parent_name.as_str(), member.name()) {
            ("System", "Array", Some("Empty")) => Some((intrinsic!(array_empty), 0)),
            ("System.Threading", "Interlocked", Some("CompareExchange")) => {
                Some((intrinsic!(interlocked_compare_exchange), 3))
            }
            _ => None,
        };
    let Some(((function, intr_id), arg_count)) = recognized else {
        return false;
    };
    let id = module.add_intrinsic(asm, function, intr_id, arg_count);
    module.bind_token(asm, token, id);
    true
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

/// Whether a same-assembly (corlib-internal) `newobj` of this type's `.ctor` should allocate a
/// NATIVE list instead of running the type's managed constructor. **No type answers yes**, and the
/// rest of this comment is why the list must stay empty rather than why it happens to be.
///
/// NAMING `ArrayList` / `Hashtable` / `Stack` / `Queue` HERE WOULD BE A HALF-CONVERSION.
/// Marking the ctor gives corlib-internal code a native list -- an instance with no managed fields
/// -- while a corlib-internal CALL to `Add` still resolves to the type's own managed body, which
/// begins by reading `size`. So construction succeeded and the first field read trapped, and the
/// same method worked when a program called it (a program's calls bind to the `list_*` intrinsics,
/// so a program gets native+native and is consistent).
///
/// All four types are complete from-scratch managed implementations with zero `[RuntimeProvided]`,
/// and `ArrayList`'s own module comment says it *"overrides the native intrinsic ArrayList because
/// the corlib resolves ahead of it"*. So the managed body is the intended implementation and the
/// native list is the legacy path it supersedes. **StringBuilder is the precedent: it was de-marked
/// here when it became fully managed, and these four simply never were.**
///
/// Kept as a named predicate rather than deleted, because the marking mechanism it fed is still
/// live for anything that genuinely IS native-constructed, and a future half-conversion should have
/// this doc to read.
fn same_assembly_list_ctor(_namespace: &str, _type_name: &str) -> bool {
    false
}

#[cfg(test)]
mod same_assembly_list_ctor_tests {
    /// The four collection types are NOT marked for native same-assembly construction.
    ///
    /// THIS IS A DECISION TEST, NOT A BEHAVIOR TEST, AND THE DIFFERENCE IS WHY IT IS HERE. The
    /// defect it guards is DORMANT: `mark_same_assembly_ctors` only marks a ctor that the assembly
    /// actually `newobj`s, and no corlib code constructs a collection internally today. So a test
    /// that loads the corlib and inspects the marking passes whether or not the names are listed --
    /// I wrote that test first, and only red-proving it showed it was measuring nothing.
    ///
    /// What CAN be pinned is the decision. Re-adding a name here fails this, which is the moment to
    /// re-read why it was removed: the managed body and a native instance disagree about whether
    /// the object has fields, and construction succeeds before the first field read traps.
    #[test]
    fn the_managed_collections_are_not_marked_for_native_construction() {
        for type_name in ["ArrayList", "Hashtable", "Stack", "Queue"] {
            assert!(
                !super::same_assembly_list_ctor("System.Collections", type_name),
                "{type_name} is marked for native same-assembly construction again. Its corlib                  implementation is fully managed, so a corlib-internal `new` would allocate an                  instance with no managed fields while a corlib-internal call ran the managed body                  that reads them. If it is genuinely native-constructed now, its methods must bind                  to the intrinsics on the SAME-ASSEMBLY path too, not only the cross-assembly one."
            );
        }
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
) -> Option<(IntrinsicFn, u32)> {
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
        return Some(intrinsic!(initialize_array));
    }
    if namespace == "System.Reflection" && type_name == "MemberInfo" && method == "get_Name" {
        return Some(intrinsic!(type_get_name));
    }
    #[cfg(feature = "NETMFv4_4")]
    if namespace == "System.Reflection" {
        match method {
            "op_Equality" => return Some(intrinsic!(reflect_handle_equals)),
            "op_Inequality" => return Some(intrinsic!(reflect_handle_not_equals)),
            "HandleEquals" => return Some(intrinsic!(reflect_handle_equals)),
            _ => {}
        }
    }
    if namespace == "System.Reflection"
        && type_name == "MemberInfo"
        && method == "GetCustomAttributes"
    {
        return match parameters_of(signature) {
            [SigType::Boolean] => Some(intrinsic!(get_custom_attributes)),
            _ => None,
        };
    }
    #[cfg(feature = "NETMFv4_4")]
    if namespace == "System.Reflection" && type_name == "FieldInfo" {
        match (method, parameters_of(signature)) {
            ("GetValue", [SigType::Object]) => return Some(intrinsic!(field_get_value)),
            ("SetValue", [SigType::Object, SigType::Object]) => return Some(intrinsic!(field_set_value)),
            ("get_FieldType", []) => return Some(intrinsic!(member_get_type)),
            ("get_IsLiteral", []) => return Some(intrinsic!(field_is_literal)),
            ("get_IsStatic", []) => return Some(intrinsic!(field_is_static)),
            ("GetRawConstantValue", []) => return Some(intrinsic!(field_get_raw_constant)),
            _ => {}
        }
    }
    #[cfg(feature = "NETMFv4_4")]
    if namespace == "System.Reflection"
        && (type_name == "MethodBase" || type_name == "MethodInfo")
        && method == "Invoke"
    {
        return Some(intrinsic!(method_invoke));
    }
    #[cfg(feature = "NETMFv4_4")]
    if namespace == "System.Reflection" && type_name == "ConstructorInfo" && method == "Invoke" {
        if let [SigType::SzArray(_)] = parameters_of(signature) {
            return Some(intrinsic!(constructor_invoke));
        }
    }
    #[cfg(feature = "NETMFv4_4")]
    if namespace == "System.Reflection" && type_name == "MethodInfo" && method == "get_ReturnType" {
        return Some(intrinsic!(member_get_type));
    }
    #[cfg(feature = "NETMFv4_4")]
    if namespace == "System.Reflection" && type_name == "MethodBase" {
        match method {
            "get_IsPublic" => return Some(intrinsic!(method_is_public)),
            "get_IsStatic" => return Some(intrinsic!(method_is_static)),
            "get_IsFinal" => return Some(intrinsic!(method_is_final)),
            "get_IsVirtual" => return Some(intrinsic!(method_is_virtual)),
            "get_IsAbstract" => return Some(intrinsic!(method_is_abstract)),
            "GetParameterCount" => return Some(intrinsic!(method_parameter_count)),
            "GetParameterType" => return Some(intrinsic!(method_parameter_type)),
            "GetParameterName" => return Some(intrinsic!(method_parameter_name)),
            "GetParameterCustomAttributes" => {
                return Some(intrinsic!(method_parameter_custom_attributes));
            }
            _ => {}
        }
    }
    #[cfg(feature = "NETMFv4_4")]
    if namespace == "System.Reflection" && type_name == "Assembly" {
        match method {
            "GetType" if matches!(parameters_of(signature), [SigType::String]) => {
                return Some(intrinsic!(assembly_get_type));
            }
            "get_FullName" => return Some(intrinsic!(assembly_full_name)),
            "GetTypes" => return Some(intrinsic!(assembly_get_types)),
            _ => {}
        }
    }
    if namespace == "System" && type_name == "IntPtr" {
        match method {
            "FromRawValue" => return Some(intrinsic!(intptr_from_raw_value)),
            "ToRawValue" => return Some(intrinsic!(intptr_to_raw_value)),
            _ => {}
        }
    }
    if namespace == "System" && type_name == "UIntPtr" {
        match method {
            "FromRawValue" => return Some(intrinsic!(intptr_from_raw_value)),
            "ToRawValue" => return Some(intrinsic!(intptr_to_raw_value)),
            _ => {}
        }
    }
    if namespace == "System.Runtime.InteropServices" && type_name == "Marshal" {
        match method {
            "__AllocHGlobal" => return Some(intrinsic!(marshal_alloc_hglobal)),
            "__FreeHGlobal" => return Some(intrinsic!(marshal_free_hglobal)),
            "__ReadByte" => return Some(intrinsic!(marshal_read_byte)),
            "__ReadInt16" => return Some(intrinsic!(marshal_read_int16)),
            "__ReadInt32" => return Some(intrinsic!(marshal_read_int32)),
            "__ReadInt64" => return Some(intrinsic!(marshal_read_int64)),
            "__WriteByte" => return Some(intrinsic!(marshal_write_byte)),
            "__WriteInt16" => return Some(intrinsic!(marshal_write_int16)),
            "__WriteInt32" => return Some(intrinsic!(marshal_write_int32)),
            "__WriteInt64" => return Some(intrinsic!(marshal_write_int64)),
            "SizeOf" => return Some(intrinsic!(marshal_size_of)),
            _ => {}
        }
    }
    if namespace == "Lamella.Hardware" && type_name == "Mmio" {
        match method {
            "Read32" => return Some(intrinsic!(mmio_read32)),
            "Write32" => return Some(intrinsic!(mmio_write32)),
            "Read8" => return Some(intrinsic!(mmio_read8)),
            "Write8" => return Some(intrinsic!(mmio_write8)),
            "Read16" => return Some(intrinsic!(mmio_read16)),
            "Write16" => return Some(intrinsic!(mmio_write16)),
            _ => {}
        }
    }
    if namespace == "Lamella.Runtime" && type_name == "Clock" {
        match (method, parameters_of(signature)) {
            ("SetTicks", [SigType::I8]) => return Some(intrinsic!(clock_set_ticks)),
            ("IsSet", []) => return Some(intrinsic!(clock_is_set)),
            _ => {}
        }
    }
    if namespace == "System.Diagnostics"
        && type_name == "DefaultTraceListener"
        && method == "DebugWrite"
    {
        return match parameters_of(signature) {
            [SigType::String] => Some(intrinsic!(debug_write)),
            _ => None,
        };
    }
    if namespace == "System.Threading" && type_name == "Thread" {
        match method {
            "StartThread" => return Some(intrinsic!(thread_start)),
            "JoinThread" => return Some(intrinsic!(thread_join)),
            "YieldThread" => return Some(intrinsic!(thread_yield)),
            "SleepThread" => return Some(intrinsic!(thread_sleep)),
            _ => {}
        }
    }
    if namespace == "System.Threading" && type_name == "Monitor" {
        match method {
            "EnterLock" => return Some(intrinsic!(monitor_enter)),
            "ExitLock" => return Some(intrinsic!(monitor_exit)),
            "TryEnterLock" => return Some(intrinsic!(monitor_try_enter)),
            "TryEnterLockTimeout" => return Some(intrinsic!(monitor_try_enter_timeout)),
            "WaitLock" => return Some(intrinsic!(monitor_wait)),
            "WaitLockTimeout" => return Some(intrinsic!(monitor_wait_timeout)),
            "WaitTimedOut" => return Some(intrinsic!(monitor_wait_timed_out)),
            "PulseLock" => return Some(intrinsic!(monitor_pulse)),
            "PulseAllLock" => return Some(intrinsic!(monitor_pulse_all)),
            _ => {}
        }
    }
    if namespace == "System.Net.Sockets" && type_name == "Socket" {
        match method {
            "ConnectStart" => return Some(intrinsic!(socket_connect_start)),
            "ConnectPoll" => return Some(intrinsic!(socket_connect_poll)),
            "ListenStart" => return Some(intrinsic!(socket_listen)),
            "AcceptPoll" => return Some(intrinsic!(socket_accept)),
            "SendPoll" => return Some(intrinsic!(socket_send)),
            "ReceivePoll" => return Some(intrinsic!(socket_recv)),
            "SetRecvTimeout" => return Some(intrinsic!(socket_set_recv_timeout)),
            "LocalPort" => return Some(intrinsic!(socket_local_port)),
            "CloseSocket" => return Some(intrinsic!(socket_close)),
            "UdpBind" => return Some(intrinsic!(socket_udp_bind)),
            "UdpSendTo" => return Some(intrinsic!(socket_udp_send_to)),
            "UdpReceiveFrom" => return Some(intrinsic!(socket_udp_recv_from)),
            _ => {}
        }
    }
    if namespace == "System.Net" && type_name == "Dns" {
        match method {
            "ResolveHost" => return Some(intrinsic!(dns_resolve_host)),
            _ => {}
        }
    }
    if namespace == "System.Net.NetworkInformation" && type_name == "NetworkInterface" {
        match method {
            "NetworkAvailable" => return Some(intrinsic!(net_is_available)),
            "InterfaceCount" => return Some(intrinsic!(net_iface_count)),
            "OperStatus" => return Some(intrinsic!(net_iface_oper_status)),
            "IfaceType" => return Some(intrinsic!(net_iface_type)),
            "IPv4" => return Some(intrinsic!(net_iface_ipv4)),
            "Ipv4Mask" => return Some(intrinsic!(net_iface_subnet)),
            "Ipv4Gateway" => return Some(intrinsic!(net_iface_gateway)),
            "IfaceFlags" => return Some(intrinsic!(net_iface_flags)),
            _ => {}
        }
    }
    if namespace == "System.Net.Security" && type_name == "TlsNative" {
        match method {
            "ClientConfig" => return Some(intrinsic!(tls_client_config)),
            "ServerConfig" => return Some(intrinsic!(tls_server_config)),
            "ClientNew" => return Some(intrinsic!(tls_client_new)),
            "ServerNew" => return Some(intrinsic!(tls_server_new)),
            "Process" => return Some(intrinsic!(tls_process)),
            "WantsWrite" => return Some(intrinsic!(tls_wants_write)),
            "WriteTls" => return Some(intrinsic!(tls_write_tls)),
            "ReadTls" => return Some(intrinsic!(tls_read_tls)),
            "ReadPlain" => return Some(intrinsic!(tls_read_plain)),
            "WritePlain" => return Some(intrinsic!(tls_write_plain)),
            "PeerCert" => return Some(intrinsic!(tls_peer_cert)),
            "SessionFlags" => return Some(intrinsic!(tls_session_flags)),
            "CloseTls" => return Some(intrinsic!(tls_close)),
            "DefaultStack" => return Some(intrinsic!(tls_default_stack)),
            _ => {}
        }
    }
    if namespace == "Lamella.Net.Time" && type_name == "NtsNative" {
        match method {
            "ClientConfigAlpn" => return Some(intrinsic!(tls_client_config_alpn)),
            "ClientNew" => return Some(intrinsic!(tls_client_new)),
            "Process" => return Some(intrinsic!(tls_process)),
            "WantsWrite" => return Some(intrinsic!(tls_wants_write)),
            "WriteTls" => return Some(intrinsic!(tls_write_tls)),
            "ReadTls" => return Some(intrinsic!(tls_read_tls)),
            "ReadPlain" => return Some(intrinsic!(tls_read_plain)),
            "WritePlain" => return Some(intrinsic!(tls_write_plain)),
            "CloseTls" => return Some(intrinsic!(tls_close)),
            "DefaultStack" => return Some(intrinsic!(tls_default_stack)),
            "AlpnIs" => return Some(intrinsic!(tls_alpn_is)),
            "ExporterKey" => return Some(intrinsic!(tls_exporter_key)),
            "DropKey" => return Some(intrinsic!(tls_drop_key)),
            "SivEncrypt" => return Some(intrinsic!(aead_siv_encrypt)),
            "SivDecrypt" => return Some(intrinsic!(aead_siv_decrypt)),
            "ImportKey" => return Some(intrinsic!(aead_import_key)),
            _ => {}
        }
    }
    #[cfg(feature = "varargs")]
    if namespace == "System" && type_name == "ArgIteratorNative" {
        match method {
            "Cookie" => return Some(intrinsic!(arg_iterator_cookie)),
            "RemainingCount" => return Some(intrinsic!(arg_iterator_remaining)),
            "GetArg" => return Some(intrinsic!(arg_iterator_get)),
            _ => {}
        }
    }
    if namespace == "System.IO" && type_name == "NativeFs" {
        match method {
            "Open" => return Some(intrinsic!(fs_open)),
            "Read" => return Some(intrinsic!(fs_read)),
            "Write" => return Some(intrinsic!(fs_write)),
            "Seek" => return Some(intrinsic!(fs_seek)),
            "Length" => return Some(intrinsic!(fs_length)),
            "SetLength" => return Some(intrinsic!(fs_set_length)),
            "Flush" => return Some(intrinsic!(fs_flush)),
            "Close" => return Some(intrinsic!(fs_close)),
            "FileExists" => return Some(intrinsic!(fs_file_exists)),
            "DirExists" => return Some(intrinsic!(fs_dir_exists)),
            "DeleteFile" => return Some(intrinsic!(fs_delete_file)),
            "CreateDir" => return Some(intrinsic!(fs_create_dir)),
            "DeleteDir" => return Some(intrinsic!(fs_delete_dir)),
            "Move" => return Some(intrinsic!(fs_move)),
            "List" => return Some(intrinsic!(fs_list)),
            _ => {}
        }
    }
    if namespace == "System.IO" && type_name == "NativeDrive" {
        match method {
            "Names" => return Some(intrinsic!(drive_names)),
            "Kind" => return Some(intrinsic!(drive_kind)),
            "TotalSize" => return Some(intrinsic!(drive_total_size)),
            "Format" => return Some(intrinsic!(drive_format)),
            "FileSystems" => return Some(intrinsic!(drive_filesystems)),
            "MountRemovableVolumes" => return Some(intrinsic!(drive_mount_removable)),
            _ => {}
        }
    }
    if namespace == "Lamella.IO" && type_name == "NativeStorage" {
        match method {
            "MountRam" => return Some(intrinsic!(storage_mount_ram)),
            "MountSdOverSpi" => return Some(intrinsic!(storage_mount_sd_over_spi)),
            "Unmount" => return Some(intrinsic!(storage_unmount)),
            "IsMounted" => return Some(intrinsic!(storage_is_mounted)),
            _ => {}
        }
    }
    if namespace == "nanoFramework.System.IO.FileSystem" && type_name == "NativeSdCard" {
        match method {
            "MountSdOverSpiBus" => return Some(intrinsic!(storage_mount_sd_over_spi_bus)),
            "Unmount" => return Some(intrinsic!(storage_unmount)),
            "IsMounted" => return Some(intrinsic!(storage_is_mounted)),
            _ => {}
        }
    }
    if namespace == "System.IO.Ports" && type_name == "NativeSerial" {
        match method {
            "Open" => return Some(intrinsic!(serial_open)),
            "Read" => return Some(intrinsic!(serial_read)),
            "Write" => return Some(intrinsic!(serial_write)),
            "BytesToRead" => return Some(intrinsic!(serial_bytes_to_read)),
            "BytesToWrite" => return Some(intrinsic!(serial_bytes_to_write)),
            "Flush" => return Some(intrinsic!(serial_flush)),
            "DiscardIn" => return Some(intrinsic!(serial_discard_in)),
            "DiscardOut" => return Some(intrinsic!(serial_discard_out)),
            "Close" => return Some(intrinsic!(serial_close)),
            _ => {}
        }
    }
    if namespace == "System" && type_name == "Environment" {
        match method {
            "get_TickCount" => return Some(intrinsic!(environment_tick_count)),
            "get_ProcessorCount" => return Some(intrinsic!(environment_processor_count)),
            "GetEnvironmentVariable" => return Some(intrinsic!(environment_get_variable)),
            _ => {}
        }
    }
    if namespace != "System" {
        return None;
    }
    let base: Option<(IntrinsicFn, u32)> = match (type_name, method) {
        ("Console", "WriteLine") => console_write_line_overload(signature),
        ("Console", "Write") => console_write_overload(signature),
        ("String", "Concat") => string_concat_overload(signature),
        ("String", "get_Length") => string_get_length_overload(signature),
        ("String", "get_Chars") => string_get_chars_overload(signature),
        ("String", "GetPinnableReference") => match parameters_of(signature) {
            [] => Some(intrinsic!(string_get_pinnable_reference)),
            _ => None,
        },
        ("String", ".ctor") => string_ctor_overload(signature),
        ("String", "op_Equality") => string_equals_overload(signature),
        ("String", "op_Inequality") => string_not_equals_overload(signature),
        ("String", "IsNullOrEmpty") => string_is_null_or_empty_overload(signature),
        ("String", "InternCore") => Some(intrinsic!(string_intern)),
        ("String", "IsInternedCore") => Some(intrinsic!(string_is_interned)),
        ("String", "Substring") => string_substring_overload(signature),
        ("String", "CreateFromChars") => match parameters_of(signature) {
            [SigType::SzArray(_), SigType::I4, SigType::I4] => Some(intrinsic!(string_create_from_chars)),
            _ => None,
        },
        ("Object", ".ctor") => object_ctor_overload(signature),
        ("Object", "ReferenceEquals") => match parameters_of(signature) {
            [SigType::Object, SigType::Object] => Some(intrinsic!(object_reference_equals)),
            _ => None,
        },
        ("Object", "Finalize") => Some(intrinsic!(object_ctor)),
        ("Object", "GetType") => match parameters_of(signature) {
            [] => Some(intrinsic!(object_get_type)),
            _ => None,
        },
        ("Exception", ".ctor") => Some(intrinsic!(exception_ctor)),
        ("Exception", "get_Message") => Some(intrinsic!(exception_get_message)),
        ("Exception", "RuntimeMessage") => Some(intrinsic!(exception_runtime_message)),
        #[cfg(feature = "finalizers")]
        ("GC", "SuppressFinalize") => Some(intrinsic!(suppress_finalize)),
        #[cfg(feature = "finalizers")]
        ("GC", "ReRegisterForFinalize") => Some(intrinsic!(reregister_finalize)),
        #[cfg(feature = "gc")]
        ("GC", "Collect") => Some(intrinsic!(gc_collect)),
        #[cfg(feature = "finalizers")]
        ("GC", "WaitForPendingFinalizers") => Some(intrinsic!(wait_for_pending_finalizers)),
        #[cfg(feature = "gc")]
        ("WeakReference", "MakeWeakCell") => Some(intrinsic!(weak_make_cell)),
        #[cfg(feature = "gc")]
        ("WeakReference", "ReadWeakCell") => Some(intrinsic!(weak_read_cell)),
        #[cfg(feature = "gc")]
        ("WeakReference", "WriteWeakCell") => Some(intrinsic!(weak_write_cell)),
        ("Type", "GetTypeFromHandle") => Some(intrinsic!(type_from_handle)),
        ("Type", "get_Name") => Some(intrinsic!(type_get_name)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_FullName") => Some(intrinsic!(type_get_full_name)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_Namespace") => Some(intrinsic!(type_get_namespace)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_Assembly") => Some(intrinsic!(type_get_assembly)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_BaseType") => Some(intrinsic!(type_get_base_type)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_IsEnum") => Some(intrinsic!(type_is_enum)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_IsValueType") => Some(intrinsic!(type_is_value_type)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_IsClass") => Some(intrinsic!(type_is_class)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_IsInterface") => Some(intrinsic!(type_is_interface)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_IsAbstract") => Some(intrinsic!(type_is_abstract)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_IsPublic") => Some(intrinsic!(type_is_public)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_IsNotPublic") => Some(intrinsic!(type_is_not_public)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "get_IsArray") => Some(intrinsic!(type_is_array)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "op_Equality") => Some(intrinsic!(reflect_handle_equals)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "op_Inequality") => Some(intrinsic!(reflect_handle_not_equals)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "HandleEquals") => Some(intrinsic!(reflect_handle_equals)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "GetField") => match parameters_of(signature) {
            [SigType::String] | [SigType::String, _] => Some(intrinsic!(type_get_field)),
            _ => None,
        },
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "GetFields") => Some(intrinsic!(type_get_fields)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "GetMethods") => Some(intrinsic!(type_get_methods)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "GetMethod") => match parameters_of(signature) {
            [SigType::String] | [SigType::String, _] => Some(intrinsic!(type_get_method)),
            _ => None,
        },
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "GetProperty") => match parameters_of(signature) {
            [SigType::String] | [SigType::String, _] => Some(intrinsic!(type_get_property)),
            _ => None,
        },
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "GetPropertyCustomAttributes") => Some(intrinsic!(type_property_custom_attributes)),
        #[cfg(feature = "NETMFv4_4")]
        ("Type", "GetConstructor") => match parameters_of(signature) {
            [SigType::SzArray(_)] => Some(intrinsic!(type_get_constructor)),
            _ => None,
        },
        #[cfg(feature = "NETMFv4_4")]
        ("Activator", "CreateInstance") => match parameters_of(signature) {
            [_] => Some(intrinsic!(activator_create_instance)),
            _ => None,
        },
        ("Enum", "Parse") => Some(intrinsic!(enum_parse)),
        ("Enum", "IsDefined") => Some(intrinsic!(enum_is_defined)),
        ("Enum", "GetName") => Some(intrinsic!(enum_get_name)),
        ("Enum", "GetNames") => Some(intrinsic!(enum_get_names)),
        ("Enum", "GetValues") => Some(intrinsic!(enum_get_values)),
        ("Enum", "Format") => Some(intrinsic!(enum_format)),
        ("Enum", "HasFlag") => Some(intrinsic!(enum_has_flag)),
        ("Enum", "ToString") => match parameters_of(signature) {
            [SigType::String] => Some(intrinsic!(enum_to_string_format)),
            _ => None,
        },
        ("Array", "get_Length") => Some(intrinsic!(md_array_length)),
        ("Array", "GetLength") => Some(intrinsic!(md_array_get_length)),
        ("Array", "get_Rank") => Some(intrinsic!(array_rank)),
        ("Array", "ClearCore") => Some(intrinsic!(array_clear_range)),
        ("Array", "CopyCore") => Some(intrinsic!(array_copy_range)),
        ("Array", "GetValue") => match parameters_of(signature) {
            [SigType::I4] => Some(intrinsic!(array_get_value)),
            _ => None,
        },
        ("Array", "SetValue") => match parameters_of(signature) {
            [SigType::Object, SigType::I4] => Some(intrinsic!(array_set_value)),
            _ => None,
        },
        ("Array", "Clone") => match parameters_of(signature) {
            [] => Some(intrinsic!(array_clone)),
            _ => None,
        },
        ("Buffer", "BlockCopyInternal") => match parameters_of(signature) {
            [SigType::Class(_), SigType::I4, SigType::Class(_), SigType::I4, SigType::I4] => {
                Some(intrinsic!(buffer_block_copy))
            }
            _ => None,
        },
        ("Buffer", "ByteLengthInternal") => match parameters_of(signature) {
            [SigType::Class(_)] => Some(intrinsic!(buffer_byte_length)),
            _ => None,
        },
        ("Int32", "ToString") => to_string_overload(intrinsic!(int32_to_string), signature),
        ("Byte" | "SByte" | "Int16" | "UInt16", "ToString") => {
            to_string_overload(intrinsic!(int32_to_string), signature)
        }
        ("Boolean", "ToString") => to_string_overload(intrinsic!(boolean_to_string), signature),
        ("Char", "ToString") => to_string_overload(intrinsic!(char_to_string), signature),
        ("Int64", "ToString") => to_string_overload(intrinsic!(int64_to_string), signature),
        #[cfg(feature = "float")]
        ("Double", "ToString") => to_string_overload(intrinsic!(double_to_string), signature),
        #[cfg(feature = "float")]
        ("Double", "ToFixed") => match parameters_of(signature) {
            [SigType::R8, SigType::I4] => Some(intrinsic!(double_to_fixed)),
            _ => None,
        },
        #[cfg(feature = "float")]
        ("Double", "ToExponential") => match parameters_of(signature) {
            [SigType::R8, SigType::I4, SigType::Boolean] => Some(intrinsic!(double_to_exponential)),
            _ => None,
        },
        #[cfg(feature = "float")]
        ("Double", "ParseValid") => match parameters_of(signature) {
            [SigType::String] => Some(intrinsic!(double_parse)),
            _ => None,
        },
        #[cfg(feature = "float")]
        ("Single", "ToString") => to_string_overload(intrinsic!(single_to_string), signature),
        #[cfg(feature = "float")]
        ("Single", "ToFixed") => match parameters_of(signature) {
            [SigType::R4, SigType::I4] => Some(intrinsic!(single_to_fixed)),
            _ => None,
        },
        #[cfg(feature = "float")]
        ("Single", "ToExponential") => match parameters_of(signature) {
            [SigType::R4, SigType::I4, SigType::Boolean] => Some(intrinsic!(single_to_exponential)),
            _ => None,
        },
        #[cfg(feature = "float")]
        ("Single", "ParseValid") => match parameters_of(signature) {
            [SigType::String] => Some(intrinsic!(single_parse)),
            _ => None,
        },
        ("Object", "ToString") => to_string_overload(intrinsic!(object_to_string), signature),
        ("Delegate", "Combine") => Some(intrinsic!(delegate_combine)),
        ("Delegate", "Remove") => Some(intrinsic!(delegate_remove)),
        ("Delegate", "op_Equality") | ("MulticastDelegate", "op_Equality") => {
            Some(intrinsic!(delegate_equals))
        }
        ("Delegate", "op_Inequality") | ("MulticastDelegate", "op_Inequality") => {
            Some(intrinsic!(delegate_not_equals))
        }
        ("DateTime", "NowTicks") => match parameters_of(signature) {
            [] => Some(intrinsic!(datetime_now_ticks)),
            _ => None,
        },
        #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
        ("BitConverter", "DoubleToInt64Bits") => match parameters_of(signature) {
            [SigType::R8] => Some(intrinsic!(bitconverter_double_to_int64_bits)),
            _ => None,
        },
        #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
        ("BitConverter", "Int64BitsToDouble") => match parameters_of(signature) {
            [SigType::I8] => Some(intrinsic!(bitconverter_int64_bits_to_double)),
            _ => None,
        },
        #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
        ("BitConverter", "SingleToInt32Bits") => match parameters_of(signature) {
            [SigType::R4] => Some(intrinsic!(bitconverter_single_to_int32_bits)),
            _ => None,
        },
        #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
        ("BitConverter", "Int32BitsToSingle") => match parameters_of(signature) {
            [SigType::I4] => Some(intrinsic!(bitconverter_int32_bits_to_single)),
            _ => None,
        },
        ("Decimal", "op_Addition") => decimal_binary_op(intrinsic!(decimal_add), signature),
        ("Decimal", "op_Subtraction") => decimal_binary_op(intrinsic!(decimal_subtract), signature),
        ("Decimal", "op_Multiply") => decimal_binary_op(intrinsic!(decimal_multiply), signature),
        ("Decimal", "op_Division") => decimal_binary_op(intrinsic!(decimal_divide), signature),
        ("Decimal", "op_Modulus") => decimal_binary_op(intrinsic!(decimal_remainder), signature),
        ("Decimal", "DecAdd") => decimal_binary_op(intrinsic!(decimal_add), signature),
        ("Decimal", "DecSub") => decimal_binary_op(intrinsic!(decimal_subtract), signature),
        ("Decimal", "DecMul") => decimal_binary_op(intrinsic!(decimal_multiply), signature),
        ("Decimal", "DecDiv") => decimal_binary_op(intrinsic!(decimal_divide), signature),
        ("Decimal", "DecRem") => decimal_binary_op(intrinsic!(decimal_remainder), signature),
        ("Decimal", "Compare") => decimal_binary_op(intrinsic!(decimal_compare), signature),
        #[cfg(feature = "float")]
        ("Decimal", "FromDouble") => match parameters_of(signature) {
            [SigType::R8] => Some(intrinsic!(decimal_from_double)),
            _ => None,
        },
        #[cfg(feature = "float")]
        ("Decimal", "ToDouble") => match parameters_of(signature) {
            [SigType::ValueType(_)] => Some(intrinsic!(decimal_to_double)),
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
fn object_ctor_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
    match parameters_of(signature) {
        [] => Some(intrinsic!(object_ctor)),
        _ => None,
    }
}

/// [`default_field_value`], plus the one value type whose zero is NOT a null reference: an ENUM.
///
/// An enum IS its underlying integral type (ECMA-335 II.14.3), so a field of enum type that nobody
/// assigns holds that type's zero -- which is the whole reason C# needs no initializer for
/// `private Sampling _mode;` and emits CS0649 as a warning rather than an error. Left as null it
/// fails in both directions at once: `_mode == Sampling.Skipped` compares a null against an
/// `Int32(0)` and answers FALSE with nothing reported, and `(int)_mode` traps on `conv.i4`. The
/// silent half is the dangerous one -- a driver reads "this is not Skipped" for a field that is.
///
/// Non-enum value types still fall back to null: they are not laid out inline yet, and handing a
/// struct field a numeric zero would be a different wrong answer rather than a fix.
fn default_field_value_of(
    assembly: &Assembly,
    signature: Option<SigType>,
    enum_zeros: &BTreeMap<String, Value>,
) -> Value {
    if let Some(SigType::ValueType(token)) = signature {
        if let Some(name) = assembly.type_token_name(token) {
            if let Some(zero) = enum_zeros.get(&type_name_key(name)) {
                return zero.clone();
            }
        }
    }
    default_field_value(signature)
}

/// Record every enum this assembly DECLARES against the zero its storage takes, so a field typed
/// with one gets that zero rather than a null reference. Runs before the field walk, because a
/// field may name a type declared later in the same assembly.
fn index_enum_zeros(assembly: &Assembly, enum_zeros: &mut BTreeMap<String, Value>) {
    for type_def in assembly.type_defs() {
        if !is_enum_type(assembly, type_def.extends()) {
            continue;
        }
        let Some((namespace, name)) = key_type_name(assembly, &type_def) else {
            continue;
        };
        let underlying = type_def
            .fields()
            .find(|field| !field.is_static())
            .and_then(|field| field.signature());
        let zero = match default_field_value(underlying) {
            Value::Null => Value::Int32(0),
            zero => zero,
        };
        enum_zeros.insert(alloc::format!("{namespace}.{name}"), zero);
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
        Some(SigType::R4) => Value::Single(0.0),
        #[cfg(feature = "float")]
        Some(SigType::R8) => Value::Float(0.0),
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

/// Binds each `newarr` element-type token to its elements' zero value, and -- for a sized
/// primitive element type -- the element type's [`PrimKind`], so `newarr` packs the array's
/// element storage at the true byte width (and `System.Buffer` can size its byte image; a
/// `byte[]` and an `int[]` are otherwise indistinguishable on the stack).
fn bind_array_defaults(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    type_index: &TypeNameIndex,
    enum_zeros: &BTreeMap<String, Value>,
    tokens: &BTreeSet<Token>,
) {
    for token in tokens {
        let default = array_element_default(assembly, *token, enum_zeros);
        module.bind_array_default(asm, *token, default);
        if let Some(kind) = assembly
            .type_token_name(*token)
            .and_then(|name| array_prim_kind_of(name.namespace, name.name))
        {
            module.bind_array_prim_kind(asm, *token, kind);
        }
        if module.type_id_of(asm, *token).is_none() {
            if let Some(name) = assembly.type_token_name(*token) {
                if let Some(id) = type_index.get(&type_name_key(name)).copied() {
                    module.bind_type_token(asm, *token, id);
                }
            }
        }
    }
}

/// Records, for each `box` operand type token naming `System.Boolean` or `System.Char`, the
/// [`BoxedPrimitive`] display kind -- so a boxed `bool`/`char` displays as `"True"`/`"False"` or
/// its character rather than the raw `Int32` the stack collapses both into. The kind comes from the
/// operand's metadata NAME ([`Assembly::type_token_name`]), which resolves even for a corlib
/// `TypeRef` the runtime never loads (the incremental REPL's case); no other boxed type is recorded
/// (a plain integer, enum, or struct box keeps its normal display path).
fn bind_box_primitives(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    tokens: &BTreeSet<Token>,
) {
    for token in tokens {
        if let Some(kind) = assembly
            .type_token_name(*token)
            .and_then(|name| box_primitive_kind(name.namespace, name.name))
        {
            module.bind_box_primitive(asm, *token, kind);
        }
    }
}

/// The [`BoxedPrimitive`] display kind of a boxed `System` primitive type name, or `None` for any
/// type whose boxed display already matches its underlying `Int32` (every other integer primitive)
/// or is handled elsewhere. Only `Boolean` and `Char` differ from the raw integer, so only they are
/// classified.
fn box_primitive_kind(namespace: &str, name: &str) -> Option<BoxedPrimitive> {
    if namespace != "System" {
        return None;
    }
    match name {
        "Boolean" => Some(BoxedPrimitive::Boolean),
        "Char" => Some(BoxedPrimitive::Char),
        _ => None,
    }
}

/// The [`PrimKind`] of a sized primitive element type -- the .NET "primitive of fixed size"
/// set (the integer types, `Char`, `Boolean`, `Single`, `Double`), which `newarr` packs and
/// `System.Buffer.BlockCopy` / `ByteLength` accept. `None` for anything else, including
/// `IntPtr`/`UIntPtr` (whose size is not fixed and which `System.Buffer` rejects), references,
/// and value types -- such an array keeps boxed element storage, and a `Buffer` call over it
/// raises `ArgumentException`. The kind's read canonicalization mirrors
/// `array_element_default`'s widening (a `Byte` element reads back as the zero-extended
/// `Int32` its stack value is).
fn array_prim_kind_of(namespace: &str, name: &str) -> Option<PrimKind> {
    if namespace != "System" {
        return None;
    }
    Some(match name {
        "Boolean" | "Byte" => PrimKind::U1,
        "SByte" => PrimKind::I1,
        "Int16" => PrimKind::I2,
        "UInt16" | "Char" => PrimKind::U2,
        "Int32" | "UInt32" => PrimKind::I4,
        "Single" => PrimKind::F4,
        "Int64" | "UInt64" => PrimKind::I8,
        "Double" => PrimKind::F8,
        _ => return None,
    })
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

/// Marks a same-assembly (`MethodDef`) `newobj` of a `System.Collections` list `.ctor` declared in
/// this assembly, so a corlib-INTERNAL `new ArrayList()` allocates a native list at construction.
/// `bind_bcl_calls` already covers the cross-assembly (`MemberRef`) form for a program's `newobj`;
/// this is the same-assembly analog (modelled on [`mark_value_type_ctors`]). The per-row arity was
/// captured as the type's methods were walked. (StringBuilder is fully managed now, so it is no
/// longer marked here -- its ctor runs the managed body.)
fn mark_same_assembly_ctors(
    module: &mut Module,
    asm: u8,
    newobj_tokens: &BTreeSet<Token>,
    list_ctor_rows: &BTreeMap<u32, u16>,
) {
    for token in newobj_tokens {
        if token.table() != METHOD_DEF {
            continue;
        }
        if let Some(&params) = list_ctor_rows.get(&token.row()) {
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
/// [`lamella_metadata::Assembly::type_token_full_name`]) and the field from the member name; the
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
        let (Some((declaring_namespace, declaring_name)), Some(field_name)) =
            (assembly.type_token_full_name(member.parent()), member.name())
        else {
            continue;
        };
        let key = field_key(&declaring_namespace, &declaring_name, field_name);
        if let Some(&slot) = field_index.statics.get(&key) {
            module.bind_static_field_ref(asm, *token, slot);
        }
    }
}

/// Binds a program's `ldfld`/`stfld`/`ldflda` `MemberRef` to another assembly's INSTANCE field, by
/// qualified name through `field_index`.
///
/// The instance twin of [`bind_static_field_refs`], and it exists for the same reason: the two
/// assemblies' tokens are unrelated, so only the name can match them. It went unwritten because
/// nothing needed it -- the corlib and the libraries above it expose behaviour through properties
/// and methods, and a `MemberRef` to a public FIELD in another assembly first appears with the
/// compatibility surfaces, whose parameter objects are declared as bare public fields and must stay
/// that way to be what an app was compiled against.
fn bind_instance_field_refs(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    field_index: &FieldNameIndex,
    type_index: &TypeNameIndex,
    tokens: &BTreeSet<Token>,
) {
    for token in tokens {
        if module.field_slot(asm, *token).is_some() {
            continue;
        }
        let Some(member) = assembly.member_ref(token.row()) else {
            continue;
        };
        if !member.is_field() {
            continue;
        }
        let (Some((declaring_namespace, declaring_name)), Some(field_name)) =
            (assembly.type_token_full_name(member.parent()), member.name())
        else {
            continue;
        };
        let key = field_key(&declaring_namespace, &declaring_name, field_name);
        if let Some(&slot) = field_index.instances.get(&key) {
            module.bind_field(asm, *token, slot);
            if let Some(&type_id) = type_index.get(&type_key(&declaring_namespace, &declaring_name)) {
                module.bind_field_type(asm, *token, type_id);
            }
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

/// Resolves a field's or method-return `SigType` to the asm-folded handle of its `Type`, for
/// `FieldInfo.FieldType` / `MethodInfo.ReturnType`. Primitives / string / object / void resolve by
/// name through the type index (to the corlib type); a same-assembly class/struct uses its folded
/// `TypeDef` token; a cross-assembly `TypeRef` resolves by name. Arrays / pointers / by-refs are not
/// modeled as `Type` handles yet (`None`).
fn sigtype_to_type_handle(
    assembly: &Assembly,
    module: &Module,
    asm: u8,
    sig: &SigType,
    type_index: &TypeNameIndex,
) -> Option<u64> {
    let by_name = |namespace: &str, name: &str| {
        let key = if namespace.is_empty() {
            String::from(name)
        } else {
            alloc::format!("{namespace}.{name}")
        };
        module
            .type_handle_by_name(&key)
            .or_else(|| type_index.get(&key).and_then(|id| module.type_handle_of(*id)))
    };
    match sig {
        SigType::Boolean => by_name("System", "Boolean"),
        SigType::Char => by_name("System", "Char"),
        SigType::I1 => by_name("System", "SByte"),
        SigType::U1 => by_name("System", "Byte"),
        SigType::I2 => by_name("System", "Int16"),
        SigType::U2 => by_name("System", "UInt16"),
        SigType::I4 => by_name("System", "Int32"),
        SigType::U4 => by_name("System", "UInt32"),
        SigType::I8 => by_name("System", "Int64"),
        SigType::U8 => by_name("System", "UInt64"),
        SigType::R4 => by_name("System", "Single"),
        SigType::R8 => by_name("System", "Double"),
        SigType::IntPtr => by_name("System", "IntPtr"),
        SigType::UIntPtr => by_name("System", "UIntPtr"),
        SigType::String => by_name("System", "String"),
        SigType::Object => by_name("System", "Object"),
        SigType::Void => by_name("System", "Void"),
        SigType::Class(token) | SigType::ValueType(token) => {
            if token.0 >> 24 == u32::from(TYPE_DEF) {
                Some(asm_key(asm, token.0))
            } else {
                assembly
                    .type_token_name(*token)
                    .and_then(|name| by_name(name.namespace, name.name))
            }
        }
        _ => None,
    }
}

/// Records, for every target in this assembly, the custom attributes applied to it (decoded
/// and resolved to a runtime form) and the member-name maps `Type.GetField`/`GetMethod`/
/// `GetProperty` resolve through. For a type and each of its fields, methods, and properties,
/// each applied attribute whose constructor is an instantiable same-module method becomes a
/// [`LoadedAttribute`] keyed by the target's asm-folded token (the `Type` / `MemberInfo` handle a
/// `GetCustomAttributes` receiver carries). Framework/compiler attributes (whose ctor is a
/// cross-assembly `MemberRef` that resolves to no `MethodId`) are skipped. The member-name maps
/// let an accessor map a name to the member handle whose attributes then read here.
fn record_custom_attributes(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    #[cfg_attr(not(feature = "NETMFv4_4"), allow(unused_variables))] type_index: &TypeNameIndex,
) {
    for (local_index, type_def) in assembly.type_defs().enumerate() {
        let type_token = Token::new(TYPE_DEF, (local_index + 1) as u32);
        let type_handle = asm_key(asm, type_token.0);
        record_target_attributes(assembly, module, asm, type_handle, type_token);
        #[cfg(feature = "NETMFv4_4")]
        if let Some(type_name) = type_def.name() {
            module.bind_type_name(asm, type_token, String::from(type_name.name));
            let full_name = if type_name.namespace.is_empty() {
                String::from(type_name.name)
            } else {
                alloc::format!("{}.{}", type_name.namespace, type_name.name)
            };
            let is_enum = assembly
                .type_token_name(type_def.extends())
                .is_some_and(|base| base.namespace == "System" && base.name == "Enum");
            let base_handle = {
                let extends = type_def.extends();
                if extends.0 == 0 {
                    0
                } else if extends.0 >> 24 == u32::from(TYPE_DEF) {
                    asm_key(asm, extends.0)
                } else {
                    assembly
                        .type_token_name(extends)
                        .and_then(|base| {
                            let key = if base.namespace.is_empty() {
                                String::from(base.name)
                            } else {
                                alloc::format!("{}.{}", base.namespace, base.name)
                            };
                            module.type_handle_by_name(&key)
                        })
                        .unwrap_or(0)
                }
            };
            module.bind_reflect_type(
                type_handle,
                ReflectType {
                    namespace: String::from(type_name.namespace),
                    full_name,
                    is_enum,
                    is_value_type: type_def.is_value_type() && !is_special_reference_base(type_def.name()),
                    is_interface: type_def.is_interface(),
                    is_abstract: type_def.is_abstract(),
                    is_public: type_def.is_public(),
                    base_handle,
                },
            );
        }
        #[cfg(feature = "NETMFv4_4")]
        let mut reflect_fields = Vec::new();
        for field in type_def.fields() {
            let handle = asm_key(asm, field.token().0);
            record_target_attributes(assembly, module, asm, handle, field.token());
            #[cfg(feature = "NETMFv4_4")]
            if let Some(name) = field.name() {
                module.bind_type_field_name(type_handle, name, handle);
                module.bind_type_name(asm, field.token(), String::from(name));
                let field_flags = field.flags();
                let is_literal = field_flags & 0x0040 != 0;
                module.bind_field_meta(handle, is_literal, field_flags & 0x0010 != 0);
                if is_literal {
                    if let Some(constant) = field.constant() {
                        module.bind_field_constant(handle, field_constant_of(constant));
                    }
                }
                reflect_fields.push(ReflectField {
                    handle,
                    is_static: field_flags & 0x0010 != 0,
                    is_public: field_flags & 0x0007 == 0x0006,
                });
                if let Some(field_sig) = field.signature() {
                    if let Some(type_handle) =
                        sigtype_to_type_handle(assembly, module, asm, &field_sig, type_index)
                    {
                        module.bind_member_type(handle, type_handle);
                    }
                }
            }
        }
        #[cfg(feature = "NETMFv4_4")]
        module.bind_type_fields(type_handle, reflect_fields);
        #[cfg(feature = "NETMFv4_4")]
        let mut reflect_methods = Vec::new();
        for method in type_def.methods() {
            let token = Token::new(METHOD_DEF, method.rid());
            let handle = asm_key(asm, token.0);
            record_target_attributes(assembly, module, asm, handle, token);
            #[cfg(feature = "NETMFv4_4")]
            for param in method.params() {
                record_target_attributes(
                    assembly,
                    module,
                    asm,
                    param_attr_key(handle, param.sequence() as u16),
                    param.token(),
                );
            }
            #[cfg(feature = "NETMFv4_4")]
            if let Some(name) = method.name() {
                module.bind_type_method_name(type_handle, name, handle);
                module.bind_type_name(asm, token, String::from(name));
                if name == ".ctor" {
                    let param_count = parameters_of(method.signature().as_ref()).len();
                    module.bind_type_ctor_overload(type_handle, handle, param_count);
                    if param_count == 0 {
                        if let Some(ctor) = module.resolve_by_handle(handle) {
                            module.bind_type_ctor(type_handle, ctor);
                        }
                    }
                } else if name != ".cctor" {
                    let method_flags = method.flags();
                    reflect_methods.push(ReflectMethod {
                        handle,
                        is_static: method_flags & 0x0010 != 0,
                        is_public: method_flags & 0x0007 == 0x0006,
                    });
                    module.bind_method_attrs(handle, method_flags);
                    if let Some(method_sig) = method.signature() {
                        if let Some(return_handle) = sigtype_to_type_handle(
                            assembly,
                            module,
                            asm,
                            &method_sig.return_type,
                            type_index,
                        ) {
                            module.bind_member_type(handle, return_handle);
                        }
                        let mut names: Vec<String> = Vec::new();
                        for param in method.params() {
                            let sequence = param.sequence() as usize;
                            if sequence >= 1 {
                                if names.len() < sequence {
                                    names.resize(sequence, String::new());
                                }
                                names[sequence - 1] = String::from(param.name().unwrap_or(""));
                            }
                        }
                        let params: Vec<MethodParam> = method_sig
                            .parameters
                            .iter()
                            .enumerate()
                            .map(|(index, sig_type)| MethodParam {
                                type_handle: sigtype_to_type_handle(
                                    assembly, module, asm, sig_type, type_index,
                                )
                                .unwrap_or(0),
                                name: names.get(index).cloned().unwrap_or_default(),
                            })
                            .collect();
                        module.bind_method_params(handle, params);
                    }
                }
            }
        }
        #[cfg(feature = "NETMFv4_4")]
        module.bind_type_methods(type_handle, reflect_methods);
        for property in type_def.properties() {
            let handle = asm_key(asm, property.token().0);
            record_target_attributes(assembly, module, asm, handle, property.token());
            #[cfg(feature = "NETMFv4_4")]
            if let Some(name) = property.name() {
                module.bind_type_property_name(type_handle, name, handle);
            }
        }
    }
}

/// Decodes and records the custom attributes applied to one target (`target_token`), keyed by
/// its asm-folded `target_handle`. Each attribute's value blob is decoded against its
/// constructor's parameter signature; an attribute whose ctor does not resolve to a same-module
/// [`MethodId`] (a framework attribute) is skipped, as is one whose attribute type has no declared
/// `TypeId` (so it cannot be instantiated).
fn record_target_attributes(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    target_handle: u64,
    target_token: Token,
) {
    for attribute in assembly.custom_attributes(target_token) {
        let ctor_token = attribute.constructor;
        let Some(ctor) = module.resolve(asm, ctor_token) else {
            continue;
        };
        let Some(type_id) = module.method_type(ctor) else {
            continue;
        };
        let ctor_params = assembly
            .resolve_method(ctor_token)
            .and_then(|method| method.signature)
            .map(|signature| signature.parameters)
            .unwrap_or_default();
        let Some(decoded) = decode_custom_attribute(attribute.value, &ctor_params, &|enum_name| {
            enum_underlying_element_type(assembly, enum_name)
        }) else {
            continue;
        };
        let positional = decoded
            .fixed
            .iter()
            .map(|argument| attr_arg_to_value(argument, assembly, asm))
            .collect();
        let mut named_fields = Vec::new();
        let mut named_properties = Vec::new();
        for named in &decoded.named {
            if named.is_field {
                if let Some(slot) = attribute_field_slot(assembly, module, asm, type_id, named.name) {
                    named_fields.push((slot, attr_arg_to_value(&named.value, assembly, asm)));
                }
            } else if let Some(setter) =
                attribute_setter_method(assembly, module, asm, type_id, named.name)
            {
                named_properties.push((setter, attr_arg_to_value(&named.value, assembly, asm)));
            }
        }
        module.add_custom_attribute(
            target_handle,
            LoadedAttribute {
                ctor,
                type_id,
                positional,
                named_fields,
                named_properties,
            },
        );
    }
}

/// The instance-field slot of the field named `name` on the attribute type `type_id` (a
/// same-module type): its position among the type's instance fields, the slot a `stfld` /
/// `set_instance_field` addresses. `None` if the type declares no such instance field.
fn attribute_field_slot(
    assembly: &Assembly,
    module: &Module,
    asm: u8,
    type_id: TypeId,
    name: &str,
) -> Option<u32> {
    for type_def in assembly.type_defs() {
        if module.type_id_of(asm, type_def.token()) != Some(type_id) {
            continue;
        }
        for field in type_def.fields() {
            if field.is_static() {
                continue;
            }
            if field.name() == Some(name) {
                return module.field_slot(asm, field.token());
            }
        }
    }
    None
}

/// The bound [`MethodId`] of the property setter `set_<name>` on the attribute type `type_id` (a
/// same-module type) -- the method a named-PROPERTY custom-attribute argument invokes. Invoking the
/// setter (rather than guessing a backing field) handles auto- AND explicit-property setters alike.
/// `None` if the type declares no such setter.
fn attribute_setter_method(
    assembly: &Assembly,
    module: &Module,
    asm: u8,
    type_id: TypeId,
    name: &str,
) -> Option<MethodId> {
    let setter = alloc::format!("set_{name}");
    for type_def in assembly.type_defs() {
        if module.type_id_of(asm, type_def.token()) != Some(type_id) {
            continue;
        }
        for method in type_def.methods() {
            if method.name() == Some(setter.as_str()) {
                return module.resolve(asm, Token::new(METHOD_DEF, method.rid()));
            }
        }
    }
    None
}

/// The underlying integer element-type byte of the enum named (by its reflection name) in this
/// assembly -- what a custom-attribute blob's enum argument is serialized at. Resolves the enum's
/// `TypeDef` and reads the underlying type of its first constant (II.22.9). Defaults to
/// [`SigType`]-`I4` (the C# default `int`) for an unknown enum or one with no constants.
fn enum_underlying_element_type(assembly: &Assembly, reflection_name: &str) -> u8 {
    const I1: u8 = 0x04;
    const U1: u8 = 0x05;
    const I2: u8 = 0x06;
    const U2: u8 = 0x07;
    const I4: u8 = 0x08;
    const U4: u8 = 0x09;
    const I8: u8 = 0x0A;
    const U8: u8 = 0x0B;
    let (namespace, name) = split_reflection_name(reflection_name);
    let Some(type_def) = assembly.find_type(namespace, name) else {
        return I4;
    };
    for field in type_def.fields() {
        if let Some(constant) = field.constant() {
            return match constant {
                ConstantValue::I1(_) => I1,
                ConstantValue::U1(_) => U1,
                ConstantValue::I2(_) => I2,
                ConstantValue::U2(_) => U2,
                ConstantValue::U4(_) => U4,
                ConstantValue::I8(_) => I8,
                ConstantValue::U8(_) => U8,
                _ => I4,
            };
        }
    }
    I4
}

/// Splits an attribute blob's reflection type name into `(namespace, name)` for resolution: a
/// `typeof(X)` / enum argument serializes the type's reflection name (e.g. `"Color"`,
/// `"Foo.Bar"`). The namespace is everything before the LAST `.`; a name with no `.` is in the
/// global namespace. (Assembly-qualified names and nested-type `+` separators are not
/// recognized.)
fn split_reflection_name(reflection_name: &str) -> (&str, &str) {
    match reflection_name.rfind('.') {
        Some(dot) => (&reflection_name[..dot], &reflection_name[dot + 1..]),
        None => ("", reflection_name),
    }
}

/// Maps a decoded Constant-row value (II.22.9) to the KIND-preserving [`FieldConstant`] the
/// module stores for `FieldInfo.GetRawConstantValue`: bool/char keep their identity (they must
/// box as Boolean/Char, not bare ints), integers ride sign-extended with a wide flag, floats
/// per width (dropped to null on a no-float build -- no corpus does this), strings as UTF-16.
#[cfg(feature = "NETMFv4_4")]
fn field_constant_of(constant: ConstantValue) -> lamella_cil_runtime::module::FieldConstant {
    use lamella_cil_runtime::module::FieldConstant;
    match constant {
        ConstantValue::Bool(value) => FieldConstant::Bool(value),
        ConstantValue::Char(value) => FieldConstant::Char(value),
        ConstantValue::I1(value) => FieldConstant::Int { value: i64::from(value), wide: false },
        ConstantValue::U1(value) => FieldConstant::Int { value: i64::from(value), wide: false },
        ConstantValue::I2(value) => FieldConstant::Int { value: i64::from(value), wide: false },
        ConstantValue::U2(value) => FieldConstant::Int { value: i64::from(value), wide: false },
        ConstantValue::I4(value) => FieldConstant::Int { value: i64::from(value), wide: false },
        ConstantValue::U4(value) => FieldConstant::Int { value: i64::from(value), wide: false },
        ConstantValue::I8(value) => FieldConstant::Int { value, wide: true },
        ConstantValue::U8(value) => FieldConstant::Int { value: value as i64, wide: true },
        #[cfg(feature = "float")]
        ConstantValue::R4(value) => FieldConstant::R4(value),
        #[cfg(feature = "float")]
        ConstantValue::R8(value) => FieldConstant::R8(value),
        #[cfg(not(feature = "float"))]
        ConstantValue::R4(_) | ConstantValue::R8(_) => FieldConstant::Null,
        ConstantValue::String(units) => FieldConstant::Str(units.into_boxed_slice()),
        ConstantValue::Null => FieldConstant::Null,
    }
}

/// Materializes a decoded custom-attribute argument into the load-time [`AttrValue`] the module
/// stores: an integer at its width, a string's UTF-16 units, a resolved `Type` handle for a
/// `typeof(X)` argument (the asm-folded `TypeDef` token of a same-assembly `X`, which is exactly
/// the handle `typeof(X)` pushes at runtime; `0` for a type this assembly does not define -- a
/// cross-assembly `typeof`), or null. The interpreter turns these into runtime
/// values when it instantiates the attribute.
fn attr_arg_to_value(argument: &AttrArg, assembly: &Assembly, asm: u8) -> AttrValue {
    match argument {
        AttrArg::Bool(value) => AttrValue::Int {
            value: i64::from(*value),
            wide: false,
        },
        AttrArg::Char(value) => AttrValue::Int {
            value: i64::from(*value),
            wide: false,
        },
        AttrArg::Int(value) => AttrValue::Int {
            value: *value,
            wide: !(i32::MIN as i64..=i32::MAX as i64).contains(value),
        },
        AttrArg::UInt(value) => AttrValue::Int {
            value: *value as i64,
            wide: *value > u32::MAX as u64,
        },
        AttrArg::R4(value) => AttrValue::R4(*value),
        AttrArg::R8(value) => AttrValue::R8(*value),
        AttrArg::Str(text) => AttrValue::Str(text.encode_utf16().collect()),
        AttrArg::Null => AttrValue::Null,
        AttrArg::Type(name) => {
            let (namespace, simple) = split_reflection_name(name);
            let handle = assembly
                .find_type(namespace, simple)
                .map_or(0, |type_def| asm_key(asm, type_def.token().0));
            AttrValue::Type(handle)
        }
        AttrArg::Array(elements) => AttrValue::Array(
            elements
                .iter()
                .map(|element| attr_arg_to_value(element, assembly, asm))
                .collect(),
        ),
    }
}
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
            module.bind_cast_elem(
                asm,
                *token,
                cast_elem_of_name(asm, name.namespace, name.name, *token),
            );
        } else if let Some(sig) = assembly.type_spec_signature(*token) {
            module.bind_cast_elem(asm, *token, cast_elem_of_sig(asm, &sig));
        }
    }
}

/// The [`CastElem`] shape a NAMED cast-test token denotes: the exact `System` primitive /
/// `String` / `Object` by metadata name, else the named type itself (by asm-folded handle).
fn cast_elem_of_name(asm: u8, namespace: &str, name: &str, token: Token) -> CastElem {
    if namespace == "System" {
        let prim = match name {
            "Boolean" => Some(CastPrim::Bool),
            "Char" => Some(CastPrim::Char),
            "SByte" => Some(CastPrim::I1),
            "Byte" => Some(CastPrim::U1),
            "Int16" => Some(CastPrim::I2),
            "UInt16" => Some(CastPrim::U2),
            "Int32" => Some(CastPrim::I4),
            "UInt32" => Some(CastPrim::U4),
            "Int64" => Some(CastPrim::I8),
            "UInt64" => Some(CastPrim::U8),
            "Single" => Some(CastPrim::F4),
            "Double" => Some(CastPrim::F8),
            "IntPtr" => Some(CastPrim::I),
            "UIntPtr" => Some(CastPrim::U),
            _ => None,
        };
        if let Some(prim) = prim {
            return CastElem::Prim(prim);
        }
        match name {
            "String" => return CastElem::String,
            "Object" => return CastElem::Object,
            _ => {}
        }
    }
    CastElem::Named(asm_key(asm, token.0))
}

/// The [`CastElem`] shape a `TypeSpec` signature denotes. Tokens inside the signature are in
/// the same assembly's token space as the spec row itself. Shapes the cast checks do not
/// model (multi-dimensional arrays, pointers, byrefs) become [`CastElem::Lenient`].
fn cast_elem_of_sig(asm: u8, sig: &SigType) -> CastElem {
    match sig {
        SigType::Boolean => CastElem::Prim(CastPrim::Bool),
        SigType::Char => CastElem::Prim(CastPrim::Char),
        SigType::I1 => CastElem::Prim(CastPrim::I1),
        SigType::U1 => CastElem::Prim(CastPrim::U1),
        SigType::I2 => CastElem::Prim(CastPrim::I2),
        SigType::U2 => CastElem::Prim(CastPrim::U2),
        SigType::I4 => CastElem::Prim(CastPrim::I4),
        SigType::U4 => CastElem::Prim(CastPrim::U4),
        SigType::I8 => CastElem::Prim(CastPrim::I8),
        SigType::U8 => CastElem::Prim(CastPrim::U8),
        SigType::R4 => CastElem::Prim(CastPrim::F4),
        SigType::R8 => CastElem::Prim(CastPrim::F8),
        SigType::IntPtr => CastElem::Prim(CastPrim::I),
        SigType::UIntPtr => CastElem::Prim(CastPrim::U),
        SigType::String => CastElem::String,
        SigType::Object => CastElem::Object,
        SigType::Class(token) | SigType::ValueType(token) => {
            CastElem::Named(asm_key(asm, token.0))
        }
        SigType::SzArray(element) => CastElem::Array(Box::new(cast_elem_of_sig(asm, element))),
        _ => CastElem::Lenient,
    }
}

/// Records the byte size of every type a `sizeof` operand names (III.4.25), and of every
/// value type this assembly declares, so the interpreter's `sizeof` resolves the operand.
///
/// A value type's size is its shared [`lamella_metadata::Assembly::value_type_layout`]
/// (the one computation the AOT stack maps and the GC ref-map also consume) at the
/// 32-bit target ([`TargetLayout::ilp32`] -- these targets use a 4-byte pointer). A `sizeof`
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
fn array_element_default(
    assembly: &Assembly,
    element_type: Token,
    enum_zeros: &BTreeMap<String, Value>,
) -> Value {
    let Some(name) = assembly.type_token_name(element_type) else {
        return Value::Null;
    };
    if let Some(zero) = enum_zeros.get(&type_name_key(name)) {
        return zero.clone();
    }
    if name.namespace != "System" {
        return Value::Null;
    }
    match name.name {
        "Int32" | "UInt32" | "Int16" | "UInt16" | "SByte" | "Byte" | "Boolean" | "Char" => {
            Value::Int32(0)
        }
        "Int64" | "UInt64" => Value::Int64(0),
        #[cfg(feature = "float")]
        "Single" => Value::Single(0.0),
        #[cfg(feature = "float")]
        "Double" => Value::Float(0.0),
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
        "Single" => Value::Single(0.0),
        #[cfg(feature = "float")]
        "Double" => Value::Float(0.0),
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
        .type_token_full_name(extends_token)
        .and_then(|(namespace, name)| type_index.get(&type_key(&namespace, &name)).copied())
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
        Some(defaults) => BaseFields::Extern(defaults),
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
    field_index: &mut FieldNameIndex,
    instance_field_keys: &BTreeMap<u32, String>,
) {
    let bases: Vec<BaseFields> = (0..extends.len())
        .map(|local| resolve_base_fields(module, assembly, type_offset, type_index, extends, local))
        .collect();
    let mut memo: Vec<Option<Vec<Value>>> = alloc::vec![None; extends.len()];
    for local in 0..extends.len() {
        let full = field_layout(local, &bases, own_fields, &mut memo);
        let base_count = full.len() - own_fields[local].len();
        for (index, (token, _)) in own_fields[local].iter().enumerate() {
            let slot = (base_count + index) as u32;
            module.bind_field(asm, *token, slot);
            if let Some(key) = instance_field_keys.get(&token.0) {
                field_index.instances.insert(key.clone(), slot);
            }
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
    /// How many type parameters the method itself declares, or 0. Part of the dispatch key, and
    /// II.9.9 makes it part of the OVERRIDE relation too: "the number of generic parameters shall
    /// match exactly those of the overridden method". Carried here because `sig_encode` is computed
    /// from this struct on one side of a dispatch and from a signature on the other, and the two
    /// have to agree.
    generic_arity: u32,
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
    let base_name = if extends.table() == TYPE_REF {
        assembly.type_ref(extends.row()).and_then(|type_ref| type_ref.name())
    } else if extends.table() == TYPE_DEF {
        assembly.type_def(extends.row()).and_then(|type_def| type_def.name())
    } else {
        return false;
    };
    base_name.is_some_and(|name| matches!(name.name, "MulticastDelegate" | "Delegate"))
}

/// Whether a type extends `System.Enum` -- i.e. is an enum, whose literal constants the
/// loader records (by value) so `Enum.ToString` can name them.
/// Records the `[DllImport]` target of a bodyless PinvokeImpl method, when its signature
/// is within the supported marshaling surface: integer/char/bool parameters (each rides
/// the host call's 64-bit scalar slot), `string` parameters under the ANSI (or default)
/// charset, and a `void`/32-bit-integer return. A method outside that surface records
/// nothing and its call site keeps trapping as unresolved -- the honest partial surface.
fn bind_pinvoke_target(
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    token: Token,
    method: &lamella_metadata::Method,
) {
    let rid = token.row();
    let (Some(entry), Some(dll)) = (assembly.pinvoke_import(rid), assembly.pinvoke_module(rid))
    else {
        return;
    };
    if matches!(
        assembly.pinvoke_charset(rid),
        Some(lamella_metadata::CharSet::Unicode | lamella_metadata::CharSet::Auto)
    ) {
        return;
    }
    let Some(signature) = method.signature() else {
        return;
    };
    let mut param_kinds = Vec::with_capacity(signature.parameters.len());
    for parameter in &signature.parameters {
        param_kinds.push(match parameter {
            SigType::Boolean
            | SigType::Char
            | SigType::I1
            | SigType::U1
            | SigType::I2
            | SigType::U2
            | SigType::I4
            | SigType::U4
            | SigType::I8
            | SigType::U8 => PInvokeParam::Scalar,
            SigType::String => PInvokeParam::AnsiString,
            _ => return,
        });
    }
    let returns = match signature.return_type {
        SigType::Void => PInvokeReturn::Void,
        SigType::Boolean | SigType::I1 | SigType::U1 | SigType::I2 | SigType::U2
        | SigType::I4 | SigType::U4 => PInvokeReturn::Int32,
        _ => return,
    };
    module.bind_pinvoke_target(
        asm,
        token.0,
        PInvokeTarget {
            module: String::from(dll),
            entry: String::from(entry),
            params: param_kinds,
            returns,
        },
    );
}

fn is_enum_type(assembly: &Assembly, extends: Token) -> bool {
    assembly
        .type_token_name(extends)
        .is_some_and(|name| name.namespace == "System" && name.name == "Enum")
}

/// Whether a type is one of the two special CLI base classes that extend a value-type base yet are
/// themselves REFERENCE types: `System.ValueType` and `System.Enum`. The generic `is_value_type()`
/// predicate (extends `ValueType`/`Enum`) flags them, but only a CONCRETE struct/enum deriving from
/// them is a value type -- these two bases are not (III.4.2). Excluding them keeps `Enum`'s
/// inherited instance methods (`HasFlag` / `ToString(format)`) dispatching on a boxed receiver as a
/// reference, rather than being unboxed to a bare managed pointer -- which strips the enum's type
/// identity, so the formatter can no longer name the constant.
fn is_special_reference_base(name: Option<TypeName<'_>>) -> bool {
    name.is_some_and(|name| name.namespace == "System" && matches!(name.name, "ValueType" | "Enum"))
}

/// Whether the type `type_token` (a `TypeDef`) carries `[System.FlagsAttribute]`, by scanning its
/// custom attributes (II.22.10) for one whose constructor's declaring type is named
/// `FlagsAttribute`. The attribute type lives in another assembly (the program references
/// `[ref]System.FlagsAttribute`), so the match is on the resolved declaring-type NAME, exactly as
/// [`Assembly::param_array_params`] matches `ParamArrayAttribute`.
fn has_flags_attribute(assembly: &Assembly, type_token: Token) -> bool {
    assembly.custom_attributes(type_token).any(|attribute| {
        assembly
            .resolve_method(attribute.constructor)
            .and_then(|ctor| ctor.declaring_type)
            .is_some_and(|name| name.namespace == "System" && name.name == "FlagsAttribute")
    })
}

/// Whether the method `method_token` carries `[Lamella.Runtime.RuntimeProvided]` -- the managed
/// corlib's marker for a method whose (empty) body is a placeholder for a native runtime intrinsic
/// (`Console.WriteLine`, `Buffer.BlockCopyInternal`, `Marshal.__ReadByte`, ...). The loader binds
/// such a method to its [`bcl_intrinsic`] instead of running the placeholder body. Mirrors
/// [`has_flags_attribute`]; the attribute type is defined by the corlib itself.
/// What an unbound `[RuntimeProvided]` seam means for a caller that reaches it -- the disposition
/// the AOT tier's census already reports, answered for the INTERPRETER tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeamDisposition {
    /// The seam declares `[Lamella.Runtime.IntendedDefault]`: a build that does not implement it is
    /// still CORRECT, because the default IS the answer (a capability probe that reports absence).
    Intended,
    /// The seam declares nothing: its placeholder body answers a constant -- 0, false, null -- that
    /// the caller cannot tell from a real result. Every row of this kind is a silent wrong answer
    /// waiting for a caller, and the reason this report exists.
    Silent,
}

/// One `[RuntimeProvided]` method this build binds to no intrinsic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnboundSeam {
    /// The declaring type's namespace.
    pub namespace: String,
    /// The declaring type's name.
    pub type_name: String,
    /// The method's name.
    pub method: String,
    /// The parameter types, encoded by [`encode_sig_type`].
    ///
    /// **WITHOUT THIS THE REPORT CANNOT BE ACTED ON, WHICH IS WHAT IT IS FOR.** `Math::Abs` has
    /// four overloads and only the `double` one is a seam; `Math::Log` has two and BOTH are, so the
    /// name alone printed one line twice with nothing to tell them apart. The decision this report
    /// exists to support -- intrinsic, or `[IntendedDefault]` -- is a decision about a SIGNATURE,
    /// and a list of names asks a reader to go find out which overload was meant.
    pub params: Vec<String>,
    /// The return type, same encoding. It is not part of overload identity in C#, but it IS what
    /// says whether a silent seam hands back a zero, a null, or nothing at all -- which is the
    /// difference between a wrong answer and a no-op.
    pub returns: String,
    /// What the absence means -- see [`SeamDisposition`].
    pub disposition: SeamDisposition,
}

impl fmt::Display for UnboundSeam {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}.{}::{}({}) -> {}",
            self.namespace,
            self.type_name,
            self.method,
            self.params.join(", "),
            self.returns
        )
    }
}

/// Every `[RuntimeProvided]` seam in `assembly` that THIS build binds to no intrinsic, in metadata
/// order.
///
/// # Why this exists
///
/// The loader binds a runtime-supplied method to its intrinsic, and where there is none it keeps the
/// managed placeholder body on purpose -- so a seam with no implementation YET is not dropped from
/// the type. The cost of that kindness is silence: the method then answers 0 / false / null forever,
/// and nothing distinguishes it from one that ran. `[IntendedDefault]` is the declaration that the
/// silence is correct; this report is how the ones with no such declaration become visible instead
/// of being counted by hand.
///
/// # Why it cannot drift from the loader
///
/// It asks the same two questions of the same metadata, through the same functions the binding site
/// calls -- `is_runtime_provided` and [`bcl_intrinsic`], with the same arguments. A seam this
/// reports as unbound is a seam the loader leaves unbound, by construction rather than by a
/// parallel rule someone has to keep in step. A separate walk that re-derived either answer would be
/// the exact shape that let the array-descriptor readers disagree for a month.
#[must_use]
pub fn unbound_seams(assembly: &Assembly) -> Vec<UnboundSeam> {
    let mut seams = Vec::new();
    let mut method_row: u32 = 0;
    for type_def in assembly.type_defs() {
        let declaring = type_def.name();
        let is_delegate = is_delegate_type(assembly, type_def.extends());
        for method in type_def.methods() {
            method_row += 1;
            let token = Token::new(METHOD_DEF, method_row);
            if is_delegate {
                continue;
            }
            if !(method.is_runtime_impl() || assembly.is_runtime_provided(token)) {
                continue;
            }
            let Some(declaring) = declaring else { continue };
            let name = method.name().unwrap_or("");
            let signature = method.signature();
            if bcl_intrinsic(
                declaring.namespace,
                declaring.name,
                name,
                signature.as_ref(),
            )
            .is_some()
            {
                continue;
            }
            seams.push(UnboundSeam {
                namespace: declaring.namespace.into(),
                type_name: declaring.name.into(),
                method: name.into(),
                params: signature
                    .as_ref()
                    .map(|sig| {
                        sig.parameters
                            .iter()
                            .map(|param| encode_sig_type(assembly, param))
                            .collect()
                    })
                    .unwrap_or_default(),
                returns: signature
                    .as_ref()
                    .map_or_else(String::new, |sig| encode_sig_type(assembly, &sig.return_type)),
                disposition: if assembly.is_intended_default(token) {
                    SeamDisposition::Intended
                } else {
                    SeamDisposition::Silent
                },
            });
        }
    }
    seams
}

/// Whether every `[RuntimeProvided]` seam `assembly` declares has an implementation in a runtime
/// whose registered intrinsic ids are `available` -- the question "is this corlib legal on THAT
/// board", answered against the board's own listing rather than against this process's features.
///
/// # Why it takes an id set instead of reading the local build
///
/// [`unbound_seams`] answers for the runtime it is COMPILED INTO, which is exactly right for the
/// seam-honesty gate and exactly wrong for a host asking about a device. A board's surface arrives
/// as DATA -- the intrinsic-id listing in its Lamella Link `PROFILE_MANIFEST` -- and no amount of
/// inspecting the local feature set consults it.
///
/// Legality is a SUBSET test, so it is not symmetric: a smaller corlib profile is legal on a richer
/// firmware and uses less of it; the reverse never holds. The failure it prevents is silent -- the
/// loader keeps a `[RuntimeProvided]` placeholder when no intrinsic matches, so a corlib demanding
/// more than the firmware implements does not fail to load, it answers 0 / false / null forever.
///
/// # The honesty this cannot provide by itself
///
/// [`bcl_intrinsic`] is `#[cfg]`-gated, so a build of THIS crate that lacks a capability cannot
/// resolve that capability's seams to an id at all. Those land in [`SeamLegality::indeterminate`]
/// rather than being silently treated as "needs nothing" -- which would UNDER-report the demand and
/// make an illegal pairing look legal, in the same silent direction as the defect above. **A caller
/// that gets a non-empty `indeterminate` has not certified anything**; run the query from a
/// full-surface build, where every seam resolves.
#[must_use]
pub fn seam_legality(assembly: &Assembly, available: &BTreeSet<u32>) -> SeamLegality {
    let mut legality = SeamLegality::default();
    let mut method_row: u32 = 0;
    for type_def in assembly.type_defs() {
        let declaring = type_def.name();
        let is_delegate = is_delegate_type(assembly, type_def.extends());
        for method in type_def.methods() {
            method_row += 1;
            let token = Token::new(METHOD_DEF, method_row);
            if is_delegate {
                continue;
            }
            if !(method.is_runtime_impl() || assembly.is_runtime_provided(token)) {
                continue;
            }
            let Some(declaring) = declaring else { continue };
            let name = method.name().unwrap_or("");
            let signature = method.signature();
            let seam = UnboundSeam {
                namespace: declaring.namespace.into(),
                type_name: declaring.name.into(),
                method: name.into(),
                params: signature
                    .as_ref()
                    .map(|sig| {
                        sig.parameters
                            .iter()
                            .map(|param| encode_sig_type(assembly, param))
                            .collect()
                    })
                    .unwrap_or_default(),
                returns: signature
                    .as_ref()
                    .map_or_else(String::new, |sig| encode_sig_type(assembly, &sig.return_type)),
                disposition: if assembly.is_intended_default(token) {
                    SeamDisposition::Intended
                } else {
                    SeamDisposition::Silent
                },
            };
            match bcl_intrinsic(declaring.namespace, declaring.name, name, signature.as_ref()) {
                Some((_, id)) => {
                    if !available.contains(&id) {
                        legality.unmet.push(seam);
                    }
                }
                None => legality.indeterminate.push(seam),
            }
        }
    }
    legality
}

/// The verdict of [`seam_legality`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SeamLegality {
    /// Seams whose intrinsic the runtime does NOT register: the corlib is illegal on it, and every
    /// one of these would answer a constant rather than fail to load.
    pub unmet: Vec<UnboundSeam>,
    /// Seams this build could not resolve to an intrinsic id, because its own `#[cfg]` gates
    /// compiled the resolver arm out. **Nothing is certified while this is non-empty.**
    pub indeterminate: Vec<UnboundSeam>,
}

impl SeamLegality {
    /// Whether the corlib is legal on that runtime AND this build was able to say so.
    #[must_use]
    pub fn is_certified_legal(&self) -> bool {
        self.unmet.is_empty() && self.indeterminate.is_empty()
    }
}

fn has_runtime_provided_attribute(assembly: &Assembly, method_token: Token) -> bool {
    assembly.is_runtime_provided(method_token)
}

/// The underlying byte width an enum constant's kind implies (`sbyte`/`byte` = 1,
/// `short`/`ushort`/`char` = 2, `int`/`uint` = 4, `long`/`ulong` = 8) -- the enum's underlying
/// type, which every member shares. `Enum.Format`'s "X" zero-pads to `width * 2` hex digits.
fn enum_constant_width(value: &ConstantValue) -> u8 {
    match value {
        ConstantValue::I1(_) | ConstantValue::U1(_) => 1,
        ConstantValue::I2(_) | ConstantValue::U2(_) | ConstantValue::Char(_) => 2,
        ConstantValue::I8(_) | ConstantValue::U8(_) => 8,
        _ => 4,
    }
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
/// so they match. Each parameter is encoded portably via [`encode_sig_type`] (resolving a
/// token-bearing `Class` / `ValueType` -- e.g. an enum parameter like `System.IO.SeekOrigin`
/// -- to its qualified name): a program's `callvirt` MemberRef names that type through a
/// `TypeRef` token while the corlib's abstract MethodDef names it through a `TypeDef` token, so
/// the raw `{:?}` of the two `SigType`s would differ and the dispatch would resolve to no method.
/// Resolving to the name makes both sides encode identically (the same fix `name_key` applies to
/// BCL-call resolution). A token-free parameter (a primitive, `string`, `object`, or an
/// array/pointer/byref thereof) keeps its stable short form, so primitive-only signatures encode
/// exactly as before.
fn sig_encode(
    assembly: &Assembly,
    name: &str,
    params: &[SigType],
    generic_arity: u32,
    arguments: &[SigType],
) -> String {
    let mut key = match (generic_arity, arguments.is_empty()) {
        (0, _) => alloc::format!("{name}|"),
        (arity, true) => alloc::format!("{name}`{arity}|"),
        (arity, false) => {
            let mut spelled = alloc::format!("{name}`{arity}<");
            for (index, argument) in arguments.iter().enumerate() {
                if index > 0 {
                    spelled.push(',');
                }
                spelled.push_str(&encode_sig_type(assembly, argument));
            }
            spelled.push_str(">|");
            spelled
        }
    };
    for param in params {
        key.push_str(&encode_sig_type(assembly, param));
        key.push(',');
    }
    key
}

/// Builds each type's signature-keyed method map (its virtual / interface-implementing
/// methods, including inherited, keyed by [`sig_encode`]), for dispatching `callvirt`
/// to an interface or abstract method on a value of that runtime type.
fn build_sig_methods(
    module: &mut Module,
    assembly: &Assembly,
    _asm: u8,
    type_offset: usize,
    extends: &[Token],
    virtuals: &[Vec<VirtualMethod>],
    nonvirtuals: &[Vec<VirtualMethod>],
) {
    let mut virtual_memo: Vec<Option<BTreeMap<String, MethodId>>> =
        alloc::vec![None; extends.len()];
    let mut nonvirtual_memo: Vec<Option<BTreeMap<String, MethodId>>> =
        alloc::vec![None; extends.len()];
    for local in 0..extends.len() {
        let methods = compute_sig_methods(assembly, local, extends, virtuals, &mut virtual_memo);
        if !methods.is_empty() {
            module.set_sig_methods((type_offset + local) as u32, methods);
        }
        let nonvirtual =
            compute_sig_methods(assembly, local, extends, nonvirtuals, &mut nonvirtual_memo);
        if !nonvirtual.is_empty() {
            module.set_sig_methods_nonvirtual((type_offset + local) as u32, nonvirtual);
        }
    }
}

/// The memoized signature-keyed method map of `type_id`: its base's map plus its own
/// virtual methods (a derived method's key replaces the inherited one).
fn compute_sig_methods(
    assembly: &Assembly,
    type_id: usize,
    extends: &[Token],
    virtuals: &[Vec<VirtualMethod>],
    memo: &mut [Option<BTreeMap<String, MethodId>>],
) -> BTreeMap<String, MethodId> {
    if let Some(methods) = &memo[type_id] {
        return methods.clone();
    }
    let mut methods = match base_type_id(extends[type_id], extends.len()) {
        Some(base) => compute_sig_methods(assembly, base, extends, virtuals, memo),
        None => BTreeMap::new(),
    };
    for method in &virtuals[type_id] {
        methods.insert(
            sig_encode(assembly, &method.name, &method.params, method.generic_arity, &[]),
            method.id,
        );
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
    methoddef_sigs: &BTreeMap<u32, (String, Vec<SigType>, u32)>,
) {
    for token in tokens {
        let (key, param_count) = match token.table() {
            METHOD_DEF => match methoddef_sigs.get(&token.row()) {
                Some((name, params, arity)) => (
                    sig_encode(assembly, name, params, *arity, &[]),
                    params.len(),
                ),
                None => continue,
            },
            MEMBER_REF => {
                let Some(member) = assembly.member_ref(token.row()) else {
                    continue;
                };
                let name = member.name().unwrap_or("");
                let signature = member.method_signature();
                let arity = signature.as_ref().map_or(0, |sig| sig.generic_param_count);
                let params = signature.map(|sig| sig.parameters).unwrap_or_default();
                (sig_encode(assembly, name, &params, arity, &[]), params.len())
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
        let table = compute_vtable(assembly, local, &bases, virtuals, &mut memo, &mut visiting, &mut method_slots);
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
    assembly: &Assembly,
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
            compute_vtable(assembly, *base, bases, virtuals, memo, visiting, method_slots)
        }
        BaseVtable::Extern(slots) => slots.clone(),
        BaseVtable::None => Vec::new(),
    };
    for method in &virtuals[type_id] {
        let key = sig_encode(assembly, &method.name, &method.params, method.generic_arity, &[]);
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
    assembly: &Assembly,
    module: &mut Module,
    asm: u8,
    type_offset: usize,
    extends: &[Token],
    is_value_type: &[bool],
    type_index: &TypeNameIndex,
) {
    for local in 0..extends.len() {
        let token = Token::new(TYPE_DEF, (local + 1) as u32);
        let type_id = (type_offset + local) as u32;
        module.bind_type_token(asm, token, type_id);
        let base = base_type_id(extends[local], extends.len())
            .map(|base| (type_offset + base) as u32)
            .or_else(|| external_base_type_id(assembly, extends[local], type_index));
        module.set_type_base(type_id, base);
        module.set_type_is_value_type(type_id, is_value_type[local]);
    }
}

/// The id of a base type declared in ANOTHER assembly, resolved by name through the shared
/// type index.
///
/// [`base_type_id`] answers only for a `TypeDef` -- a base in the same assembly -- and returned
/// `None` for everything else, so a type extending one from another assembly was recorded with NO
/// BASE AT ALL. The base chain is what `castclass` / `isinst` and the `stelem.ref` covariance check
/// walk, so the subtype relation simply stopped at every assembly boundary: a covariant store of a
/// program's driver into a library-typed array was refused, and a library delegate could not be
/// recognized as one because its base (`System.MulticastDelegate`) is a `TypeRef` into corlib.
/// Virtual dispatch never noticed, because it goes through the vtable and never walks the chain --
/// which is why overrides across the boundary always worked and hid this.
///
/// Resolution is by NAME, the same lookup the delegate `Invoke` arm uses. Libraries are loaded
/// before the assemblies that reference them, so the base is already in the index by the time
/// anything refers to it.
fn external_base_type_id(
    assembly: &Assembly,
    extends: Token,
    type_index: &TypeNameIndex,
) -> Option<TypeId> {
    if extends.table() != TYPE_REF {
        return None;
    }
    let name = assembly.type_ref(extends.row())?.name()?;
    type_index.get(&type_name_key(name)).copied()
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
fn console_write_line_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
    let intrinsic: (IntrinsicFn, u32) = match parameters_of(signature) {
        [] => intrinsic!(console_write_line_empty),
        [SigType::String] => intrinsic!(console_write_line),
        [SigType::I4] => intrinsic!(console_write_line_int32),
        [SigType::I8] => intrinsic!(console_write_line_int64),
        [SigType::U4] => intrinsic!(console_write_line_uint32),
        [SigType::U8] => intrinsic!(console_write_line_uint64),
        [SigType::Boolean] => intrinsic!(console_write_line_bool),
        [SigType::Char] => intrinsic!(console_write_line_char),
        #[cfg(feature = "float")]
        [SigType::R8] => intrinsic!(console_write_line_double),
        #[cfg(feature = "float")]
        [SigType::R4] => intrinsic!(console_write_line_single),
        [SigType::Object] => intrinsic!(console_write_line_object),
        _ => return None,
    };
    Some(intrinsic)
}

/// A two-`Decimal` operator/comparison: both parameters are the `Decimal` value type (each
/// arriving as the inline value-type struct). Binds only that two-operand form.
fn decimal_binary_op(
    intrinsic: (IntrinsicFn, u32),
    signature: Option<&MethodSig>,
) -> Option<(IntrinsicFn, u32)> {
    match parameters_of(signature) {
        [SigType::ValueType(_), SigType::ValueType(_)] => Some(intrinsic),
        _ => None,
    }
}

/// The parameterless `ToString()` overload binds to `intrinsic`; the formatting
/// overloads (`ToString(string)` / `ToString(IFormatProvider)`) are not modeled.
fn to_string_overload(
    intrinsic: (IntrinsicFn, u32),
    signature: Option<&MethodSig>,
) -> Option<(IntrinsicFn, u32)> {
    match parameters_of(signature) {
        [] => Some(intrinsic),
        _ => None,
    }
}

/// Picks the `Console.Write` overload (no line terminator) by its parameter type.
fn console_write_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
    let intrinsic: (IntrinsicFn, u32) = match parameters_of(signature) {
        [SigType::String] => intrinsic!(console_write),
        [SigType::I4] => intrinsic!(console_write_int32),
        [SigType::I8] => intrinsic!(console_write_int64),
        [SigType::U4] => intrinsic!(console_write_uint32),
        [SigType::U8] => intrinsic!(console_write_uint64),
        [SigType::Boolean] => intrinsic!(console_write_bool),
        [SigType::Char] => intrinsic!(console_write_char),
        #[cfg(feature = "float")]
        [SigType::R8] => intrinsic!(console_write_double),
        #[cfg(feature = "float")]
        [SigType::R4] => intrinsic!(console_write_single),
        _ => return None,
    };
    Some(intrinsic)
}

/// Picks the `String.Concat` overload by its parameter types (the two-string form
/// for now -- what `a + b` on strings emits).
fn string_concat_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
    match parameters_of(signature) {
        [SigType::String, SigType::String] => Some(intrinsic!(string_concat)),
        [SigType::String, SigType::String, SigType::String] => Some(intrinsic!(string_concat3)),
        [SigType::Object, SigType::Object] => Some(intrinsic!(string_concat_object2)),
        [SigType::Object, SigType::Object, SigType::Object] => Some(intrinsic!(string_concat_object3)),
        _ => None,
    }
}

/// The `String.Length` getter -- an instance method with no explicit parameters.
fn string_get_length_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
    match parameters_of(signature) {
        [] => Some(intrinsic!(string_get_length)),
        _ => None,
    }
}

/// The `String.op_Equality(string, string)` operator (what `==` on strings emits).
fn string_equals_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
    match parameters_of(signature) {
        [SigType::String, SigType::String] => Some(intrinsic!(string_equals)),
        _ => None,
    }
}

/// The `String.op_Inequality(string, string)` operator (`!=`).
fn string_not_equals_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
    match parameters_of(signature) {
        [SigType::String, SigType::String] => Some(intrinsic!(string_not_equals)),
        _ => None,
    }
}

/// `String.IsNullOrEmpty(string)` -- a static one-string predicate.
fn string_is_null_or_empty_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
    match parameters_of(signature) {
        [SigType::String] => Some(intrinsic!(string_is_null_or_empty)),
        _ => None,
    }
}

/// `String.Substring(int)` / `Substring(int, int)` -- instance methods.
fn string_substring_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
    match parameters_of(signature) {
        [SigType::I4] => Some(intrinsic!(string_substring)),
        [SigType::I4, SigType::I4] => Some(intrinsic!(string_substring_len)),
        _ => None,
    }
}

/// The `String.get_Chars(int)` indexer (`s[i]`) -- an instance method taking an
/// `int` index.
fn string_get_chars_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
    match parameters_of(signature) {
        [SigType::I4] => Some(intrinsic!(string_get_chars)),
        _ => None,
    }
}

/// The `String(char*)` / `String(char*, int, int)` constructors. The bound intrinsics are
/// identity anchors: the interpreter's `newobj` routes both through its frames-aware
/// string materialization (the char* may point into a caller frame's stackalloc buffer),
/// so binding here is what makes the constructor token resolve.
fn string_ctor_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
    match parameters_of(signature) {
        [SigType::Pointer(value)] if **value == SigType::Char => {
            Some(intrinsic!(string_ctor_char_ptr))
        }
        [SigType::Pointer(value), SigType::I4, SigType::I4] if **value == SigType::Char => {
            Some(intrinsic!(string_ctor_char_ptr_range))
        }
        [SigType::SzArray(element)] if **element == SigType::Char => {
            Some(intrinsic!(string_ctor_char_array))
        }
        [SigType::SzArray(element), SigType::I4, SigType::I4] if **element == SigType::Char => {
            Some(intrinsic!(string_ctor_char_array_range))
        }
        [SigType::Char, SigType::I4] => Some(intrinsic!(string_ctor_char_repeat)),
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
    fn string_index_of_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::Char] => Some(intrinsic!(string_index_of_char)),
            [SigType::String] => Some(intrinsic!(string_index_of_string)),
            _ => None,
        }
    }

    /// `String.LastIndexOf(char)`.
    fn string_last_index_of_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::Char] => Some(intrinsic!(string_last_index_of_char)),
            _ => None,
        }
    }

    /// A one-string-argument predicate (`StartsWith` / `EndsWith` / `Contains`), ordinal.
    fn string_one_string_predicate(
        intrinsic: (IntrinsicFn, u32),
        signature: Option<&MethodSig>,
    ) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::String] => Some(intrinsic),
            _ => None,
        }
    }

    /// A parameterless string-returning transform (`ToUpper` / `ToLower` / `Trim`); the
    /// culture/char-set overloads are not modeled.
    fn string_no_arg_transform(
        intrinsic: (IntrinsicFn, u32),
        signature: Option<&MethodSig>,
    ) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [] => Some(intrinsic),
            _ => None,
        }
    }

    /// `String.Replace(char, char)` / `Replace(string, string)`.
    fn string_replace_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::Char, SigType::Char] => Some(intrinsic!(string_replace_char)),
            [SigType::String, SigType::String] => Some(intrinsic!(string_replace_string)),
            _ => None,
        }
    }

    /// `Math.Abs(int)` / `Abs(long)` -- the integer overloads (float/double need libm).
    fn math_abs_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::I4] => Some(intrinsic!(math_abs_int32)),
            [SigType::I8] => Some(intrinsic!(math_abs_int64)),
            #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
            [SigType::R8] => Some(intrinsic!(math_abs_f64)),
            _ => None,
        }
    }

    /// A unary `double -> double` `Math` overload (`Floor` / `Ceiling` / `Truncate` / `Round`).
    #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
    fn math_unary_f64_overload(
        intrinsic: (IntrinsicFn, u32),
        signature: Option<&MethodSig>,
    ) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::R8] => Some(intrinsic),
            _ => None,
        }
    }

    /// A binary `Math` overload (`Max` / `Min`) over two ints or two longs.
    fn math_binary_overload(
        int32: (IntrinsicFn, u32),
        int64: (IntrinsicFn, u32),
        float: Option<(IntrinsicFn, u32)>,
        signature: Option<&MethodSig>,
    ) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::I4, SigType::I4] => Some(int32),
            [SigType::I8, SigType::I8] => Some(int64),
            [SigType::R8, SigType::R8] => float,
            _ => None,
        }
    }

    /// The double `Math.Max` / `Math.Min` intrinsics, present only with `float` (and, since they live
    /// in the NETMFv4_4 `extended` module, only with reflection on).
    #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
    const MATH_MAX_F64: Option<(IntrinsicFn, u32)> = Some(intrinsic!(math_max_f64));
    #[cfg(not(all(feature = "NETMFv4_4", feature = "float")))]
    const MATH_MAX_F64: Option<(IntrinsicFn, u32)> = None;
    #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
    const MATH_MIN_F64: Option<(IntrinsicFn, u32)> = Some(intrinsic!(math_min_f64));
    #[cfg(not(all(feature = "NETMFv4_4", feature = "float")))]
    const MATH_MIN_F64: Option<(IntrinsicFn, u32)> = None;

    /// `Math.Sign(int)` / `Sign(long)` -- both return an `int`.
    fn math_sign_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::I4] => Some(intrinsic!(math_sign_int32)),
            [SigType::I8] => Some(intrinsic!(math_sign_int64)),
            #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
            [SigType::R8] => Some(intrinsic!(math_sign_f64)),
            _ => None,
        }
    }

    /// A one-`char` `System.Char` method (classification or ASCII casing).
    fn char_one_arg_overload(
        intrinsic: (IntrinsicFn, u32),
        signature: Option<&MethodSig>,
    ) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::Char] => Some(intrinsic),
            _ => None,
        }
    }

    /// A single-`string`-argument static method (`Int32.Parse`, `Boolean.Parse`, ...). The
    /// format-provider / number-styles overloads are not modeled.
    fn one_string_overload(
        intrinsic: (IntrinsicFn, u32),
        signature: Option<&MethodSig>,
    ) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::String] => Some(intrinsic),
            _ => None,
        }
    }

    /// `System.Convert.ToString(value)`: dispatch to the primitive's `ToString` rendering by
    /// the argument type (each is a Kernel/base intrinsic reused for the static conversion).
    fn convert_to_string_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::I4] => Some(intrinsic!(int32_to_string)),
            [SigType::I8] => Some(intrinsic!(int64_to_string)),
            [SigType::Boolean] => Some(intrinsic!(boolean_to_string)),
            #[cfg(feature = "float")]
            [SigType::R8] => Some(intrinsic!(double_to_string)),
            #[cfg(feature = "float")]
            [SigType::R4] => Some(intrinsic!(single_to_string)),
            [SigType::Char] => Some(intrinsic!(char_to_string)),
            _ => None,
        }
    }

    /// `String.PadLeft(int)` / `PadLeft(int, char)` (and the `PadRight` pair).
    fn string_pad_overload(
        intrinsic: (IntrinsicFn, u32),
        signature: Option<&MethodSig>,
    ) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::I4] | [SigType::I4, SigType::Char] => Some(intrinsic),
            _ => None,
        }
    }

    /// `String.Insert(int, string)`.
    fn string_insert_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::I4, SigType::String] => Some(intrinsic!(string_insert)),
            _ => None,
        }
    }

    /// `String.Remove(int)` / `Remove(int, int)`.
    fn string_remove_overload(signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
        match parameters_of(signature) {
            [SigType::I4] | [SigType::I4, SigType::I4] => Some(intrinsic!(string_remove)),
            _ => None,
        }
    }

    /// `System.Text` intrinsic hook. `System.Text.StringBuilder` is now ordinary managed C# over a
    /// `char[] _chars` buffer -- materialized through the one `String.CreateFromChars` seam -- so no
    /// StringBuilder instance method binds to an intrinsic here (the `Encoding` family is managed
    /// too). Kept as the `System.Text` dispatch hook (`bcl_intrinsic` routes the namespace here).
    pub(super) fn text_intrinsic(
        _type_name: &str,
        _method: &str,
        _signature: Option<&MethodSig>,
    ) -> Option<(IntrinsicFn, u32)> {
        None
    }

    /// The `get_Count` / `Contains` / `Clear` methods shared by `Stack` and `Queue` (both
    /// are array-backed, so they reuse the list intrinsics).
    fn collection_shared(method: &str, signature: Option<&MethodSig>) -> Option<(IntrinsicFn, u32)> {
        match (method, parameters_of(signature)) {
            ("get_Count", []) => Some(intrinsic!(list_get_count)),
            ("Contains", [SigType::Object]) => Some(intrinsic!(collection_contains)),
            ("Clear", []) => Some(intrinsic!(list_clear)),
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
    ) -> Option<(IntrinsicFn, u32)> {
        match (type_name, method) {
            ("ArrayList", "Add") => match parameters_of(signature) {
                [SigType::Object] => Some(intrinsic!(list_add)),
                _ => None,
            },
            ("ArrayList", "get_Item") => match parameters_of(signature) {
                [SigType::I4] => Some(intrinsic!(list_get_item)),
                _ => None,
            },
            ("ArrayList", "set_Item") => match parameters_of(signature) {
                [SigType::I4, SigType::Object] => Some(intrinsic!(list_set_item)),
                _ => None,
            },
            ("ArrayList", "get_Count") => match parameters_of(signature) {
                [] => Some(intrinsic!(list_get_count)),
                _ => None,
            },
            ("ArrayList", "Clear") => match parameters_of(signature) {
                [] => Some(intrinsic!(list_clear)),
                _ => None,
            },
            ("ArrayList", "RemoveAt") => match parameters_of(signature) {
                [SigType::I4] => Some(intrinsic!(list_remove_at)),
                _ => None,
            },
            ("ArrayList", "Insert") => match parameters_of(signature) {
                [SigType::I4, SigType::Object] => Some(intrinsic!(list_insert)),
                _ => None,
            },
            ("Hashtable", "Add") => match parameters_of(signature) {
                [SigType::Object, SigType::Object] => Some(intrinsic!(map_add)),
                _ => None,
            },
            ("Hashtable", "get_Item") => match parameters_of(signature) {
                [SigType::Object] => Some(intrinsic!(map_get_item)),
                _ => None,
            },
            ("Hashtable", "set_Item") => match parameters_of(signature) {
                [SigType::Object, SigType::Object] => Some(intrinsic!(map_set_item)),
                _ => None,
            },
            ("Hashtable", "get_Count") => match parameters_of(signature) {
                [] => Some(intrinsic!(map_get_count)),
                _ => None,
            },
            ("Hashtable", "Contains" | "ContainsKey") => match parameters_of(signature) {
                [SigType::Object] => Some(intrinsic!(map_contains)),
                _ => None,
            },
            ("Hashtable", "Remove") => match parameters_of(signature) {
                [SigType::Object] => Some(intrinsic!(map_remove)),
                _ => None,
            },
            ("Hashtable", "Clear") => match parameters_of(signature) {
                [] => Some(intrinsic!(list_clear)),
                _ => None,
            },
            ("Stack", "Push") => match parameters_of(signature) {
                [SigType::Object] => Some(intrinsic!(collection_push)),
                _ => None,
            },
            ("Stack", "Pop") => match parameters_of(signature) {
                [] => Some(intrinsic!(stack_pop)),
                _ => None,
            },
            ("Stack", "Peek") => match parameters_of(signature) {
                [] => Some(intrinsic!(stack_peek)),
                _ => None,
            },
            ("Queue", "Enqueue") => match parameters_of(signature) {
                [SigType::Object] => Some(intrinsic!(collection_push)),
                _ => None,
            },
            ("Queue", "Dequeue") => match parameters_of(signature) {
                [] => Some(intrinsic!(queue_dequeue)),
                _ => None,
            },
            ("Queue", "Peek") => match parameters_of(signature) {
                [] => Some(intrinsic!(queue_peek)),
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
    ) -> Option<(IntrinsicFn, u32)> {
        match (type_name, method) {
            ("String", "IndexOf") => string_index_of_overload(signature),
            ("String", "LastIndexOf") => string_last_index_of_overload(signature),
            ("String", "StartsWith") => string_one_string_predicate(intrinsic!(string_starts_with), signature),
            ("String", "EndsWith") => string_one_string_predicate(intrinsic!(string_ends_with), signature),
            ("String", "Contains") => string_one_string_predicate(intrinsic!(string_contains), signature),
            ("String", "ToUpper") => string_no_arg_transform(intrinsic!(string_to_upper), signature),
            ("String", "ToLower") => string_no_arg_transform(intrinsic!(string_to_lower), signature),
            ("String", "Trim") => string_no_arg_transform(intrinsic!(string_trim), signature),
            ("String", "Replace") => string_replace_overload(signature),
            ("String", "PadLeft") => string_pad_overload(intrinsic!(string_pad_left), signature),
            ("String", "PadRight") => string_pad_overload(intrinsic!(string_pad_right), signature),
            ("String", "Insert") => string_insert_overload(signature),
            ("String", "Remove") => string_remove_overload(signature),
            ("String", "ToCharArray") => string_no_arg_transform(intrinsic!(string_to_char_array), signature),
            ("String", "Equals") => string_one_string_predicate(intrinsic!(string_equals), signature),
            ("String", "Split") => match parameters_of(signature) {
                [SigType::Char, SigType::ValueType(_)] => Some(intrinsic!(string_split_char)),
                _ => None,
            },
            ("String", "Join") => match parameters_of(signature) {
                [SigType::String, SigType::SzArray(element)]
                    if matches!(element.as_ref(), SigType::String) =>
                {
                    Some(intrinsic!(string_join))
                }
                _ => None,
            },
            ("Math", "Abs") => math_abs_overload(signature),
            ("Math", "Max") => {
                math_binary_overload(intrinsic!(math_max_int32), intrinsic!(math_max_int64), MATH_MAX_F64, signature)
            }
            ("Math", "Min") => {
                math_binary_overload(intrinsic!(math_min_int32), intrinsic!(math_min_int64), MATH_MIN_F64, signature)
            }
            ("Math", "Sign") => math_sign_overload(signature),
            #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
            ("Math", "Floor") => math_unary_f64_overload(intrinsic!(math_floor_f64), signature),
            #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
            ("Math", "Ceiling") => math_unary_f64_overload(intrinsic!(math_ceiling_f64), signature),
            #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
            ("Math", "Truncate") => math_unary_f64_overload(intrinsic!(math_truncate_f64), signature),
            #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
            ("Math", "Round") => math_unary_f64_overload(intrinsic!(math_round_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Sqrt") => math_unary_f64_overload(intrinsic!(math_sqrt_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Sin") => math_unary_f64_overload(intrinsic!(math_sin_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Cos") => math_unary_f64_overload(intrinsic!(math_cos_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Tan") => math_unary_f64_overload(intrinsic!(math_tan_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Log") => match parameters_of(signature) {
                [SigType::R8] => Some(intrinsic!(math_log_f64)),
                [SigType::R8, SigType::R8] => Some(intrinsic!(math_log_base_f64)),
                _ => None,
            },
            #[cfg(feature = "math-transcendental")]
            ("Math", "Log10") => math_unary_f64_overload(intrinsic!(math_log10_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Exp") => math_unary_f64_overload(intrinsic!(math_exp_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Pow") => match parameters_of(signature) {
                [SigType::R8, SigType::R8] => Some(intrinsic!(math_pow_f64)),
                _ => None,
            },
            #[cfg(feature = "math-transcendental")]
            ("Math", "Asin") => math_unary_f64_overload(intrinsic!(math_asin_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Acos") => math_unary_f64_overload(intrinsic!(math_acos_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Atan") => math_unary_f64_overload(intrinsic!(math_atan_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Atan2") => match parameters_of(signature) {
                [SigType::R8, SigType::R8] => Some(intrinsic!(math_atan2_f64)),
                _ => None,
            },
            #[cfg(feature = "math-transcendental")]
            ("Math", "Sinh") => math_unary_f64_overload(intrinsic!(math_sinh_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Cosh") => math_unary_f64_overload(intrinsic!(math_cosh_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "Tanh") => math_unary_f64_overload(intrinsic!(math_tanh_f64), signature),
            #[cfg(feature = "math-transcendental")]
            ("Math", "IEEERemainder") => match parameters_of(signature) {
                [SigType::R8, SigType::R8] => Some(intrinsic!(math_ieee_remainder_f64)),
                _ => None,
            },
            ("Char", "IsDigit") => char_one_arg_overload(intrinsic!(char_is_digit), signature),
            ("Char", "IsLetter") => char_one_arg_overload(intrinsic!(char_is_letter), signature),
            ("Char", "IsLetterOrDigit") => {
                char_one_arg_overload(intrinsic!(char_is_letter_or_digit), signature)
            }
            ("Char", "IsWhiteSpace") => char_one_arg_overload(intrinsic!(char_is_white_space), signature),
            ("Char", "IsUpper") => char_one_arg_overload(intrinsic!(char_is_upper), signature),
            ("Char", "IsLower") => char_one_arg_overload(intrinsic!(char_is_lower), signature),
            ("Char", "ToUpper") => char_one_arg_overload(intrinsic!(char_to_upper), signature),
            ("Char", "ToLower") => char_one_arg_overload(intrinsic!(char_to_lower), signature),
            ("Int32", "Parse") => one_string_overload(intrinsic!(int32_parse), signature),
            ("Int64", "Parse") => one_string_overload(intrinsic!(int64_parse), signature),
            ("Boolean", "Parse") => one_string_overload(intrinsic!(boolean_parse), signature),
            ("Convert", "ToInt32") => match parameters_of(signature) {
                [SigType::String] => Some(intrinsic!(int32_parse)),
                #[cfg(all(feature = "NETMFv4_4", feature = "float"))]
                [SigType::R8] => Some(intrinsic!(convert_to_int32_double)),
                _ => None,
            },
            ("Convert", "ToInt64") => one_string_overload(intrinsic!(int64_parse), signature),
            ("Convert", "ToBoolean") => match parameters_of(signature) {
                [SigType::String] => Some(intrinsic!(boolean_parse)),
                [SigType::I4] => Some(intrinsic!(convert_to_boolean_int)),
                _ => None,
            },
            ("Convert", "ToChar") => match parameters_of(signature) {
                [SigType::I4] => Some(intrinsic!(convert_to_char_int)),
                _ => None,
            },
            ("Convert", "ToByte") => match parameters_of(signature) {
                [SigType::I4] => Some(intrinsic!(convert_to_byte_int)),
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
