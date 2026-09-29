//! The STM32H747 host's pure parts: the dividers from the kernel clock,
//! the register words, the pin and board tables.

use crate::sdio::{Response, Width};
use crate::stm32h7::{
    Board, Direction, GIGA_R1_WIFI, IDENTIFICATION_HZ, PORTENTA_H7, Pin, SDMMC1_AF, SDMMC1_PINS,
    TRANSFER_HZ, clkcr, clkcr_word, cmdr, cmdr_word, data_timeout, dctrl, dctrl_word, divider,
    word_of_bytes,
};

#[test]
fn the_dividers_from_the_kernel_clock() {
    assert_eq!(divider(48_000_000, IDENTIFICATION_HZ), 61, "393.4 kHz");
    assert_eq!(divider(48_000_000, TRANSFER_HZ), 1, "24 MHz");
    assert_eq!(divider(200_000_000, IDENTIFICATION_HZ), 251);
    assert_eq!(divider(200_000_000, TRANSFER_HZ), 5, "20 MHz");
    assert_eq!(divider(1_000_000, TRANSFER_HZ), 1, "never a bypass");
    assert_eq!(
        divider(2_000_000_000, IDENTIFICATION_HZ),
        0x3FF,
        "the field's ceiling"
    );
}

#[test]
fn the_register_words() {
    assert_eq!(clkcr_word(61, Width::One), 61 | clkcr::HWFC_EN);
    assert_eq!(
        clkcr_word(1, Width::Four),
        1 | clkcr::WIDBUS_4 | clkcr::HWFC_EN
    );
    assert_eq!(
        cmdr_word(52, Response::Short, false),
        52 | (1 << 8) | (1 << 12)
    );
    assert_eq!(
        cmdr_word(5, Response::ShortWithoutCrc, false),
        5 | (2 << 8) | (1 << 12)
    );
    assert_eq!(
        cmdr_word(53, Response::Short, true),
        53 | (1 << 8) | cmdr::CMDTRANS | (1 << 12)
    );
    assert_eq!(cmdr_word(0, Response::None, false), 1 << 12);
    assert_eq!(
        dctrl_word(Direction::FromCard, 64),
        dctrl::DTDIR | (6 << 4) | dctrl::SDIOEN
    );
    assert_eq!(dctrl_word(Direction::ToCard, 4), (2 << 4) | dctrl::SDIOEN);
    assert_eq!(data_timeout(24_000_000), 6_000_000);
    assert_eq!(word_of_bytes([1, 2, 3, 4]), 0x0403_0201);
}

#[test]
fn the_pins_and_the_boards() {
    assert_eq!(
        SDMMC1_PINS,
        [
            Pin::new(2, 8),
            Pin::new(2, 9),
            Pin::new(2, 10),
            Pin::new(2, 11),
            Pin::new(2, 12),
            Pin::new(3, 2)
        ]
    );
    assert_eq!(SDMMC1_AF, 12);
    assert_eq!(Pin::new(1, 10).port_base(), 0x5802_0400);
    assert_eq!(Pin::new(9, 5).port_base(), 0x5802_2400);
    assert_eq!(Pin::new(2, 12).afr(), (0x24, 16));
    assert_eq!(Pin::new(3, 2).afr(), (0x20, 8));
    assert_eq!(
        GIGA_R1_WIFI,
        Board {
            power: Pin::new(1, 10),
            host_wake: Some(Pin::new(8, 8)),
            io_millivolts: 3300
        }
    );
    assert_eq!(
        PORTENTA_H7,
        Board {
            power: Pin::new(9, 1),
            host_wake: Some(Pin::new(9, 5)),
            io_millivolts: 3100
        }
    );
}
