//! Intel HEX: the format a DAPLink mass-storage volume accepts.

use crate::EmitError;

/// Payload bytes per data record.
///
/// Sixteen is what essentially every toolchain emits, and some bootloaders have been observed to
/// assume it, so this is not worth making configurable.
const RECORD_LEN: usize = 16;

/// One 64 KB span -- the range a single extended linear address record covers.
const SPAN: usize = 0x1_0000;

const TYPE_DATA: u8 = 0x00;
const TYPE_EOF: u8 = 0x01;
const TYPE_EXTENDED_LINEAR_ADDRESS: u8 = 0x04;

/// Render `image` as Intel HEX, to be loaded at `base`.
///
/// Returns [`EmitError::EmptyImage`] rather than a file containing only an end-of-file record;
/// the crate documentation explains why that case is worth refusing.
///
/// # Images larger than 64 KB
///
/// A data record addresses only 16 bits, so a fresh extended linear address record is emitted at
/// every 64 KB boundary and no record is allowed to straddle one. This is the part that fails
/// quietly if skipped: with a single leading address record the arithmetic simply wraps, and the
/// tail of the image is written over its own head. Nothing reports an error -- the file is well
/// formed, the checksums are correct, and the board is flashed with a corrupt image. A 512 KB
/// part such as the nRF52833 on a micro:bit v2 reaches this comfortably.
///
/// # Examples
///
/// ```
/// # use lamella_flash_format::hex::to_intel_hex;
/// let hex = to_intel_hex(&[0xDE, 0xAD], 0).unwrap();
/// assert_eq!(hex, ":020000040000FA\n:02000000DEAD73\n:00000001FF\n");
/// ```
pub fn to_intel_hex(image: &[u8], base: u32) -> Result<String, EmitError> {
    if image.is_empty() {
        return Err(EmitError::EmptyImage);
    }
    if base as u64 + image.len() as u64 > u64::from(u32::MAX) + 1 {
        return Err(EmitError::AddressOverflow {
            base,
            len: image.len(),
        });
    }

    let mut out = String::with_capacity(image.len() / RECORD_LEN * (RECORD_LEN * 2 + 12) + 32);
    let mut written_upper: Option<u16> = None;
    let mut pos = 0usize;

    while pos < image.len() {
        let addr = base + pos as u32;
        let upper = (addr >> 16) as u16;
        if written_upper != Some(upper) {
            push_record(&mut out, 0, TYPE_EXTENDED_LINEAR_ADDRESS, &upper.to_be_bytes());
            written_upper = Some(upper);
        }
        let to_boundary = SPAN - (addr as usize & (SPAN - 1));
        let n = RECORD_LEN.min(image.len() - pos).min(to_boundary);
        push_record(&mut out, addr as u16, TYPE_DATA, &image[pos..pos + n]);
        pos += n;
    }

    push_record(&mut out, 0, TYPE_EOF, &[]);
    Ok(out)
}

