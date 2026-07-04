//! The intrinsic registry: every intrinsic under a BUILD-STABLE id (FNV-1a of the
//! function's own name), so a baked image records ids and a booting module rebinds
//! them to this build's function pointers. Ids derive from names, so the registry is
//! append-only by construction; a feature-gated intrinsic simply has no entry in a
//! build without the feature, and an image needing it fails the rebind loudly.

use crate::intrinsics::*;
use crate::module::IntrinsicFn;

/// Every intrinsic this build compiles, as `(stable id, function)`.
static REGISTRY: &[(u32, IntrinsicFn)] = &[
    #[cfg(feature = "NETMFv4_4")]
    (0xC4EA5B05, activator_create_instance),
    #[cfg(feature = "varargs")]
    (0x7AC18FC9, arg_iterator_cookie),
    #[cfg(feature = "varargs")]
    (0x44D76201, arg_iterator_get),
    #[cfg(feature = "varargs")]
    (0x8EEE9351, arg_iterator_remaining),
    (0x70AB44F0, array_clone),
    (0x189C62D8, array_empty),
    (0xFB6CAA9F, array_get_value),
    (0x6AFC33EB, array_set_value),
    #[cfg(feature = "NETMFv4_4")]
    (0x03607F3B, assembly_full_name),
    #[cfg(feature = "NETMFv4_4")]
    (0xBA5A3D87, assembly_get_type),
    #[cfg(feature = "NETMFv4_4")]
    (0x500F871C, assembly_get_types),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0x4BB3CD6D, bitconverter_double_to_int64_bits),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0x6387D691, bitconverter_int32_bits_to_single),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0x2671E6E5, bitconverter_int64_bits_to_double),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0x8B5EFFD9, bitconverter_single_to_int32_bits),
    #[cfg(feature = "NETMFv4_4")]
    (0xB8425F0F, boolean_parse),
    (0xCA7DFC3B, boolean_to_string),
    (0x156598C3, buffer_block_copy),
    (0xBEFD4573, buffer_byte_length),
    #[cfg(feature = "NETMFv4_4")]
    (0x44004A38, char_is_digit),
    #[cfg(feature = "NETMFv4_4")]
    (0xB038494B, char_is_letter),
    #[cfg(feature = "NETMFv4_4")]
    (0x1B514117, char_is_letter_or_digit),
    #[cfg(feature = "NETMFv4_4")]
    (0xCAD30E04, char_is_lower),
    #[cfg(feature = "NETMFv4_4")]
    (0x91018CC9, char_is_upper),
    #[cfg(feature = "NETMFv4_4")]
    (0x135FA6A5, char_is_white_space),
    #[cfg(feature = "NETMFv4_4")]
    (0xECAE1909, char_to_lower),
    (0xE2DE5AAD, char_to_string),
    #[cfg(feature = "NETMFv4_4")]
    (0x24EA7BD0, char_to_upper),
    #[cfg(feature = "NETMFv4_4")]
    (0x9CE422A7, collection_contains),
    #[cfg(feature = "NETMFv4_4")]
    (0xD6BC1F1A, collection_push),
    (0x5B30A692, console_write),
    (0xF8192B63, console_write_bool),
    (0x6102238F, console_write_char),
    #[cfg(feature = "float")]
    (0x33C4DA4A, console_write_double),
    (0xC6D365B9, console_write_int32),
    (0x5ADB0A7A, console_write_int64),
    (0x8BCBA345, console_write_line),
    (0x72FC7442, console_write_line_bool),
    (0x83EC0E02, console_write_line_char),
    #[cfg(feature = "float")]
    (0x29297713, console_write_line_double),
    (0xF8A00D03, console_write_line_empty),
    (0x4E31DE02, console_write_line_int32),
    (0xDE2A71ED, console_write_line_int64),
    (0x27654D6D, console_write_line_object),
    #[cfg(feature = "float")]
    (0x369A7F06, console_write_line_single),
    (0xACDF0D63, console_write_line_uint32),
    (0xBCE5E258, console_write_line_uint64),
    #[cfg(feature = "float")]
    (0x177A57BF, console_write_single),
    #[cfg(feature = "NETMFv4_4")]
    (0xFB309D10, constructor_invoke),
    #[cfg(feature = "NETMFv4_4")]
    (0xFFE70BB5, convert_to_boolean_int),
    #[cfg(feature = "NETMFv4_4")]
    (0xFC358755, convert_to_byte_int),
    #[cfg(feature = "NETMFv4_4")]
    (0x02A380AB, convert_to_char_int),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0xD7807889, convert_to_int32_double),
    (0xD87EDB0A, datetime_now_ticks),
    (0xDEBF16DC, debug_write),
    (0x791B18F0, decimal_add),
    (0x48DC7348, decimal_compare),
    (0xF5ABD624, decimal_divide),
    #[cfg(feature = "float")]
    (0xEC4A8F87, decimal_from_double),
    (0xC281ADA1, decimal_multiply),
    (0xF5AA0A9C, decimal_remainder),
    (0x0D0DFDFD, decimal_subtract),
    #[cfg(feature = "float")]
    (0x0F9A6248, decimal_to_double),
    (0x0D2EB098, delegate_combine),
    (0xC33210B3, delegate_remove),
    (0xC9F91A4A, dns_resolve_host),
    #[cfg(feature = "float")]
    (0x6C8ABBC4, double_to_exponential),
    #[cfg(feature = "float")]
    (0xED002E35, double_to_fixed),
    #[cfg(feature = "float")]
    (0x2F0F718C, double_to_string),
    (0x65C41EDA, enum_format),
    (0x302EC4F3, enum_get_name),
    (0x599F5580, enum_get_names),
    (0x83442896, enum_get_values),
    (0xEEAB46B7, enum_is_defined),
    (0x531B10E4, enum_parse),
    (0xEB8AF736, environment_get_variable),
    (0x2B518CE1, environment_processor_count),
    (0x83BAB022, environment_tick_count),
    (0x56E85E99, exception_ctor),
    (0x0ABFAE4D, exception_get_message),
    #[cfg(feature = "NETMFv4_4")]
    (0xE574285E, field_get_value),
    #[cfg(feature = "NETMFv4_4")]
    (0x89D68A1A, field_set_value),
    #[cfg(feature = "gc")]
    (0xB70FF342, gc_collect),
    (0x8DF82675, get_custom_attributes),
    (0x179856F1, initialize_array),
    #[cfg(feature = "NETMFv4_4")]
    (0x4339330F, int32_parse),
    (0x330E103B, int32_to_string),
    #[cfg(feature = "NETMFv4_4")]
    (0x48ED8808, int64_parse),
    (0x305A26A0, int64_to_string),
    (0x7CE886D5, interlocked_compare_exchange),
    (0x0137A90A, intptr_from_raw_value),
    (0xE0043367, intptr_to_raw_value),
    #[cfg(feature = "NETMFv4_4")]
    (0x7B123381, list_add),
    #[cfg(feature = "NETMFv4_4")]
    (0x1107C403, list_clear),
    #[cfg(feature = "NETMFv4_4")]
    (0x3CC99D9A, list_get_count),
    #[cfg(feature = "NETMFv4_4")]
    (0x36B7062C, list_get_item),
    #[cfg(feature = "NETMFv4_4")]
    (0x4DE76053, list_insert),
    #[cfg(feature = "NETMFv4_4")]
    (0x589FEDA6, list_remove_at),
    #[cfg(feature = "NETMFv4_4")]
    (0xD9A18B58, list_set_item),
    #[cfg(feature = "NETMFv4_4")]
    (0xA07A9A91, map_add),
    #[cfg(feature = "NETMFv4_4")]
    (0x34EF5207, map_contains),
    #[cfg(feature = "NETMFv4_4")]
    (0x93D9E9EA, map_get_count),
    #[cfg(feature = "NETMFv4_4")]
    (0xEFC0313C, map_get_item),
    #[cfg(feature = "NETMFv4_4")]
    (0x7988548E, map_remove),
    #[cfg(feature = "NETMFv4_4")]
    (0x3BAC6308, map_set_item),
    (0xAE897237, marshal_alloc_hglobal),
    (0xD64D17C0, marshal_free_hglobal),
    (0xE48BDDE1, marshal_read_byte),
    (0x697C03A3, marshal_read_int16),
    (0x7D80A04D, marshal_read_int32),
    (0xF974978E, marshal_read_int64),
    (0x1C72CC89, marshal_size_of),
    (0xFD94BDA0, marshal_write_byte),
    (0x55DB7A50, marshal_write_int16),
    (0xD1D62D56, marshal_write_int32),
    (0x69E25591, marshal_write_int64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0x64FBC3E3, math_abs_f64),
    #[cfg(feature = "NETMFv4_4")]
    (0xAEE5A8F7, math_abs_int32),
    #[cfg(feature = "NETMFv4_4")]
    (0x16D980BC, math_abs_int64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0x00339648, math_ceiling_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    (0x4913ED8C, math_cos_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    (0xF53843E8, math_exp_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0xAA05CFD5, math_floor_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    (0x6F2CEF94, math_log10_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    (0x886CCAA1, math_log_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0x5689B11D, math_max_f64),
    #[cfg(feature = "NETMFv4_4")]
    (0x887947DD, math_max_int32),
    #[cfg(feature = "NETMFv4_4")]
    (0x046D3F1E, math_max_int64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0x38CC22FF, math_min_f64),
    #[cfg(feature = "NETMFv4_4")]
    (0x2C7F5DBB, math_min_int32),
    #[cfg(feature = "NETMFv4_4")]
    (0x9C86C9D0, math_min_int64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    (0x3A8B911D, math_pow_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0x13982B3B, math_round_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0x4A6772F6, math_sign_f64),
    #[cfg(feature = "NETMFv4_4")]
    (0x08299D26, math_sign_int32),
    #[cfg(feature = "NETMFv4_4")]
    (0x20368EE1, math_sign_int64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    (0x29FB8E19, math_sin_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    (0xA64F053D, math_sqrt_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "math-transcendental")]
    (0x30B91E44, math_tan_f64),
    #[cfg(feature = "NETMFv4_4")]
    #[cfg(feature = "float")]
    (0x88990959, math_truncate_f64),
    (0xA071F45D, md_array_address),
    (0x97875BB9, md_array_get),
    (0xCAB8CF06, md_array_get_length),
    (0xC100C0EF, md_array_length),
    (0x7E7FC5C5, md_array_set),
    #[cfg(feature = "NETMFv4_4")]
    (0x46FDE575, member_get_type),
    (0x09BB783D, mmio_read32),
    (0xF37A3AFA, mmio_write32),
    #[cfg(feature = "NETMFv4_4")]
    (0xD83F0EA5, method_invoke),
    #[cfg(feature = "NETMFv4_4")]
    (0x7CEC4F3C, method_is_abstract),
    #[cfg(feature = "NETMFv4_4")]
    (0x51E9DD62, method_is_final),
    #[cfg(feature = "NETMFv4_4")]
    (0x3BB9244F, method_is_public),
    #[cfg(feature = "NETMFv4_4")]
    (0xCD6324A4, method_is_static),
    #[cfg(feature = "NETMFv4_4")]
    (0x923C2725, method_is_virtual),
    #[cfg(feature = "NETMFv4_4")]
    (0xEED86904, method_parameter_count),
    #[cfg(feature = "NETMFv4_4")]
    (0xC86B4C16, method_parameter_name),
    #[cfg(feature = "NETMFv4_4")]
    (0x67D0181D, method_parameter_type),
    (0x972A66E4, monitor_enter),
    (0xC956FD4A, monitor_exit),
    (0xA43F2673, monitor_pulse),
    (0x3868631F, monitor_pulse_all),
    (0x819E5A8A, monitor_try_enter),
    (0x580BA387, monitor_wait),
    (0x2BA75CFD, object_ctor),
    (0x91980EE2, object_get_type),
    (0xF0B01450, object_reference_equals),
    (0xADC7CD4A, object_to_string),
    #[cfg(feature = "NETMFv4_4")]
    (0x008AC6E5, queue_dequeue),
    #[cfg(feature = "NETMFv4_4")]
    (0x443D8AFE, queue_peek),
    #[cfg(feature = "NETMFv4_4")]
    (0x4FB94F27, reflect_handle_equals),
    #[cfg(feature = "NETMFv4_4")]
    (0x354FFC89, reflect_handle_not_equals),
    #[cfg(feature = "finalizers")]
    (0xFFBA37D8, reregister_finalize),
    #[cfg(feature = "float")]
    (0x14CF6D25, single_parse),
    #[cfg(feature = "float")]
    (0x0043C3CF, single_to_exponential),
    #[cfg(feature = "float")]
    (0x03EAB562, single_to_fixed),
    #[cfg(feature = "float")]
    (0x47F9C4A1, single_to_string),
    (0xE9A8BF2D, socket_accept),
    (0x5DE41927, socket_close),
    (0xCDDF5557, socket_connect_poll),
    (0x37B58466, socket_connect_start),
    (0x7C84D69A, socket_listen),
    (0xF958666A, socket_local_port),
    (0xED9A0751, socket_recv),
    (0xCE6D884B, socket_send),
    (0xD333B536, socket_udp_bind),
    (0x75D8BF1A, socket_udp_recv_from),
    (0xA8245DE1, socket_udp_send_to),
    #[cfg(feature = "NETMFv4_4")]
    (0x16294F2F, stack_peek),
    #[cfg(feature = "NETMFv4_4")]
    (0x70C91FC7, stack_pop),
    #[cfg(feature = "NETMFv4_4")]
    (0x64D3A0E6, string_builder_append_char),
    #[cfg(feature = "NETMFv4_4")]
    (0x31DC102B, string_builder_append_int),
    #[cfg(feature = "NETMFv4_4")]
    (0x1826588B, string_builder_append_string),
    #[cfg(feature = "NETMFv4_4")]
    (0x76171274, string_builder_get_capacity),
    #[cfg(feature = "NETMFv4_4")]
    (0x574CA66C, string_builder_get_char),
    #[cfg(feature = "NETMFv4_4")]
    (0x0183320C, string_builder_get_length),
    #[cfg(feature = "NETMFv4_4")]
    (0x07223CB0, string_builder_insert),
    #[cfg(feature = "NETMFv4_4")]
    (0x70762A85, string_builder_remove),
    #[cfg(feature = "NETMFv4_4")]
    (0xA2B711F8, string_builder_replace_char),
    #[cfg(feature = "NETMFv4_4")]
    (0x8E8A1368, string_builder_set_char),
    #[cfg(feature = "NETMFv4_4")]
    (0xD97F7360, string_builder_set_length),
    #[cfg(feature = "NETMFv4_4")]
    (0xE68F9BB4, string_builder_to_string),
    (0xF106E8DD, string_concat),
    (0x5BE0AEAA, string_concat3),
    (0x035C6D55, string_concat_object2),
    (0x025C6BC2, string_concat_object3),
    #[cfg(feature = "NETMFv4_4")]
    (0x91E30098, string_contains),
    #[cfg(feature = "NETMFv4_4")]
    (0xDD31F9E4, string_ends_with),
    (0x91FFAFF4, string_equals),
    (0x2FA1F415, string_get_chars),
    (0xF5A7A3C4, string_get_length),
    #[cfg(feature = "NETMFv4_4")]
    (0xF478116A, string_index_of_char),
    #[cfg(feature = "NETMFv4_4")]
    (0x344B49B7, string_index_of_string),
    #[cfg(feature = "NETMFv4_4")]
    (0xDB27A788, string_insert),
    (0x51B1C9AB, string_is_null_or_empty),
    #[cfg(feature = "NETMFv4_4")]
    (0x0E892BD9, string_join),
    #[cfg(feature = "NETMFv4_4")]
    (0x9FED174B, string_last_index_of_char),
    (0xC83F2912, string_not_equals),
    #[cfg(feature = "NETMFv4_4")]
    (0x77DD9246, string_pad_left),
    #[cfg(feature = "NETMFv4_4")]
    (0xB3A47DFB, string_pad_right),
    #[cfg(feature = "NETMFv4_4")]
    (0x750D755D, string_remove),
    #[cfg(feature = "NETMFv4_4")]
    (0xD26C6440, string_replace_char),
    #[cfg(feature = "NETMFv4_4")]
    (0x6125D8D5, string_replace_string),
    #[cfg(feature = "NETMFv4_4")]
    (0x7FC27D20, string_split_char),
    #[cfg(feature = "NETMFv4_4")]
    (0x28B16475, string_starts_with),
    (0x6D83C408, string_substring),
    (0x12D5B39C, string_substring_len),
    #[cfg(feature = "NETMFv4_4")]
    (0xB7E75053, string_to_char_array),
    #[cfg(feature = "NETMFv4_4")]
    (0x8794297E, string_to_lower),
    #[cfg(feature = "NETMFv4_4")]
    (0xF802EFEB, string_to_upper),
    #[cfg(feature = "NETMFv4_4")]
    (0xB8804229, string_trim),
    #[cfg(feature = "finalizers")]
    (0xEC2440C9, suppress_finalize),
    (0xE21B5996, thread_join),
    (0x82C9864D, thread_sleep),
    (0xA30B0392, thread_start),
    (0xA12B0AF7, thread_yield),
    (0x9AFBA65F, tls_client_config),
    (0x568CA399, tls_client_new),
    (0x820ADA95, tls_close),
    (0xE90321DF, tls_default_stack),
    (0x5E32F422, tls_peer_cert),
    (0x7724979C, tls_process),
    (0x9E5AC3B0, tls_read_plain),
    (0x36B3C805, tls_read_tls),
    (0xA01AC323, tls_server_config),
    (0x5C42EB8D, tls_server_new),
    (0xC034A00E, tls_wants_write),
    (0x10CEF6E1, tls_write_plain),
    (0x01982C74, tls_write_tls),
    (0xDA7D62D9, type_from_handle),
    #[cfg(feature = "NETMFv4_4")]
    (0xD7FD8A3B, type_get_assembly),
    #[cfg(feature = "NETMFv4_4")]
    (0xF5833687, type_get_base_type),
    #[cfg(feature = "NETMFv4_4")]
    (0x3870695D, type_get_constructor),
    (0x926EBB49, type_get_field),
    #[cfg(feature = "NETMFv4_4")]
    (0xBE50BC4E, type_get_fields),
    #[cfg(feature = "NETMFv4_4")]
    (0xFF3856B4, type_get_full_name),
    (0xC8E3E15E, type_get_method),
    #[cfg(feature = "NETMFv4_4")]
    (0x6BBB79D7, type_get_methods),
    (0xE0227654, type_get_name),
    #[cfg(feature = "NETMFv4_4")]
    (0x8D8AE48A, type_get_namespace),
    (0x5F8EEF06, type_get_property),
    #[cfg(feature = "NETMFv4_4")]
    (0xF9ED43F5, type_is_abstract),
    #[cfg(feature = "NETMFv4_4")]
    (0x81F9B23C, type_is_array),
    #[cfg(feature = "NETMFv4_4")]
    (0xECDED1E1, type_is_class),
    #[cfg(feature = "NETMFv4_4")]
    (0x1F735DA6, type_is_enum),
    #[cfg(feature = "NETMFv4_4")]
    (0x02C80C7E, type_is_interface),
    #[cfg(feature = "NETMFv4_4")]
    (0xC55F0C38, type_is_not_public),
    #[cfg(feature = "NETMFv4_4")]
    (0x081BA11A, type_is_public),
    #[cfg(feature = "NETMFv4_4")]
    (0x9E932299, type_is_value_type),
    (0x3393F659, type_property_custom_attributes),
    #[cfg(feature = "finalizers")]
    (0x2CA64B30, wait_for_pending_finalizers),
    #[cfg(feature = "gc")]
    (0x39D92355, weak_make_cell),
    #[cfg(feature = "gc")]
    (0xEC4A3BB3, weak_read_cell),
    #[cfg(feature = "gc")]
    (0x797C820E, weak_write_cell),
];

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
}
