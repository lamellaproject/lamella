//! Microsoft UF2: the format an RP2040 or RP2350 accepts in BOOTSEL mode.
//!
//! [`to_uf2`] packs a flat image into the blocks a bootloader takes from its drive, and
//! [`image_for_family`] reads such a file back into the image it holds for one chip family, for a
//! writer that puts the image on the chip itself, over a debug probe.

use core::fmt;

use crate::EmitError;

/// Family id for the RP2040.
pub const FAMILY_RP2040: u32 = 0xe48b_ff56;

/// Family id for the RP2350 running Arm code in the secure state.
pub const FAMILY_RP2350_ARM_S: u32 = 0xe48b_ff59;

/// The default base address for an RP2040 / RP2350 image: the start of the XIP flash window,
/// where a flat `.bin` begins with its second-stage bootloader at offset zero.
pub const XIP_BASE: u32 = 0x1000_0000;

/// The first word of every UF2 block, which is how a reader recognizes one.
///
/// **PUBLIC BECAUSE "IS THIS ALREADY A UF2" IS A QUESTION A WRITER HAS TO ASK.** An image reaches a
/// bootloader volume either already wrapped or as a flat binary that must be wrapped, and wrapping
/// a UF2 a second time produces a file the bootloader silently ignores.
pub const MAGIC_START0: u32 = 0x0A32_4655;
const MAGIC_START1: u32 = 0x9E5D_5157;
const MAGIC_END: u32 = 0x0AB1_6F30;

/// Set when the family id field is meaningful rather than reserved.
const FLAG_FAMILY_ID: u32 = 0x0000_2000;

/// Set on a block whose payload is not for the device's flash: a comment the file carries, such
/// as the program's source.
const FLAG_NOT_MAIN_FLASH: u32 = 0x0000_0001;

/// Set on a block that carries part of a file for a file system on the device, at an offset in
/// that file, rather than bytes at a flash address.
const FLAG_FILE_CONTAINER: u32 = 0x0000_1000;

/// Payload bytes per block. Fixed by the format, and by the mask ROM's expectations -- see
/// [`to_uf2`].
const PAYLOAD: usize = 256;

/// The most payload one block can carry: every byte between its 32-byte header and its final
/// magic.
const MAX_PAYLOAD: usize = 476;

/// Where a block's payload starts: just past its header.
const HEADER: usize = 32;

/// Total block size, payload plus header plus trailing magic.
const BLOCK: usize = 512;

/// What erased NOR flash reads as, and so what a gap between two blocks' payloads holds once an
/// image is written.
const ERASED: u8 = 0xFF;

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

/// The image a UF2 file holds for one chip family, checked and not yet laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyImage<'a> {
    /// Each payload for the device's flash with the address it starts at, in order of address.
    /// Never empty.
    pieces: Vec<(u32, &'a [u8])>,
    /// One past the highest address any payload covers.
    end: u64,
}

impl FamilyImage<'_> {
    /// The lowest address any of the family's blocks writes.
    #[must_use]
    pub fn base(&self) -> u32 {
        self.pieces[0].0
    }

    /// How many bytes the image spans: from [`base`](Self::base) to one past the highest byte any
    /// of the family's blocks writes.
    #[must_use]
    pub fn span(&self) -> u64 {
        self.end - u64::from(self.base())
    }

    /// The image laid out flat from [`base`](Self::base), with erased flash (`0xFF`) wherever no
    /// block writes.
    ///
    /// # Memory
    ///
    /// The image takes [`span`](Self::span) bytes, which blocks that lie far apart make large: up
    /// to 4 GiB. A caller that knows the chip's flash size refuses a span longer than it before
    /// calling this.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let base = u64::from(self.base());
        let at = |address: u32| usize::try_from(u64::from(address) - base).unwrap_or(usize::MAX);
        let mut bytes = vec![ERASED; usize::try_from(self.span()).unwrap_or(usize::MAX)];
        for &(address, payload) in &self.pieces {
            let start = at(address);
            bytes[start..start + payload.len()].copy_from_slice(payload);
        }
        bytes
    }
}