/// Append one record. The checksum is the two's complement of the sum of every byte in the
/// record except the checksum itself.
fn push_record(out: &mut String, addr: u16, kind: u8, data: &[u8]) {
    use core::fmt::Write;

    let len = data.len() as u8;
    let mut sum = len
        .wrapping_add((addr >> 8) as u8)
        .wrapping_add(addr as u8)
        .wrapping_add(kind);
    let _ = write!(out, ":{len:02X}{addr:04X}{kind:02X}");
    for &b in data {
        let _ = write!(out, "{b:02X}");
        sum = sum.wrapping_add(b);
    }
    let _ = writeln!(out, "{:02X}", (!sum).wrapping_add(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a hex file back into (address, byte) pairs, honoring extended linear address records.
    /// Deliberately a separate implementation from the writer: a round-trip against a shared
    /// helper would agree with itself no matter what either of them did.
    fn parse(hex: &str) -> Vec<(u32, u8)> {
        let mut upper = 0u32;
        let mut out = Vec::new();
        for line in hex.lines() {
            let body = line.strip_prefix(':').expect("record starts with a colon");
            let bytes: Vec<u8> = (0..body.len() / 2)
                .map(|i| u8::from_str_radix(&body[i * 2..i * 2 + 2], 16).expect("hex digits"))
                .collect();
            let sum = bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b));
            assert_eq!(sum, 0, "checksum of {line}");
            let len = bytes[0] as usize;
            let addr = u32::from(bytes[1]) << 8 | u32::from(bytes[2]);
            let data = &bytes[4..4 + len];
            match bytes[3] {
                TYPE_DATA => out.extend(
                    data.iter()
                        .enumerate()
                        .map(|(i, &b)| (upper | (addr + i as u32), b)),
                ),
                TYPE_EOF => break,
                TYPE_EXTENDED_LINEAR_ADDRESS => {
                    upper = (u32::from(data[0]) << 8 | u32::from(data[1])) << 16
                }
                other => panic!("unexpected record type {other:#04x}"),
            }
        }
        out
    }

    #[test]
    fn empty_image_is_refused() {
        assert_eq!(to_intel_hex(&[], 0), Err(EmitError::EmptyImage));
    }

    #[test]
    fn matches_the_conventional_leading_record() {
        assert!(to_intel_hex(&[0u8; 32], 0).unwrap().starts_with(":020000040000FA\n"));
    }

    #[test]
    fn round_trips_at_every_offset_in_a_record() {
        for len in [1usize, 15, 16, 17, 31, 32, 33] {
            let image: Vec<u8> = (0..len).map(|i| (i * 7 + 1) as u8).collect();
            let parsed = parse(&to_intel_hex(&image, 0).unwrap());
            let expected: Vec<(u32, u8)> =
                image.iter().enumerate().map(|(i, &b)| (i as u32, b)).collect();
            assert_eq!(parsed, expected, "at length {len}");
        }
    }

    #[test]
    fn crossing_64k_addresses_the_upper_span() {
        let image: Vec<u8> = (0..0x1_8000usize).map(|i| (i % 251) as u8).collect();
        let parsed = parse(&to_intel_hex(&image, 0).unwrap());
        assert_eq!(parsed.len(), image.len());
        for (i, (addr, b)) in parsed.iter().enumerate() {
            assert_eq!((*addr, *b), (i as u32, image[i]), "byte {i}");
        }
        assert!(parsed.iter().any(|(a, _)| *a >= 0x1_0000), "reached the upper span");
    }

    #[test]
    fn no_record_straddles_a_span_boundary() {
        let image = vec![0xA5u8; 64];
        let hex = to_intel_hex(&image, 0xFFF8).unwrap();
        for line in hex.lines().filter(|l| &l[7..9] == "00") {
            let len = u8::from_str_radix(&line[1..3], 16).unwrap() as u32;
            let addr = u32::from_str_radix(&line[3..7], 16).unwrap();
            assert!(addr + len <= 0x1_0000, "record {line} runs past the span");
        }
        let parsed = parse(&hex);
        assert_eq!(parsed.len(), image.len());
        assert_eq!(parsed[0].0, 0xFFF8);
        assert_eq!(parsed.last().unwrap().0, 0xFFF8 + 63);
    }

    #[test]
    fn a_high_base_emits_its_own_address_record() {
        let hex = to_intel_hex(&[0x11, 0x22], 0x0800_0000).unwrap();
        assert!(hex.starts_with(":020000040800"), "got {hex}");
        assert_eq!(parse(&hex), vec![(0x0800_0000, 0x11), (0x0800_0001, 0x22)]);
    }

    #[test]
    fn an_image_past_the_address_space_is_refused() {
        assert_eq!(
            to_intel_hex(&[0u8; 16], u32::MAX - 8),
            Err(EmitError::AddressOverflow {
                base: u32::MAX - 8,
                len: 16
            })
        );
    }
}
