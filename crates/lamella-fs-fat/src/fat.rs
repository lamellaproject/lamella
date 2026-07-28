//! FAT (file allocation table) entry access: turning a cluster number into the next link in its
//! chain, or into a terminal/free/bad verdict.

use crate::boot::FatType;

/// What a FAT entry says about its cluster.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FatEntry {
    /// The cluster is unallocated (value 0).
    Free,
    /// The chain continues at this cluster number. May be out of range on a corrupt volume; the
    /// caller validates it against [`crate::boot::Geometry::is_valid_cluster`] before reading.
    Next(u32),
    /// The bad-cluster marker: this cluster must never be part of a chain.
    Bad,
    /// An end-of-chain marker: the file or directory ends at the previous cluster.
    End,
}

/// The byte offset of cluster `n`'s entry, measured from the start of the FAT region. For FAT12
/// the entry is 12 bits, so consecutive clusters share a byte and the offset advances by 1.5.
pub(crate) fn entry_byte_offset(fat_type: FatType, n: u32) -> u32 {
    match fat_type {
        FatType::Fat12 => n + n / 2,
        FatType::Fat16 => n * 2,
        FatType::Fat32 => n * 4,
    }
}

/// How many bytes must be read at [`entry_byte_offset`] to decode one entry: two for FAT12/16,
/// four for FAT32. (FAT12 reads two bytes and keeps 12 bits of them.)
pub(crate) fn entry_read_width(fat_type: FatType) -> usize {
    match fat_type {
        FatType::Fat12 | FatType::Fat16 => 2,
        FatType::Fat32 => 4,
    }
}

/// Decodes the raw entry value for cluster `n` from `window`, the bytes at its offset (at least
/// [`entry_read_width`] of them). For FAT12 the shared byte is resolved by the cluster's parity;
/// for FAT32 the top four bits are masked off (reserved). The value is then classified.
pub(crate) fn decode(fat_type: FatType, n: u32, window: &[u8]) -> FatEntry {
    let value = match fat_type {
        FatType::Fat12 => {
            let raw = u16::from_le_bytes([window[0], window[1]]);
            u32::from(if n & 1 == 0 { raw & 0x0FFF } else { raw >> 4 })
        }
        FatType::Fat16 => u32::from(u16::from_le_bytes([window[0], window[1]])),
        FatType::Fat32 => {
            u32::from_le_bytes([window[0], window[1], window[2], window[3]]) & 0x0FFF_FFFF
        }
    };
    classify(fat_type, value)
}

/// Encodes `value` (a cluster number, 0 for free, or an end/bad marker) into `window`, the CURRENT
/// FAT bytes at cluster `n`'s offset (at least [`entry_read_width`] of them). The caller reads the
/// bytes, calls this, and writes them back. Bits the width does not own are preserved: FAT12's
/// shared nibble in the neighbouring entry, and FAT32's reserved top four bits (the spec requires a
/// writer to leave them untouched).
pub(crate) fn encode(fat_type: FatType, n: u32, value: u32, window: &mut [u8]) {
    match fat_type {
        FatType::Fat12 => {
            let current = u16::from_le_bytes([window[0], window[1]]);
            let updated = if n & 1 == 0 {
                (current & 0xF000) | (value as u16 & 0x0FFF)
            } else {
                (current & 0x000F) | ((value as u16 & 0x0FFF) << 4)
            };
            window[0..2].copy_from_slice(&updated.to_le_bytes());
        }
        FatType::Fat16 => window[0..2].copy_from_slice(&(value as u16).to_le_bytes()),
        FatType::Fat32 => {
            let current = u32::from_le_bytes([window[0], window[1], window[2], window[3]]);
            let updated = (current & 0xF000_0000) | (value & 0x0FFF_FFFF);
            window[0..4].copy_from_slice(&updated.to_le_bytes());
        }
    }
}