/// Why a UF2 file holds no flash image for a chip family.
///
/// A block is named by its byte offset in the file, which is where a hex editor shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The file is empty, or its length is not a whole number of 512-byte blocks.
    NotWholeBlocks {
        /// The file's length, in bytes.
        len: usize,
    },
    /// A block lacks one of the three magic numbers every UF2 block carries.
    NotABlock {
        /// Where the block starts in the file.
        offset: usize,
    },
    /// A block states more payload than a block can carry.
    PayloadTooLong {
        /// Where the block starts in the file.
        offset: usize,
        /// The payload size the block states, in bytes.
        size: u32,
    },
    /// A block carries part of a file for a file system on the device, not bytes for its flash.
    FileContainer {
        /// Where the block starts in the file.
        offset: usize,
    },
    /// A block for the device's flash names no chip family, so it cannot be checked against one.
    NoFamily {
        /// Where the block starts in the file.
        offset: usize,
    },
    /// No block in the file is for the family asked for.
    NoBlockForFamily {
        /// The family asked for.
        family: u32,
        /// The families the file's blocks name instead, each once, in the order they first
        /// appear.
        found: Vec<u32>,
    },
    /// The family's blocks disagree about how many of them there are.
    CountDisagrees {
        /// Where the first block that disagrees with the family's first block starts in the file.
        offset: usize,
        /// The count that block states.
        stated: u32,
        /// The count the family's first block states.
        first: u32,
    },
    /// A block's number is not below the count the family's blocks state.
    NumberOutOfRange {
        /// Where the block starts in the file.
        offset: usize,
        /// The block's number.
        number: u32,
        /// The count the family's blocks state.
        count: u32,
    },
    /// Two of the family's blocks carry the same number.
    NumberRepeated {
        /// The number both carry.
        number: u32,
        /// Where the first of the two starts in the file.
        first: usize,
        /// Where the second of the two starts in the file.
        second: usize,
    },
    /// The file holds fewer of the family's blocks than the count they state.
    BlocksMissing {
        /// How many of the family's blocks the file holds.
        present: usize,
        /// The count they state.
        count: u32,
    },
    /// A block's payload runs past the end of the 32-bit address space.
    AddressOverflow {
        /// Where the block starts in the file.
        offset: usize,
    },
    /// Two of the family's blocks put different bytes at one address.
    Overlap {
        /// The first address the two disagree about.
        address: u32,
        /// Where the first of the two starts in the file.
        first: usize,
        /// Where the second of the two starts in the file.
        second: usize,
    },
    /// The family's blocks carry no byte for the device's flash.
    NothingToWrite,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotWholeBlocks { len: 0 } => {
                f.write_str("the file is empty, so it holds no UF2 block")
            }
            Self::NotWholeBlocks { len } => write!(
                f,
                "the file is {len} B, which is not a whole number of {BLOCK}-byte UF2 blocks, so \
                 it was cut short or is not a UF2"
            ),
            Self::NotABlock { offset } => write!(
                f,
                "the block at byte {offset} lacks the UF2 magic numbers, so the file is damaged or \
                 is not a UF2"
            ),
            Self::PayloadTooLong { offset, size } => write!(
                f,
                "the block at byte {offset} states {size} B of payload, and a UF2 block carries at \
                 most {MAX_PAYLOAD}"
            ),
            Self::FileContainer { offset } => write!(
                f,
                "the block at byte {offset} carries part of a file for a file system on the \
                 device, so this UF2 holds files rather than a flash image"
            ),
            Self::NoFamily { offset } => write!(
                f,
                "the block at byte {offset} names no chip family, so nothing can check that it is \
                 for this chip"
            ),
            Self::NoBlockForFamily { found, .. } if found.is_empty() => {
                f.write_str("no block in the file is for the device's flash")
            }
            Self::NoBlockForFamily { family, found } => {
                let named: Vec<String> = found.iter().map(|&other| described(other)).collect();
                write!(
                    f,
                    "no block in the file is for chip family {}: its blocks are for {}",
                    described(*family),
                    named.join(" and ")
                )
            }
            Self::CountDisagrees { offset, stated, first } => write!(
                f,
                "the family's first block says there are {first} of its blocks, and the block at \
                 byte {offset} says {stated}, so the file holds two images or is damaged"
            ),
            Self::NumberOutOfRange { offset, number, count } => write!(
                f,
                "the block at byte {offset} is numbered {number}, and the family's blocks say \
                 there are {count}, numbered from 0, so the file is damaged"
            ),
            Self::NumberRepeated { number, first, second } => write!(
                f,
                "the blocks at bytes {first} and {second} are both numbered {number}, so the file \
                 holds parts of two images or is damaged"
            ),
            Self::BlocksMissing { present, count } => write!(
                f,
                "the family's blocks say there are {count}, and the file holds {present} of them, \
                 so it is incomplete -- most often a download that stopped early"
            ),
            Self::AddressOverflow { offset } => write!(
                f,
                "the block at byte {offset} runs past the end of the 32-bit address space"
            ),
            Self::Overlap { address, first, second } => write!(
                f,
                "the blocks at bytes {first} and {second} put different bytes at {address:#010x}"
            ),
            Self::NothingToWrite => {
                f.write_str("the family's blocks carry no byte for the device's flash")
            }
        }
    }
}

