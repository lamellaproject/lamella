//! The SDIO command arguments, the response fields, the sizing rule, the
//! voltage windows and the CRC of the command line, on the values of the
//! recorded exchange and on published vectors.

use crate::error::Refusal;
use crate::sdio::{
    OcrWindow, STAGE_TRANSFER, Shape, cccr, cmd52_arg, cmd53_arg, command_frame, crc7, r4, r5, r6,
    wire_shape,
};
use crate::transport::Func;

#[test]
fn the_direct_command_words_of_the_attach() {
    assert_eq!(cmd52_arg(false, Func::F0, 0x07, 0), 0x0000_0E00);
    assert_eq!(cmd52_arg(true, Func::F0, 0x07, 0x02), 0x8000_0E02);
    assert_eq!(cmd52_arg(true, Func::F0, 0x10, 0x20), 0x8000_2020);
    assert_eq!(cmd52_arg(true, Func::F0, 0x110, 0x40), 0x8002_2040);
    assert_eq!(cmd52_arg(true, Func::F0, 0x211, 0x00), 0x8004_2200);
    assert_eq!(cmd52_arg(true, Func::F0, 0x04, 0x07), 0x8000_0807);
    assert_eq!(cmd52_arg(false, Func::F0, 0x03, 0), 0x0000_0600);
    assert_eq!(cmd52_arg(true, Func::F1, 0x1000F, 0), 0x9200_1E00);
    assert_eq!(cmd52_arg(true, Func::F1, 0x1000C, 0x18), 0x9200_1818);
    assert_eq!(cmd52_arg(false, Func::F1, 0x1000E, 0), 0x1200_1C00);
    assert_eq!(cmd52_arg(true, Func::F0, 0x06, 0x02), 0x8000_0C02);
}

#[test]
fn the_extended_command_words() {
    assert_eq!(
        cmd53_arg(false, Func::F1, 0x8000, true, Shape::bytes(4)),
        0x1500_0004
    );
    assert_eq!(
        cmd53_arg(false, Func::F2, 0, true, Shape::bytes(4)),
        0x2400_0004
    );
    assert_eq!(
        cmd53_arg(true, Func::F2, 0, true, Shape::blocks(2, 64)),
        0xAC00_0002
    );
    assert_eq!(
        cmd53_arg(true, Func::F1, 0x1000, true, Shape::blocks(32, 64)),
        0x9C20_0020
    );
    assert_eq!(
        cmd53_arg(false, Func::F1, 0, false, Shape::bytes(64)),
        0x1000_0040
    );
}

#[test]
fn the_response_fields_read() {
    assert_eq!(r4::functions(0x20FF_FF00), 2);
    assert_eq!(r4::ocr(0x20FF_FF00), 0x00FF_FF00);
    assert_eq!(0x20FF_FF00 & r4::READY, 0);
    assert_ne!(0xA0FF_FF00 & r4::READY, 0);
    assert_eq!(r5::data(0x0000_1069), 0x69);
    assert_eq!(r5::flags(0x0000_1069), 0x10);
    assert_ne!(0x0000_1220 & r5::ERRORS, 0);
    assert_eq!(0x0000_1000 & r5::ERRORS, 0);
    assert_eq!(0x0000_2000 & r5::ERRORS, 0);
    assert_eq!(r6::rca(0x0001_0000), 1);
    assert_ne!(0x0001_2000 & r6::ERRORS, 0);
}

#[test]
fn the_sizing_rule() {
    assert_eq!(wire_shape(Func::F1, 2), Ok(Shape::bytes(2)));
    assert_eq!(wire_shape(Func::F1, 3), Ok(Shape::bytes(4)));
    assert_eq!(wire_shape(Func::F1, 5), Ok(Shape::bytes(8)));
    assert_eq!(wire_shape(Func::F1, 33), Ok(Shape::bytes(64)));
    assert_eq!(wire_shape(Func::F1, 63), Ok(Shape::bytes(64)));
    assert_eq!(wire_shape(Func::F1, 64), Ok(Shape::blocks(1, 64)));
    assert_eq!(wire_shape(Func::F1, 65), Ok(Shape::blocks(2, 64)));
    assert_eq!(wire_shape(Func::F1, 100), Ok(Shape::blocks(2, 64)));
    assert_eq!(wire_shape(Func::F1, 2048), Ok(Shape::blocks(32, 64)));
    assert_eq!(
        wire_shape(Func::F1, 2049),
        Err(Refusal::new(STAGE_TRANSFER, 2049))
    );
    assert_eq!(wire_shape(Func::F0, 32), Ok(Shape::bytes(32)));
    assert_eq!(
        wire_shape(Func::F0, 33),
        Err(Refusal::new(STAGE_TRANSFER, 33))
    );
    assert_eq!(wire_shape(Func::F2, 2048), Ok(Shape::blocks(32, 64)));
    assert_eq!(
        wire_shape(Func::F2, 0),
        Err(Refusal::new(STAGE_TRANSFER, 0))
    );
    assert_eq!(Shape::blocks(2, 64).count(), 2);
    assert_eq!(Shape::bytes(8).count(), 1);
    assert_eq!(Shape::bytes(8).block_len(), 8);
    assert_eq!(Shape::blocks(2, 64).block_len(), 64);
}

#[test]
fn the_block_size_registers() {
    assert_eq!(cccr::block_size(Func::F0), 0x10);
    assert_eq!(cccr::block_size(Func::F1), 0x110);
    assert_eq!(cccr::block_size(Func::F2), 0x210);
}

#[test]
fn the_voltage_windows() {
    assert_eq!(OcrWindow::V3_3.bits(), 0x0030_0000);
    assert_eq!(OcrWindow::V3_1.bits(), 0x000C_0000);
    assert_eq!(OcrWindow::for_millivolts(3300), Some(OcrWindow::V3_3));
    assert_eq!(OcrWindow::for_millivolts(3100), Some(OcrWindow::V3_1));
    assert_eq!(
        OcrWindow::for_millivolts(3250).map(OcrWindow::bits),
        Some(1 << 20)
    );
    assert_eq!(
        OcrWindow::for_millivolts(2700).map(OcrWindow::bits),
        Some(1 << 15)
    );
    assert_eq!(
        OcrWindow::for_millivolts(3600).map(OcrWindow::bits),
        Some(1 << 23)
    );
    assert_eq!(OcrWindow::for_millivolts(2600), None);
    assert_eq!(OcrWindow::for_millivolts(3700), None);
}

#[test]
fn the_crc7_of_the_command_line_on_published_vectors() {
    assert_eq!(crc7(&[0x40, 0, 0, 0, 0]), 0x4A);
    assert_eq!(crc7(&[0x48, 0, 0, 0x01, 0xAA]), 0x43);
    assert_eq!(command_frame(0, 0), [0x40, 0, 0, 0, 0, 0x95]);
    assert_eq!(command_frame(8, 0x1AA), [0x48, 0, 0, 0x01, 0xAA, 0x87]);
}