/// Classifies a decoded (and, for FAT32, already-masked) entry value into a [`FatEntry`]. The
/// marker ranges are fixed by the FAT width, not by the volume's cluster count.
fn classify(fat_type: FatType, value: u32) -> FatEntry {
    let (bad, eoc_floor) = match fat_type {
        FatType::Fat12 => (0x0000_0FF7, 0x0000_0FF8),
        FatType::Fat16 => (0x0000_FFF7, 0x0000_FFF8),
        FatType::Fat32 => (0x0FFF_FFF7, 0x0FFF_FFF8),
    };
    if value == 0 {
        FatEntry::Free
    } else if value == bad {
        FatEntry::Bad
    } else if value >= eoc_floor {
        FatEntry::End
    } else {
        FatEntry::Next(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_offsets_match_the_width() {
        assert_eq!(entry_byte_offset(FatType::Fat16, 10), 20);
        assert_eq!(entry_byte_offset(FatType::Fat32, 10), 40);
        assert_eq!(entry_byte_offset(FatType::Fat12, 2), 3);
        assert_eq!(entry_byte_offset(FatType::Fat12, 3), 4);
    }

    #[test]
    fn decodes_fat16_links_and_markers() {
        assert_eq!(decode(FatType::Fat16, 2, &[0x00, 0x00]), FatEntry::Free);
        assert_eq!(decode(FatType::Fat16, 2, &[0x05, 0x00]), FatEntry::Next(5));
        assert_eq!(decode(FatType::Fat16, 2, &[0xF7, 0xFF]), FatEntry::Bad);
        assert_eq!(decode(FatType::Fat16, 2, &[0xF8, 0xFF]), FatEntry::End);
        assert_eq!(decode(FatType::Fat16, 2, &[0xFF, 0xFF]), FatEntry::End);
    }

    #[test]
    fn decodes_fat32_and_masks_the_reserved_top_nibble() {
        assert_eq!(decode(FatType::Fat32, 2, &[0x05, 0x00, 0x00, 0xF0]), FatEntry::Next(5));
        assert_eq!(decode(FatType::Fat32, 2, &[0xF8, 0xFF, 0xFF, 0x0F]), FatEntry::End);
        assert_eq!(decode(FatType::Fat32, 2, &[0xF7, 0xFF, 0xFF, 0xFF]), FatEntry::Bad);
        assert_eq!(decode(FatType::Fat32, 2, &[0x00, 0x00, 0x00, 0x00]), FatEntry::Free);
    }

    #[test]
    fn decodes_fat12_shared_bytes_by_parity() {
        let table = [0x23u8, 0x61, 0x45];
        assert_eq!(decode(FatType::Fat12, 0, &table[0..2]), FatEntry::Next(0x123));
        assert_eq!(decode(FatType::Fat12, 1, &table[1..3]), FatEntry::Next(0x456));
    }

    #[test]
    fn fat12_markers() {
        assert_eq!(decode(FatType::Fat12, 0, &[0xF8, 0x0F]), FatEntry::End);
        assert_eq!(decode(FatType::Fat12, 0, &[0xF7, 0x0F]), FatEntry::Bad);
    }

    #[test]
    fn encode_round_trips_through_decode() {
        for fat_type in [FatType::Fat12, FatType::Fat16, FatType::Fat32] {
            let width = entry_read_width(fat_type);
            let mut window = [0u8; 4];
            encode(fat_type, 2, 0x123, &mut window[..width]);
            assert_eq!(decode(fat_type, 2, &window[..width]), FatEntry::Next(0x123));
        }
    }

    #[test]
    fn fat12_encode_preserves_the_shared_neighbour_nibble() {
        let mut table = [0u8; 3];
        encode(FatType::Fat12, 0, 0x123, &mut table[0..2]);
        encode(FatType::Fat12, 1, 0x456, &mut table[1..3]);
        assert_eq!(decode(FatType::Fat12, 0, &table[0..2]), FatEntry::Next(0x123));
        assert_eq!(decode(FatType::Fat12, 1, &table[1..3]), FatEntry::Next(0x456));
        assert_eq!(table, [0x23, 0x61, 0x45]);
    }

    #[test]
    fn fat32_encode_preserves_the_reserved_top_nibble() {
        let mut window = [0x00, 0x00, 0x00, 0xF0];
        encode(FatType::Fat32, 2, 5, &mut window);
        assert_eq!(window[3] & 0xF0, 0xF0);
        assert_eq!(decode(FatType::Fat32, 2, &window), FatEntry::Next(5));
    }
}
