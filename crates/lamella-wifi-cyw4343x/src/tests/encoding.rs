//! The wire orders and the command word, on the values the chip is known
//! to send and receive.

use crate::gspi::{
    CONFIG_DWORD, TEST_PATTERN, command_word, decode_configured, decode_default, encode_configured,
    encode_default,
};
use crate::transport::Func;

/// The chip identity word of the CYW43439 as it arrives in the configured
/// order.
const CHIP_ID_WORD: u32 = 0x1545_A9AF;

#[test]
fn the_test_pattern_arrives_halfword_swapped_in_the_power_up_order() {
    assert_eq!(encode_default(TEST_PATTERN), [0xBE, 0xAD, 0xFE, 0xED]);
    assert_eq!(decode_default([0xBE, 0xAD, 0xFE, 0xED]), TEST_PATTERN);
}

#[test]
fn the_configuration_dword_goes_out_in_the_power_up_order() {
    assert_eq!(encode_default(CONFIG_DWORD), [0x00, 0xB3, 0x00, 0x02]);
}

#[test]
fn the_chip_identity_arrives_least_significant_byte_first_once_configured() {
    assert_eq!(decode_configured([0xAF, 0xA9, 0x45, 0x15]), CHIP_ID_WORD);
    assert_eq!(encode_configured(CHIP_ID_WORD), [0xAF, 0xA9, 0x45, 0x15]);
}

#[test]
fn the_two_orders_round_trip_every_byte_position() {
    for value in [0u32, 0x0102_0304, 0xFFFF_FFFF, 0x8000_0001, 0x00FF_0000] {
        assert_eq!(decode_default(encode_default(value)), value);
        assert_eq!(decode_configured(encode_configured(value)), value);
    }
}

#[test]
fn the_pattern_shifted_by_one_bit_decodes_as_the_fixture_predicts() {
    assert_eq!(decode_default([0x7D, 0x5B, 0xFD, 0xDA]), 0xFDDA_7D5B);
    assert_eq!(decode_configured([0xBE, 0xAD, 0xFE, 0xED]), 0xEDFE_ADBE);
}

#[test]
fn the_command_word_places_every_field() {
    assert_eq!(command_word(false, true, Func::F0, 0x14, 4), 0x4000_A004);
    assert_eq!(command_word(true, true, Func::F0, 0x00, 4), 0xC000_0004);
    assert_eq!(command_word(true, true, Func::F0, 0x1D, 1), 0xC000_E801);
    assert_eq!(command_word(true, true, Func::F0, 0x04, 2), 0xC000_2002);
    assert_eq!(command_word(true, true, Func::F1, 0x1000A, 1), 0xD800_5001);
    assert_eq!(command_word(false, true, Func::F1, 0x8000, 4), 0x5400_0004);
    assert_eq!(command_word(false, true, Func::F1, 0x1000E, 1), 0x5800_7001);
    assert_eq!(command_word(false, true, Func::F2, 0, 2047), 0x6000_07FF);
    assert_eq!(command_word(false, false, Func::F2, 0, 1), 0x2000_0001);
}