impl std::error::Error for ReadError {}

/// A family id as a reader can recognize it: the number, and for the families of the chips this
/// crate writes, the name the family list gives it.
fn described(family: u32) -> String {
    let name = match family {
        FAMILY_RP2040 => "RP2040",
        0xe48b_ff57 => "an absolute (unpartitioned) download",
        0xe48b_ff58 => "a data partition download",
        FAMILY_RP2350_ARM_S => "RP2350, a Secure Arm image",
        0xe48b_ff5a => "RP2350, a RISC-V image",
        0xe48b_ff5b => "RP2350, a Non-secure Arm image",
        _ => return format!("{family:#010x}"),
    };
    format!("{family:#010x} ({name})")
}

/// Read `file` as a UF2 and return the image it holds for the chip family `family`: the payload of
/// each of that family's blocks for flash, at the address the block states.
///
/// The file is read as the format's own description says a bootloader reads it:
///
/// - every block carries the three magic numbers and at most 476 bytes of payload;
/// - a block marked as not for main flash is a comment, such as the program's source, and is
///   skipped;
/// - a block of another family is skipped, as a bootloader for this family disregards it, so one
///   file can carry images for several chips;
/// - the family's blocks are numbered from 0 to one less than the count each of them states, each
///   number once, so a file that stops early is refused rather than written in part.
///
/// Two of the family's blocks may cover the same bytes only with the same values, and the image
/// has erased flash wherever none of them writes.
///
/// # Errors
///
/// Every [`ReadError`]: a file that is not whole blocks, a block that is not one or carries too
/// much, a file of files rather than a flash image, a block for flash that names no family, no
/// block for `family`, blocks numbered wrongly or missing, a payload past the end of the address
/// space, two blocks that disagree, and a family whose blocks carry no byte for flash.
///
/// # Examples
///
/// ```
/// # use lamella_flash_format::uf2::{image_for_family, to_uf2, FAMILY_RP2350_ARM_S, XIP_BASE};
/// let file = to_uf2(&[0x42; 300], XIP_BASE, FAMILY_RP2350_ARM_S).unwrap();
/// let image = image_for_family(&file, FAMILY_RP2350_ARM_S).unwrap();
/// assert_eq!(image.base(), XIP_BASE);
/// // Two whole pages: the writer padded the last one, and the padding is part of the file.
/// assert_eq!(image.span(), 512);
/// ```
pub fn image_for_family(file: &[u8], family: u32) -> Result<FamilyImage<'_>, ReadError> {
    if file.is_empty() || !file.len().is_multiple_of(BLOCK) {
        return Err(ReadError::NotWholeBlocks { len: file.len() });
    }
    let mut found = Vec::new();
    let mut first_count = None;
    let mut numbered = Vec::new();
    let mut pieces = Vec::new();
    for (index, block) in file.chunks_exact(BLOCK).enumerate() {
        let offset = index * BLOCK;
        let word = |at: usize| {
            u32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]])
        };
        if word(0) != MAGIC_START0 || word(4) != MAGIC_START1 || word(BLOCK - 4) != MAGIC_END {
            return Err(ReadError::NotABlock { offset });
        }
        let (flags, address, size) = (word(8), word(12), word(16));
        let length = usize::try_from(size).unwrap_or(usize::MAX);
        if length > MAX_PAYLOAD {
            return Err(ReadError::PayloadTooLong { offset, size });
        }
        if flags & FLAG_FILE_CONTAINER != 0 {
            return Err(ReadError::FileContainer { offset });
        }
        let comment = flags & FLAG_NOT_MAIN_FLASH != 0;
        if flags & FLAG_FAMILY_ID == 0 {
            if comment {
                continue;
            }
            return Err(ReadError::NoFamily { offset });
        }
        let named = word(28);
        if named != family {
            if !found.contains(&named) {
                found.push(named);
            }
            continue;
        }
        let (number, count) = (word(20), word(24));
        match first_count {
            None => first_count = Some(count),
            Some(first) if first != count => {
                return Err(ReadError::CountDisagrees { offset, stated: count, first });
            }
            Some(_) => {}
        }
        if number >= count {
            return Err(ReadError::NumberOutOfRange { offset, number, count });
        }
        numbered.push((number, offset));
        if comment || length == 0 {
            continue;
        }
        if u64::from(address) + u64::from(size) > 1 << 32 {
            return Err(ReadError::AddressOverflow { offset });
        }
        pieces.push((address, &block[HEADER..HEADER + length], offset));
    }
    let Some(count) = first_count else {
        return Err(ReadError::NoBlockForFamily { family, found });
    };
    numbered.sort_unstable();
    if let Some(pair) = numbered.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(ReadError::NumberRepeated {
            number: pair[0].0,
            first: pair[0].1,
            second: pair[1].1,
        });
    }
    if usize::try_from(count).ok() != Some(numbered.len()) {
        return Err(ReadError::BlocksMissing { present: numbered.len(), count });
    }
    if pieces.is_empty() {
        return Err(ReadError::NothingToWrite);
    }
    pieces.sort_by_key(|&(address, ..)| address);
    refuse_disagreeing_overlaps(&pieces)?;
    let end = pieces
        .iter()
        .map(|&(address, payload, _)| u64::from(address) + payload.len() as u64)
        .max()
        .unwrap_or(0);
    Ok(FamilyImage {
        pieces: pieces.into_iter().map(|(address, payload, _)| (address, payload)).collect(),
        end,
    })
}

