//! The download's arithmetic: the register offsets through the window,
//! the settings image's padding and placement, and the trailer word.

use crate::backplane::{CHIPCOMMON, register_offset};
use crate::cores::{
    ARM_CM3_WRAPPER, SDIOD, SOCSRAM, SOCSRAM_WRAPPER, agent, chipcommon, sdiod, socsram,
};
use crate::download::{RAM_SIZE, pad_piece, padded_len, settings_base, settings_trailer};

#[test]
fn the_register_offsets_carry_the_32_bit_flag() {
    assert_eq!(register_offset(ARM_CM3_WRAPPER + agent::IOCTL), 0xB408);
    assert_eq!(register_offset(ARM_CM3_WRAPPER + agent::RESET_CTL), 0xB800);
    assert_eq!(register_offset(SOCSRAM_WRAPPER + agent::IOCTL), 0xC408);
    assert_eq!(register_offset(SOCSRAM_WRAPPER + agent::RESET_CTL), 0xC800);
    assert_eq!(register_offset(SOCSRAM + socsram::BANK_INDEX), 0xC010);
    assert_eq!(register_offset(SOCSRAM + socsram::BANK_PDA), 0xC044);
    assert_eq!(register_offset(SDIOD + sdiod::INTSTATUS), 0xA020);
    assert_eq!(
        register_offset(CHIPCOMMON + chipcommon::SR_CONTROL1),
        0x8508
    );
    assert_eq!(register_offset(RAM_SIZE - 4), 0xFFFC);
    assert_eq!(register_offset(CHIPCOMMON), 0x8000);
}

#[test]
fn the_settings_trailer_is_the_word_count_and_its_complement() {
    assert_eq!(settings_trailer(1152), 0xFEDF_0120, "the worked example");
    assert_eq!(
        settings_trailer(576),
        0xFF6F_0090,
        "the GIGA's 576-byte image"
    );
    assert_eq!(
        settings_trailer(768),
        0xFF3F_00C0,
        "the Pico's 768-byte image"
    );
    assert_eq!(settings_trailer(128), 0xFFDF_0020);
    let right = settings_trailer(128);
    let unpadded = ((!25u32) << 16) | 25;
    let in_bytes = ((!128u32) << 16) | 128;
    let whole_word = !32u32;
    assert_eq!(unpadded, 0xFFE6_0019);
    assert_eq!(in_bytes, 0xFF7F_0080);
    assert_eq!(whole_word, 0xFFFF_FFDF);
    assert!(unpadded != right && in_bytes != right && whole_word != right);
}

#[test]
fn the_settings_image_is_padded_to_64_and_placed_under_the_trailer() {
    assert_eq!(padded_len(1100), 1152);
    assert_eq!(padded_len(100), 128);
    assert_eq!(padded_len(64), 64);
    assert_eq!(padded_len(65), 128);
    assert_eq!(padded_len(1), 64);
    assert_eq!(settings_base(128), 0x0007_FF7C);
    assert_eq!(settings_base(1152), 0x0007_FB7C);
    assert_eq!(settings_base(576), 0x0007_FDBC, "the GIGA's placement");
}

#[test]
fn the_padding_is_written_in_power_of_two_pieces() {
    let pieces = |mut remaining: usize| {
        let mut out = std::vec::Vec::new();
        while remaining > 0 {
            let piece = pad_piece(remaining);
            out.push(piece);
            remaining -= piece;
        }
        out
    };
    assert_eq!(pieces(28), [16, 8, 4]);
    assert_eq!(pieces(63), [32, 16, 8, 4, 2, 1]);
    assert_eq!(pieces(64), [32, 32]);
    assert_eq!(pieces(1), [1]);
    assert_eq!(pad_piece(0), 0);
}
