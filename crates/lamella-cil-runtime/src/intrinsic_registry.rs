//! The intrinsic registry: every intrinsic under a BUILD-STABLE id (FNV-1a of the
//! function's own name), so a baked image records ids and a booting module rebinds
//! them to this build's function pointers. Ids derive from names, so the registry is
//! append-only by construction; a feature-gated intrinsic simply has no entry in a
//! build without the feature, and an image needing it fails the rebind loudly.

use crate::intrinsics::*;
use crate::module::IntrinsicFn;

/// One registry row: the stable id is DERIVED from the intrinsic's name right here
/// (the same fold the bind-time stamp uses), so an entry can never carry a
/// mistranscribed literal -- the id and the function cannot disagree by construction.
macro_rules! entry {
    ($name:ident) => {
        (intrinsic_id(stringify!($name)), $name)
    };
}

/// Every intrinsic this build compiles, as `(stable id, function)`.
static REGISTRY: &[(u32, IntrinsicFn)] = &[
    #[cfg(feature = "reflection")]
    entry!(activator_create_instance),
    #[cfg(feature = "varargs")]
    entry!(arg_iterator_cookie),
    #[cfg(feature = "varargs")]
    entry!(arg_iterator_get),
    #[cfg(feature = "varargs")]
    entry!(arg_iterator_remaining),
    entry!(array_clear_range),
    entry!(array_copy_range),
    entry!(array_clone),
    entry!(array_create_instance),
    entry!(array_empty),
    entry!(array_get_value),
    entry!(array_rank),
    entry!(array_set_value),
    #[cfg(feature = "reflection")]
    entry!(assembly_full_name),
    #[cfg(feature = "reflection")]
    entry!(assembly_get_type),
    #[cfg(feature = "reflection")]
    entry!(assembly_get_types),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(bitconverter_double_to_int64_bits),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(bitconverter_int32_bits_to_single),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(bitconverter_int64_bits_to_double),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(bitconverter_single_to_int32_bits),
    #[cfg(feature = "NETMFv4_4")]
    entry!(boolean_parse),
    entry!(boolean_to_string),
    entry!(buffer_block_copy),
    entry!(buffer_byte_length),
    #[cfg(feature = "text")]
    entry!(char_is_digit),
    #[cfg(feature = "text")]
    entry!(char_is_letter),
    #[cfg(feature = "text")]
    entry!(char_is_letter_or_digit),
    #[cfg(feature = "text")]
    entry!(char_is_lower),
    #[cfg(feature = "text")]
    entry!(char_is_upper),
    #[cfg(feature = "text")]
    entry!(char_is_white_space),
    #[cfg(feature = "text")]
    entry!(char_to_lower),
    entry!(char_to_string),
    #[cfg(feature = "text")]
    entry!(char_to_upper),
    entry!(clock_is_set),
    entry!(clock_set_ticks),
    #[cfg(feature = "collections")]
    entry!(collection_contains),
    #[cfg(feature = "collections")]
    entry!(collection_push),
    entry!(console_write),
    entry!(console_write_bool),
    entry!(console_write_char),
    #[cfg(feature = "float")]
    entry!(console_write_double),
    entry!(console_write_int32),
    entry!(console_write_int64),
    entry!(console_write_uint32),
    entry!(console_write_uint64),
    entry!(console_write_line),
    entry!(console_write_line_bool),
    entry!(console_write_line_char),
    #[cfg(feature = "float")]
    entry!(console_write_line_double),
    entry!(console_write_line_empty),
    entry!(console_write_line_int32),
    entry!(console_write_line_int64),
    entry!(console_write_line_object),
    #[cfg(feature = "float")]
    entry!(console_write_line_single),
    entry!(console_write_line_uint32),
    entry!(console_write_line_uint64),
    #[cfg(feature = "float")]
    entry!(console_write_single),
    #[cfg(feature = "reflection")]
    entry!(constructor_invoke),
    #[cfg(feature = "NETMFv4_4")]
    entry!(convert_to_boolean_int),
    #[cfg(feature = "NETMFv4_4")]
    entry!(convert_to_byte_int),
    #[cfg(feature = "NETMFv4_4")]
    entry!(convert_to_char_int),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(convert_to_int32_double),
    entry!(datetime_now_ticks),
    entry!(debug_write),
    entry!(decimal_add),
    entry!(decimal_compare),
    entry!(decimal_divide),
    #[cfg(feature = "float")]
    entry!(decimal_from_double),
    entry!(decimal_multiply),
    entry!(decimal_remainder),
    entry!(decimal_subtract),
    #[cfg(feature = "float")]
    entry!(decimal_to_double),
    entry!(delegate_combine),
    entry!(delegate_equals),
    entry!(delegate_not_equals),
    entry!(delegate_remove),
    entry!(dns_resolve_host),
    #[cfg(feature = "float")]
    entry!(double_parse),
    #[cfg(feature = "float")]
    entry!(double_to_exponential),
    #[cfg(feature = "float")]
    entry!(double_to_fixed),
    #[cfg(feature = "float")]
    entry!(double_to_string),
    entry!(drive_filesystems),
    entry!(drive_format),
    entry!(drive_kind),
    entry!(drive_mount_removable),
    entry!(drive_names),
    entry!(drive_total_size),
    entry!(enum_format),
    entry!(enum_get_name),
    entry!(enum_get_names),
    entry!(enum_get_values),
    entry!(enum_has_flag),
    entry!(enum_is_defined),
    entry!(enum_parse),
    entry!(enum_to_string_format),
    entry!(environment_get_variable),
    entry!(environment_processor_count),
    entry!(environment_tick_count),
    entry!(exception_ctor),
    entry!(exception_get_message),
    entry!(exception_runtime_inner_exception),
    entry!(exception_runtime_message),
    #[cfg(feature = "reflection")]
    entry!(field_get_raw_constant),
    #[cfg(feature = "reflection")]
    entry!(field_get_value),
    #[cfg(feature = "reflection")]
    entry!(field_is_literal),
    #[cfg(feature = "reflection")]
    entry!(field_is_static),
    #[cfg(feature = "reflection")]
    entry!(field_set_value),
    entry!(fs_close),
    entry!(fs_create_dir),
    entry!(fs_delete_dir),
    entry!(fs_delete_file),
    entry!(fs_dir_exists),
    entry!(fs_file_exists),
    entry!(fs_flush),
    entry!(fs_length),
    entry!(fs_list),
    entry!(fs_move),
    entry!(fs_open),
    entry!(fs_read),
    entry!(fs_seek),
    entry!(fs_set_length),
    entry!(fs_write),
    #[cfg(feature = "gc")]
    entry!(gc_collect),
    entry!(get_custom_attributes),
    entry!(initialize_array),
    #[cfg(feature = "NETMFv4_4")]
    entry!(int32_parse),
    entry!(int32_to_string),
    #[cfg(feature = "NETMFv4_4")]
    entry!(int64_parse),
    entry!(int64_to_string),
    entry!(interlocked_compare_exchange),
    entry!(intptr_from_raw_value),
    entry!(intptr_to_raw_value),
    #[cfg(feature = "collections")]
    entry!(list_add),
    #[cfg(feature = "collections")]
    entry!(list_clear),
    #[cfg(feature = "collections")]
    entry!(list_get_count),
    #[cfg(feature = "collections")]
    entry!(list_get_item),
    #[cfg(feature = "collections")]
    entry!(list_insert),
    #[cfg(feature = "collections")]
    entry!(list_remove_at),
    #[cfg(feature = "collections")]
    entry!(list_set_item),
    #[cfg(feature = "collections")]
    entry!(map_add),
    #[cfg(feature = "collections")]
    entry!(map_contains),
    #[cfg(feature = "collections")]
    entry!(map_get_count),
    #[cfg(feature = "collections")]
    entry!(map_get_item),
    #[cfg(feature = "collections")]
    entry!(map_remove),
    #[cfg(feature = "collections")]
    entry!(map_set_item),
    entry!(marshal_alloc_hglobal),
    entry!(marshal_free_hglobal),
    entry!(marshal_read_byte),
    entry!(marshal_read_int16),
    entry!(marshal_read_int32),
    entry!(marshal_read_int64),
    entry!(marshal_size_of),
    entry!(marshal_write_byte),
    entry!(marshal_write_int16),
    entry!(marshal_write_int32),
    entry!(marshal_write_int64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(math_abs_f64),
    #[cfg(feature = "NETMFv4_4")]
    entry!(math_abs_int32),
    #[cfg(feature = "NETMFv4_4")]
    entry!(math_abs_int64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_acos_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_asin_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_atan2_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_atan_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(math_ceiling_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_cos_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_cosh_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_exp_f64),
    #[cfg(feature = "float")]
    entry!(math_floor_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_ieee_remainder_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_log10_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_log_base_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_log_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(math_max_f64),
    #[cfg(feature = "NETMFv4_4")]
    entry!(math_max_int32),
    #[cfg(feature = "NETMFv4_4")]
    entry!(math_max_int64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(math_min_f64),
    #[cfg(feature = "NETMFv4_4")]
    entry!(math_min_int32),
    #[cfg(feature = "NETMFv4_4")]
    entry!(math_min_int64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_pow_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(math_round_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(math_sign_f64),
    #[cfg(feature = "NETMFv4_4")]
    entry!(math_sign_int32),
    #[cfg(feature = "NETMFv4_4")]
    entry!(math_sign_int64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_sin_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_sinh_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_sqrt_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_tan_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    entry!(math_tanh_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    entry!(math_truncate_f64),
    entry!(md_array_address),
    entry!(md_array_get),
    entry!(md_array_get_length),
    entry!(md_array_length),
    entry!(md_array_set),
    #[cfg(feature = "reflection")]
    entry!(member_get_type),
    entry!(mmio_read32),
    entry!(mmio_write32),
    entry!(mmio_read8),
    entry!(mmio_write8),
    entry!(mmio_read16),
    entry!(mmio_write16),
    #[cfg(feature = "reflection")]
    entry!(method_invoke),
    #[cfg(feature = "reflection")]
    entry!(method_is_abstract),
    #[cfg(feature = "reflection")]
    entry!(method_is_final),
    #[cfg(feature = "reflection")]
    entry!(method_is_public),
    #[cfg(feature = "reflection")]
    entry!(method_is_static),
    #[cfg(feature = "reflection")]
    entry!(method_is_virtual),
    #[cfg(feature = "reflection")]
    entry!(method_parameter_count),
    #[cfg(feature = "reflection")]
    entry!(method_parameter_custom_attributes),
    #[cfg(feature = "reflection")]
    entry!(method_parameter_name),
    #[cfg(feature = "reflection")]
    entry!(method_parameter_type),
    entry!(monitor_enter),
    entry!(monitor_exit),
    entry!(monitor_pulse),
    entry!(monitor_pulse_all),
    entry!(monitor_try_enter),
    entry!(monitor_try_enter_timeout),
    entry!(monitor_wait),
    entry!(monitor_wait_timed_out),
    entry!(monitor_wait_timeout),
    entry!(net_iface_count),
    entry!(net_iface_flags),
    entry!(net_iface_gateway),
    entry!(net_iface_ipv4),
    entry!(net_iface_oper_status),
    entry!(net_iface_subnet),
    entry!(net_iface_type),
    entry!(net_is_available),
    entry!(object_ctor),
    entry!(object_get_type),
    entry!(object_get_hash_code),
    entry!(object_reference_equals),
    entry!(object_to_string),
    #[cfg(feature = "collections")]
    entry!(queue_dequeue),
    #[cfg(feature = "collections")]
    entry!(queue_peek),
    #[cfg(feature = "reflection")]
    entry!(reflect_handle_equals),
    #[cfg(feature = "reflection")]
    entry!(reflect_handle_not_equals),
    #[cfg(feature = "finalizers")]
    entry!(reregister_finalize),
    entry!(serial_bytes_to_read),
    entry!(serial_bytes_to_write),
    entry!(serial_close),
    entry!(serial_discard_in),
    entry!(serial_discard_out),
    entry!(serial_flush),
    entry!(serial_open),
    entry!(serial_read),
    entry!(serial_write),
    #[cfg(feature = "float")]
    entry!(single_parse),
    #[cfg(feature = "float")]
    entry!(single_to_exponential),
    #[cfg(feature = "float")]
    entry!(single_to_fixed),
    #[cfg(feature = "float")]
    entry!(single_to_string),
    entry!(socket_accept),
    entry!(socket_close),
    entry!(socket_connect_poll),
    entry!(socket_connect_start),
    entry!(socket_listen),
    entry!(socket_local_port),
    entry!(socket_recv),
    entry!(socket_send),
    entry!(socket_set_recv_timeout),
    entry!(socket_udp_bind),
    entry!(socket_udp_recv_from),
    entry!(socket_udp_send_to),
    #[cfg(feature = "collections")]
    entry!(stack_peek),
    #[cfg(feature = "collections")]
    entry!(stack_pop),
    entry!(storage_is_mounted),
    entry!(storage_mount_ram),
    entry!(storage_mount_sd_over_spi),
    entry!(storage_mount_sd_over_spi_bus),
    entry!(storage_unmount),
    entry!(string_concat),
    entry!(string_concat3),
    entry!(string_concat_object2),
    entry!(string_concat_object3),
    entry!(string_create_from_chars),
    entry!(string_ctor_char_array),
    entry!(string_ctor_char_array_range),
    entry!(string_ctor_char_ptr),
    entry!(string_ctor_char_ptr_range),
    entry!(string_ctor_char_repeat),
    #[cfg(feature = "text")]
    entry!(string_contains),
    #[cfg(feature = "text")]
    entry!(string_ends_with),
    entry!(string_equals),
    entry!(string_get_chars),
    entry!(string_get_length),
    entry!(string_get_pinnable_reference),
    #[cfg(feature = "text")]
    entry!(string_index_of_char),
    #[cfg(feature = "text")]
    entry!(string_index_of_string),
    #[cfg(feature = "text")]
    entry!(string_insert),
    entry!(string_intern),
    entry!(string_is_interned),
    entry!(string_is_null_or_empty),
    #[cfg(feature = "text")]
    entry!(string_join),
    #[cfg(feature = "text")]
    entry!(string_last_index_of_char),
    entry!(string_not_equals),
    #[cfg(feature = "text")]
    entry!(string_pad_left),
    #[cfg(feature = "text")]
    entry!(string_pad_right),
    #[cfg(feature = "text")]
    entry!(string_remove),
    #[cfg(feature = "text")]
    entry!(string_replace_char),
    #[cfg(feature = "text")]
    entry!(string_replace_string),
    #[cfg(feature = "text")]
    entry!(string_split_char),
    #[cfg(feature = "text")]
    entry!(string_starts_with),
    entry!(string_substring),
    entry!(string_substring_len),
    #[cfg(feature = "text")]
    entry!(string_to_char_array),
    #[cfg(feature = "text")]
    entry!(string_to_lower),
    #[cfg(feature = "text")]
    entry!(string_to_upper),
    #[cfg(feature = "text")]
    entry!(string_trim),
    #[cfg(feature = "finalizers")]
    entry!(suppress_finalize),
    entry!(thread_join),
    entry!(thread_join_timeout),
    entry!(thread_sleep),
    entry!(thread_start),
    entry!(thread_yield),
    entry!(aead_import_key),
    entry!(aead_siv_decrypt),
    entry!(aead_siv_encrypt),
    entry!(tls_alpn_is),
    entry!(tls_client_config),
    entry!(tls_client_config_alpn),
    entry!(tls_client_new),
    entry!(tls_close),
    entry!(tls_default_stack),
    entry!(tls_drop_key),
    entry!(tls_exporter_key),
    entry!(tls_peer_cert),
    entry!(tls_process),
    entry!(tls_read_plain),
    entry!(tls_read_tls),
    entry!(tls_server_config),
    entry!(tls_server_new),
    entry!(tls_session_flags),
    entry!(tls_wants_write),
    entry!(tls_write_plain),
    entry!(tls_write_tls),
    entry!(type_from_handle),
    #[cfg(feature = "reflection")]
    entry!(type_get_assembly),
    #[cfg(feature = "reflection")]
    entry!(type_get_base_type),
    #[cfg(feature = "reflection")]
    entry!(type_get_constructor),
    entry!(type_get_field),
    #[cfg(feature = "reflection")]
    entry!(type_get_fields),
    #[cfg(feature = "reflection")]
    entry!(type_get_full_name),
    entry!(type_get_method),
    #[cfg(feature = "reflection")]
    entry!(type_get_methods),
    entry!(type_get_name),
    #[cfg(feature = "exceptions")]
    entry!(type_initialization_exception_runtime_type_name),
    #[cfg(feature = "reflection")]
    entry!(type_get_namespace),
    entry!(type_get_property),
    #[cfg(feature = "reflection")]
    entry!(type_is_abstract),
    #[cfg(feature = "reflection")]
    entry!(type_is_array),
    #[cfg(feature = "reflection")]
    entry!(type_is_class),
    #[cfg(feature = "reflection")]
    entry!(type_is_enum),
    #[cfg(feature = "reflection")]
    entry!(type_is_interface),
    #[cfg(feature = "reflection")]
    entry!(type_is_not_public),
    #[cfg(feature = "reflection")]
    entry!(type_is_public),
    #[cfg(feature = "reflection")]
    entry!(type_is_value_type),
    entry!(type_property_custom_attributes),
    entry!(value_type_equals),
    entry!(value_type_get_hash_code),
    #[cfg(feature = "finalizers")]
    entry!(wait_for_pending_finalizers),
    #[cfg(feature = "gc")]
    entry!(weak_make_cell),
    #[cfg(feature = "gc")]
    entry!(weak_read_cell),
    #[cfg(feature = "gc")]
    entry!(weak_write_cell),
];

/// The intrinsic-ABI LEVEL a target advertises in its Lamella Link profile identity. Bump it ONLY
/// when an EXISTING intrinsic id's semantics change incompatibly -- adding or removing intrinsics
/// already changes [`registry_fingerprint`] (the surface CONTENT hash), not the level.
pub const INTRINSIC_ABI: u16 = 1;

/// Every intrinsic id this build registers, in registry order -- the resident-surface listing a
/// Lamella Link `PROFILE_MANIFEST` carries (a resident corlib is compatible
/// iff its `[RuntimeProvided]` demand set is a SUBSET of these).
pub fn registry_ids() -> impl Iterator<Item = u32> {
    REGISTRY.iter().map(|(id, _)| *id)
}

/// A short display name for THIS build's intrinsic surface -- the Lamella Link profile identity's
/// name hint. Derived from the same features that shape [`REGISTRY`], so it cannot drift from
/// [`registry_fingerprint`]: the desktop superset is "full", the NETMF-conformant device tier
/// "netmf-v4_4", the bcl floor "kernel-floor".
pub fn profile_name() -> &'static str {
    if cfg!(feature = "varargs") || cfg!(feature = "typed-references") {
        "full"
    } else if cfg!(feature = "NETMFv4_4") {
        "netmf-v4_4"
    } else {
        "kernel-floor"
    }
}

/// The stable registry id of an intrinsic named `name`: FNV-1a-32 of the fn's own name -- the SAME
/// scheme `REGISTRY` uses. A binder stamps this at bind time (`intrinsic_id(stringify!(the_fn))`) so
/// the bake records the id WITHOUT a function-pointer comparison, which is unreliable on wasm32 (a fn
/// can occupy more than one indirect-function-table slot, so the bound fn may not compare equal to the
/// registered one for the same logical intrinsic).
#[must_use]
pub const fn intrinsic_id(name: &str) -> u32 {
    let bytes = name.as_bytes();
    let mut hash: u32 = 0x811c_9dc5;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u32;
        hash = hash.wrapping_mul(0x0100_0193);
        i += 1;
    }
    hash
}

/// The stable id of `func`, for the bake (`None` if it is not a registered intrinsic).
#[allow(unpredictable_function_pointer_comparisons)]
#[must_use]
pub fn intrinsic_id_of(func: IntrinsicFn) -> Option<u32> {
    REGISTRY
        .iter()
        .find(|(_, registered)| *registered == func)
        .map(|(id, _)| *id)
}

/// The function registered under `id` in THIS build (`None` = the image was baked by a
/// profile with an intrinsic this build lacks).
#[must_use]
pub fn intrinsic_by_id(id: u32) -> Option<IntrinsicFn> {
    REGISTRY
        .iter()
        .find(|(registered, _)| *registered == id)
        .map(|(_, func)| *func)
}

/// This build's registry position of `id` -- the compact form a booted module keeps per method
/// (a `u16` instead of a function pointer), resolved back by [`intrinsic_at`] in one indexed
/// read of the static table.
#[must_use]
pub fn intrinsic_index_of(id: u32) -> Option<u16> {
    REGISTRY
        .iter()
        .position(|(registered, _)| *registered == id)
        .map(|position| position as u16)
}

/// The function at a registry `index` from [`intrinsic_index_of`].
#[must_use]
pub fn intrinsic_at(index: u16) -> Option<IntrinsicFn> {
    REGISTRY.get(index as usize).map(|(_, func)| *func)
}

/// The stable id at a registry `index` -- [`intrinsic_at`]'s companion, for a caller that needs to
/// know WHICH intrinsic a baked method is rather than to call it.
///
/// A function pointer cannot answer that question: two intrinsics with identical bodies may be
/// merged onto one address, and several of them exist deliberately (an identity anchor whose
/// behavior is supplied by the interpreter has nothing in its body to tell it apart). The id is
/// derived from the name, so it separates them.
///
/// Crate-private: [`Module::intrinsic_id`](crate::module::Module) is the form a caller wants, and
/// an index is this module's own bookkeeping.
#[must_use]
pub(crate) fn id_at(index: u16) -> Option<u32> {
    REGISTRY.get(index as usize).map(|(id, _)| *id)
}

/// A fingerprint of THIS build's registry (FNV-1a over the id sequence). A baked image records
/// the baking build's fingerprint beside its in-record index hints; a booting build whose
/// fingerprint matches reads the hints straight from flash (zero resident index state), and any
/// other build falls back to resolving ids at boot.
#[must_use]
pub fn registry_fingerprint() -> u64 {
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for (id, _) in REGISTRY {
        for byte in id.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_unique() {
        for (index, (id, _)) in REGISTRY.iter().enumerate() {
            for (other, _) in REGISTRY.iter().skip(index + 1) {
                assert_ne!(id, other, "fnv collision -- rename one intrinsic");
            }
        }
    }

    #[test]
    fn ids_round_trip_to_the_same_function() {
        for (id, func) in REGISTRY {
            let back = intrinsic_by_id(*id).expect("registered id resolves");
            #[allow(unpredictable_function_pointer_comparisons)]
            let same = back == *func;
            assert!(same, "id 0x{id:08X} resolves to its own function");
        }
    }

    #[test]
    fn intrinsic_id_reproduces_the_registry_scheme() {
        assert_eq!(intrinsic_id("object_ctor"), 0x2BA7_5CFD);
        assert_eq!(intrinsic_id("initialize_array"), 0x1798_56F1);
        assert_eq!(intrinsic_id("console_write"), 0x5B30_A692);
        assert_eq!(intrinsic_id("array_clone"), 0x70AB_44F0);
        assert_eq!(intrinsic_id("string_concat"), 0xF106_E8DD);
        assert_eq!(intrinsic_id("mmio_write32"), 0xF37A_3AFA);
        assert_eq!(intrinsic_id("mmio_read8"), 0x8B9D_EF8C);
        assert_eq!(intrinsic_id("mmio_write8"), 0xA004_2EBD);
        assert_eq!(intrinsic_id("mmio_read16"), 0x75B6_1213);
        assert_eq!(intrinsic_id("mmio_write16"), 0x5F7F_622C);
    }
}
