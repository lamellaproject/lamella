//! Microsoft UF2: the format an RP2040 or RP2350 accepts in BOOTSEL mode.

use crate::EmitError;

/// Family id for the RP2040.
pub const FAMILY_RP2040: u32 = 0xe48b_ff56;

/// Family id for the RP2350 running Arm code in the secure state.
pub const FAMILY_RP2350_ARM_S: u32 = 0xe48b_ff59;

/// The default base address for an RP2040 / RP2350 image: the start of the XIP flash window,
/// where a flat `.bin` begins with its second-stage bootloader at offset zero.
pub const XIP_BASE: u32 = 0x1000_0000;

const MAGIC_START0: u32 = 0x0A32_4655;
const MAGIC_START1: u32 = 0x9E5D_5157;
const MAGIC_END: u32 = 0x0AB1_6F30;

/// Set when the family id field is meaningful rather than reserved.
const FLAG_FAMILY_ID: u32 = 0x0000_2000;

/// Payload bytes per block. Fixed by the format, and by the mask ROM's expectations -- see
/// [`to_uf2`].
const PAYLOAD: usize = 256;

/// Total block size, payload plus header plus trailing magic.
const BLOCK: usize = 512;

/// Pack `image` into a UF2 file to be flashed at `base` on the part identified by `family`.
///
/// For a Raspberry Pi part, pass [`XIP_BASE`] and one of [`FAMILY_RP2040`] or
/// [`FAMILY_RP2350_ARM_S`]. Returns [`EmitError::EmptyImage`] rather than a file of no blocks,
/// which the bootloader would accept and act on by rebooting into the firmware already present.
///
/// # The output is padded, and it has to be
///
/// The mask ROM requires every block to carry a full 256-byte page: it rejects a short final
/// block, and having rejected it never reaches "all blocks received", so it never reboots into
/// the new firmware. The board simply sits in BOOTSEL looking like the copy did not take. So an
/// image is zero-padded up to a 256-byte multiple and the extra bytes land in flash past the end
/// of the firmware, which is what `elf2uf2` and `picotool` do as well.
///
/// # Examples
///
/// ```
/// # use lamella_flash_format::uf2::{to_uf2, FAMILY_RP2040, XIP_BASE};
/// // One byte still produces one whole block: 512 bytes out, 256 of them payload.
/// let uf2 = to_uf2(&[0x42], XIP_BASE, FAMILY_RP2040).unwrap();
/// assert_eq!(uf2.len(), 512);
/// ```
pub fn to_uf2(image: &[u8], base: u32, family: u32) -> Result<Vec<u8>, EmitError> {
    if image.is_empty() {
        return Err(EmitError::EmptyImage);
    }
    let padded_len = image.len().div_ceil(PAYLOAD) * PAYLOAD;
    if base as u64 + padded_len as u64 > u64::from(u32::MAX) + 1 {
        return Err(EmitError::AddressOverflow {
            base,
            len: image.len(),
        });
    }

    let num_blocks = (padded_len / PAYLOAD) as u32;
    let mut out = Vec::with_capacity(num_blocks as usize * BLOCK);

    for block_no in 0..num_blocks as usize {
        let start = block_no * PAYLOAD;
        let chunk = &image[start..image.len().min(start + PAYLOAD)];
        let header = [
            MAGIC_START0,
            MAGIC_START1,
            FLAG_FAMILY_ID,
            base + (block_no * PAYLOAD) as u32,
            PAYLOAD as u32,
            block_no as u32,
            num_blocks,
            family,
        ];
        for word in header {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(chunk);
        out.resize(block_no * BLOCK + BLOCK - 4, 0);
        out.extend_from_slice(&MAGIC_END.to_le_bytes());
        debug_assert_eq!(out.len(), (block_no + 1) * BLOCK);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(block: &[u8], index: usize) -> u32 {
        u32::from_le_bytes(block[index * 4..index * 4 + 4].try_into().unwrap())
    }

    #[test]
    fn empty_image_is_refused() {
        assert_eq!(
            to_uf2(&[], XIP_BASE, FAMILY_RP2040),
            Err(EmitError::EmptyImage)
        );
    }

    #[test]
    fn a_short_final_block_is_padded_to_a_whole_page() {
        let uf2 = to_uf2(&[0xAA; PAYLOAD + 1], XIP_BASE, FAMILY_RP2040).unwrap();
        assert_eq!(uf2.len(), 2 * BLOCK);
        let last = &uf2[BLOCK..];
        assert_eq!(word(last, 4), PAYLOAD as u32, "payload size stays a full page");
        assert_eq!(last[32], 0xAA, "the one real byte");
        assert!(last[33..32 + PAYLOAD].iter().all(|&b| b == 0), "zero-filled");
    }

    #[test]
    fn every_block_is_framed_and_numbered() {
        let image: Vec<u8> = (0..PAYLOAD * 3 + 7).map(|i| (i % 253) as u8).collect();
        let uf2 = to_uf2(&image, XIP_BASE, FAMILY_RP2350_ARM_S).unwrap();
        assert_eq!(uf2.len(), 4 * BLOCK);
        for (i, block) in uf2.chunks(BLOCK).enumerate() {
            assert_eq!(word(block, 0), MAGIC_START0);
            assert_eq!(word(block, 1), MAGIC_START1);
            assert_eq!(word(block, 2), FLAG_FAMILY_ID);
            assert_eq!(word(block, 3), XIP_BASE + (i * PAYLOAD) as u32);
            assert_eq!(word(block, 5), i as u32);
            assert_eq!(word(block, 6), 4, "total block count is the same in every block");
            assert_eq!(word(block, 7), FAMILY_RP2350_ARM_S);
            assert_eq!(word(block, 127), MAGIC_END, "trailing magic at offset 508");
        }
    }

    #[test]
    fn the_payload_reassembles_into_the_original_image() {
        let image: Vec<u8> = (0..PAYLOAD * 2 + 100).map(|i| (i % 199) as u8).collect();
        let uf2 = to_uf2(&image, XIP_BASE, FAMILY_RP2040).unwrap();
        let recovered: Vec<u8> = uf2
            .chunks(BLOCK)
            .flat_map(|b| b[32..32 + PAYLOAD].iter().copied())
            .take(image.len())
            .collect();
        assert_eq!(recovered, image);
    }
}