/// Refuse two payloads in `pieces`, which are in order of address, that put different bytes at
/// one address. Each is `(address, payload, where its block starts in the file)`.
///
fn refuse_disagreeing_overlaps(pieces: &[(u32, &[u8], usize)]) -> Result<(), ReadError> {
    for (later, &(address, payload, offset)) in pieces.iter().enumerate() {
        let start = u64::from(address);
        for &(earlier_address, earlier, earlier_offset) in pieces[..later].iter().rev() {
            let earlier_start = u64::from(earlier_address);
            if earlier_start + MAX_PAYLOAD as u64 <= start {
                break;
            }
            let earlier_end = earlier_start + earlier.len() as u64;
            let shared_end = earlier_end.min(start + payload.len() as u64);
            if shared_end <= start {
                continue;
            }
            let skip = usize::try_from(start - earlier_start).unwrap_or(usize::MAX);
            let shared = usize::try_from(shared_end - start).unwrap_or(usize::MAX);
            let differs = (0..shared).find(|&at| earlier[skip + at] != payload[at]);
            if let Some(at) = differs {
                return Err(ReadError::Overlap {
                    address: address + u32::try_from(at).unwrap_or(u32::MAX),
                    first: earlier_offset.min(offset),
                    second: earlier_offset.max(offset),
                });
            }
        }
    }
    Ok(())
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

    /// The family list's absolute download, which the RP2350's documented workaround for its
    /// erratum E10 puts in front of an image.
    const FAMILY_ABSOLUTE: u32 = 0xe48b_ff57;

    /// One block, built field by field, for the shapes `to_uf2` never writes.
    fn block(
        flags: u32,
        address: u32,
        payload: &[u8],
        number: u32,
        count: u32,
        family: u32,
    ) -> Vec<u8> {
        let size = u32::try_from(payload.len()).unwrap();
        let mut out = Vec::with_capacity(BLOCK);
        for field in [MAGIC_START0, MAGIC_START1, flags, address, size, number, count, family] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        out.extend_from_slice(payload);
        out.resize(BLOCK - 4, 0);
        out.extend_from_slice(&MAGIC_END.to_le_bytes());
        out
    }

    /// A flash block of the RP2350's family, numbered `number` of `count`.
    fn flash(address: u32, payload: &[u8], number: u32, count: u32) -> Vec<u8> {
        block(FLAG_FAMILY_ID, address, payload, number, count, FAMILY_RP2350_ARM_S)
    }

    fn read(file: &[u8]) -> Result<Vec<u8>, ReadError> {
        image_for_family(file, FAMILY_RP2350_ARM_S).map(|image| image.to_bytes())
    }

    /// A file `to_uf2` wrote reads back as its image, padded to whole pages as the file carries it,
    /// from the base it was written for: whole pages of one family, numbered from 0 and
    /// contiguous, which is also the shape of every published interpreter firmware.
    #[test]
    fn an_image_reads_back_from_its_uf2_padded_to_whole_pages() {
        let image: Vec<u8> = (0..PAYLOAD * 2 + 188).map(|i| (i % 251) as u8).collect();
        let file = to_uf2(&image, XIP_BASE, FAMILY_RP2350_ARM_S).unwrap();
        let read = image_for_family(&file, FAMILY_RP2350_ARM_S).expect("its own family");
        assert_eq!(read.base(), XIP_BASE);
        assert_eq!(read.span(), 3 * PAYLOAD as u64);
        let bytes = read.to_bytes();
        assert_eq!(&bytes[..image.len()], &image[..]);
        assert!(bytes[image.len()..].iter().all(|&byte| byte == 0), "the writer's padding");
    }

    /// **A BLOCK OF ANOTHER FAMILY IS SKIPPED**, as the bootloader for this family disregards it,
    /// so one file carries an image for each chip and each chip's image reads out whole.
    #[test]
    fn two_chips_images_in_one_file_each_read_out_for_their_own_family() {
        let pico = vec![0x11; PAYLOAD + 1];
        let pico2 = vec![0x22; 3 * PAYLOAD];
        let mut file = to_uf2(&pico, XIP_BASE, FAMILY_RP2040).unwrap();
        file.extend(to_uf2(&pico2, XIP_BASE, FAMILY_RP2350_ARM_S).unwrap());
        assert_eq!(read(&file).unwrap(), pico2);
        let for_pico = image_for_family(&file, FAMILY_RP2040).unwrap().to_bytes();
        assert_eq!(&for_pico[..pico.len()], &pico[..]);
        assert_eq!(for_pico.len(), 2 * PAYLOAD);
    }

    /// The erratum E10 workaround: one block of the absolute family at the end of flash, numbered
    /// 0 of 2, in front of the image. It is another family's, so it is skipped, and the image's
    /// own blocks still number from 0.
    #[test]
    fn the_workaround_block_in_front_of_an_rp2350_image_is_skipped() {
        let image = vec![0x5A; 2 * PAYLOAD];
        let mut file = block(FLAG_FAMILY_ID, 0x10FF_FF00, &[0xEF; PAYLOAD], 0, 2, FAMILY_ABSOLUTE);
        file.extend(to_uf2(&image, XIP_BASE, FAMILY_RP2350_ARM_S).unwrap());
        let read = image_for_family(&file, FAMILY_RP2350_ARM_S).expect("the image is whole");
        assert_eq!((read.base(), read.to_bytes()), (XIP_BASE, image));
    }

    /// A file with no block for the family is refused naming the family asked for and each family
    /// it does carry, once.
    #[test]
    fn a_file_for_another_chip_is_refused_naming_both_families() {
        let mut file = to_uf2(&[0x11; 2 * PAYLOAD], XIP_BASE, FAMILY_RP2040).unwrap();
        file.extend(block(FLAG_FAMILY_ID, 0x10FF_FF00, &[0xEF; PAYLOAD], 0, 2, FAMILY_ABSOLUTE));
        let error = read(&file).unwrap_err();
        assert_eq!(
            error,
            ReadError::NoBlockForFamily {
                family: FAMILY_RP2350_ARM_S,
                found: vec![FAMILY_RP2040, FAMILY_ABSOLUTE]
            }
        );
        let said = error.to_string();
        assert!(said.contains("0xe48bff59 (RP2350, a Secure Arm image)"), "{said}");
        assert!(said.contains("0xe48bff56 (RP2040) and 0xe48bff57 ("), "{said}");
    }

    /// A comment block -- one not for main flash -- is never written, even at an address the
    /// image does not reach, and it still counts among its family's numbered blocks. Without a
    /// family it is skipped, while a block for flash without one is refused.
    #[test]
    fn a_comment_block_is_skipped_and_a_flash_block_must_name_its_family() {
        let comment = FLAG_NOT_MAIN_FLASH | FLAG_FAMILY_ID;
        let mut file = flash(XIP_BASE, &[0x01; PAYLOAD], 0, 3);
        let source = b"the program's source";
        file.extend(block(comment, 0x2000_0000, source, 1, 3, FAMILY_RP2350_ARM_S));
        file.extend(flash(XIP_BASE + 0x100, &[0x02; PAYLOAD], 2, 3));
        let mut image = vec![0x01; PAYLOAD];
        image.extend([0x02; PAYLOAD]);
        assert_eq!(read(&file).unwrap(), image, "the comment is neither written nor spanned");

        let mut unnamed = block(FLAG_NOT_MAIN_FLASH, 0x2000_0000, b"source", 0, 1, 0);
        unnamed.extend(flash(XIP_BASE, &[0x01; PAYLOAD], 0, 1));
        assert_eq!(read(&unnamed).unwrap(), vec![0x01; PAYLOAD]);

        let mut no_family = flash(XIP_BASE, &[0x01; PAYLOAD], 0, 2);
        no_family.extend(block(0, XIP_BASE + 0x100, &[0x02; PAYLOAD], 1, 2, 0));
        assert_eq!(read(&no_family), Err(ReadError::NoFamily { offset: BLOCK }));
    }

    /// Between two blocks that do not meet, the image holds erased flash.
    #[test]
    fn a_gap_between_blocks_reads_as_erased_flash() {
        let mut file = flash(XIP_BASE + 0x1000, &[0xB2; PAYLOAD], 1, 2);
        file.extend(flash(XIP_BASE, &[0xA1; PAYLOAD], 0, 2));
        let image = image_for_family(&file, FAMILY_RP2350_ARM_S).unwrap();
        assert_eq!((image.base(), image.span()), (XIP_BASE, 0x1100), "in order of address");
        let bytes = image.to_bytes();
        assert!(bytes[..PAYLOAD].iter().all(|&byte| byte == 0xA1));
        assert!(bytes[PAYLOAD..0x1000].iter().all(|&byte| byte == ERASED));
        assert!(bytes[0x1000..].iter().all(|&byte| byte == 0xB2));
    }

    /// Each of the three magic numbers is checked, and a file of anything but whole blocks is
    /// refused.
    #[test]
    fn a_block_without_its_magic_and_a_file_of_part_blocks_are_refused() {
        let file = to_uf2(&[0x33; 2 * PAYLOAD], XIP_BASE, FAMILY_RP2350_ARM_S).unwrap();
        for at in [0, 4, BLOCK - 4] {
            let mut broken = file.clone();
            broken[BLOCK + at] ^= 0x01;
            assert_eq!(read(&broken), Err(ReadError::NotABlock { offset: BLOCK }), "word {at}");
        }
        assert_eq!(read(&[]), Err(ReadError::NotWholeBlocks { len: 0 }));
        let cut = &file[..file.len() - 1];
        assert_eq!(read(cut), Err(ReadError::NotWholeBlocks { len: 2 * BLOCK - 1 }));
    }

    /// A payload past 476 B is more than a block holds, and a file container holds files for a
    /// file system, not an image: both are refused at their block.
    #[test]
    fn an_oversized_payload_and_a_file_container_are_refused() {
        let mut oversized = flash(XIP_BASE, &[0x44; PAYLOAD], 0, 1);
        oversized[16..20].copy_from_slice(&477u32.to_le_bytes());
        assert_eq!(read(&oversized), Err(ReadError::PayloadTooLong { offset: 0, size: 477 }));
        let full = flash(XIP_BASE, &[0x44; MAX_PAYLOAD], 0, 1);
        assert_eq!(read(&full).unwrap().len(), MAX_PAYLOAD, "476 B is a whole block's payload");

        let container = FLAG_FILE_CONTAINER | FLAG_FAMILY_ID;
        let mut file = flash(XIP_BASE, &[0x44; PAYLOAD], 0, 2);
        file.extend(block(container, 0, b"main.py", 1, 2, FAMILY_RP2350_ARM_S));
        assert_eq!(read(&file), Err(ReadError::FileContainer { offset: BLOCK }));
    }

    /// **A FILE THAT STOPS EARLY IS REFUSED, NOT WRITTEN IN PART.** Cut at a block boundary, it is
    /// still whole blocks, so only the count the blocks state shows that some are gone.
    #[test]
    fn a_file_that_stops_at_a_block_boundary_is_refused_as_incomplete() {
        let file = to_uf2(&[0x55; 3 * PAYLOAD], XIP_BASE, FAMILY_RP2350_ARM_S).unwrap();
        let cut = &file[..2 * BLOCK];
        assert_eq!(read(cut), Err(ReadError::BlocksMissing { present: 2, count: 3 }));
        let said = ReadError::BlocksMissing { present: 2, count: 3 }.to_string();
        assert!(said.contains("incomplete"), "{said}");
    }

    /// A number used twice, a number at or past the count, and two counts are each refused: a
    /// file like that holds parts of two images, or is damaged.
    #[test]
    fn blocks_numbered_twice_past_the_count_or_by_two_counts_are_refused() {
        let page = [0x66; PAYLOAD];
        let mut twice = flash(XIP_BASE, &page, 0, 2);
        twice.extend(flash(XIP_BASE + 0x100, &page, 0, 2));
        let repeated = ReadError::NumberRepeated { number: 0, first: 0, second: BLOCK };
        assert_eq!(read(&twice), Err(repeated));

        let mut past = flash(XIP_BASE, &page, 0, 2);
        past.extend(flash(XIP_BASE + 0x100, &page, 2, 2));
        let out_of_range = ReadError::NumberOutOfRange { offset: BLOCK, number: 2, count: 2 };
        assert_eq!(read(&past), Err(out_of_range));

        let mut two_counts = flash(XIP_BASE, &page, 0, 2);
        two_counts.extend(flash(XIP_BASE + 0x100, &page, 1, 3));
        let disagree = ReadError::CountDisagrees { offset: BLOCK, stated: 3, first: 2 };
        assert_eq!(read(&two_counts), Err(disagree));
    }

    /// Blocks may cover the same bytes when they agree about them. When they do not, the file is
    /// refused at the first address they disagree about -- including a short block that lies
    /// inside a long one and is not that block's neighbor once the blocks are in order of address.
    #[test]
    fn overlapping_blocks_are_taken_only_when_they_agree() {
        let long: Vec<u8> = (0..MAX_PAYLOAD).map(|i| (i % 241) as u8).collect();
        let mut agree = flash(XIP_BASE, &long, 0, 2);
        agree.extend(flash(XIP_BASE + 0x40, &long[0x40..0x80], 1, 2));
        assert_eq!(read(&agree).unwrap(), long);

        let mut inner = long[0x100..0x140].to_vec();
        inner[0x10] ^= 0xFF;
        let mut disagree = flash(XIP_BASE, &long, 0, 3);
        disagree.extend(flash(XIP_BASE + 0x40, &long[0x40..0x80], 1, 3));
        disagree.extend(flash(XIP_BASE + 0x100, &inner, 2, 3));
        let overlap = ReadError::Overlap { address: XIP_BASE + 0x110, first: 0, second: 2 * BLOCK };
        assert_eq!(read(&disagree), Err(overlap));
    }

    /// A payload that runs past the top of the 32-bit address space is refused, and the family's
    /// blocks carrying no byte for flash is refused rather than read as an empty image.
    #[test]
    fn a_payload_past_the_address_space_and_a_family_with_nothing_for_flash_are_refused() {
        let top = flash(0xFFFF_FF80, &[0x77; PAYLOAD], 0, 1);
        assert_eq!(read(&top), Err(ReadError::AddressOverflow { offset: 0 }));
        let comment = FLAG_NOT_MAIN_FLASH | FLAG_FAMILY_ID;
        let only_comments = block(comment, 0, b"source", 0, 1, FAMILY_RP2350_ARM_S);
        assert_eq!(read(&only_comments), Err(ReadError::NothingToWrite));
    }
}
