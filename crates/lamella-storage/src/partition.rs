//! Reaching a FAT volume that sits inside an MBR partition, as an SD card off the shelf does.

use lamella_cil_runtime::block::{BlockDevice, BlockError, BlockResult, SECTOR_SIZE};

/// One FAT partition found in an MBR: where it starts on the medium and how big it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    /// The partition's first sector (LBA) on the underlying medium.
    pub start_lba: u32,
    /// The partition's length in 512-byte sectors.
    pub sector_count: u32,
    /// The MBR partition-type byte (e.g. `0x0E` = FAT16 with LBA addressing).
    pub type_byte: u8,
}

/// Whether an MBR partition-type byte names a FAT variant this stack mounts. Extended (`0x05`/`0x0F`)
/// and hidden/other types are deliberately excluded -- an extended partition is a container, not a
/// volume, and mounting it as FAT would misparse.
#[must_use]
pub fn is_fat_partition_type(type_byte: u8) -> bool {
    matches!(type_byte, 0x01 | 0x04 | 0x06 | 0x0B | 0x0C | 0x0E)
}

/// The first FAT partition described by a medium's sector 0, or `None` if sector 0 is not an MBR
/// with a FAT partition entry (e.g. it is itself a FAT boot sector -- a superfloppy).
///
/// Validates the boot signature and each 16-byte entry (a `0x00`/`0x80` status byte, a FAT type, and
/// a non-zero start/length), so a FAT boot sector -- whose bytes at the partition-table offset are
/// file-system data, not a partition table -- is not mistaken for a partitioned medium.
#[must_use]
pub fn first_fat_partition(sector0: &[u8; SECTOR_SIZE]) -> Option<Partition> {
    if sector0[510] != 0x55 || sector0[511] != 0xAA {
        return None;
    }
    for entry in 0..4 {
        let base = 446 + entry * 16;
        let status = sector0[base];
        if status != 0x00 && status != 0x80 {
            continue;
        }
        let type_byte = sector0[base + 4];
        if !is_fat_partition_type(type_byte) {
            continue;
        }
        let start_lba = u32::from_le_bytes([
            sector0[base + 8],
            sector0[base + 9],
            sector0[base + 10],
            sector0[base + 11],
        ]);
        let sector_count = u32::from_le_bytes([
            sector0[base + 12],
            sector0[base + 13],
            sector0[base + 14],
            sector0[base + 15],
        ]);
        if start_lba == 0 || sector_count == 0 {
            continue;
        }
        return Some(Partition { start_lba, sector_count, type_byte });
    }
    None
}

/// A window onto a slice of an underlying [`BlockDevice`] -- the sectors of one partition, addressed
/// from 0. Every access is bounds-checked against the partition length and offset by its start, so a
/// file system mounted on it can neither read the partition table below it nor another partition
/// beside it.
#[derive(Debug)]
pub struct PartitionBlockDevice<D: BlockDevice> {
    inner: D,
    start_lba: u64,
    sector_count: u64,
}

impl<D: BlockDevice> PartitionBlockDevice<D> {
    /// Wraps `inner` as the `sector_count` sectors beginning at `start_lba`.
    pub fn new(inner: D, start_lba: u64, sector_count: u64) -> Self {
        PartitionBlockDevice { inner, start_lba, sector_count }
    }

    /// Recovers the underlying device (e.g. to reach another partition, or to power down).
    pub fn into_inner(self) -> D {
        self.inner
    }

    /// The number of whole sectors `buf` spans, or `BadLength` if it is empty or not a sector
    /// multiple -- the same contract the seam states, checked here so the offset arithmetic below is
    /// only ever done on a valid length.
    fn sectors_of(buf_len: usize) -> BlockResult<u64> {
        if buf_len == 0 || buf_len % SECTOR_SIZE != 0 {
            return Err(BlockError::BadLength);
        }
        Ok((buf_len / SECTOR_SIZE) as u64)
    }

    /// Translates a partition-relative access to an absolute LBA, rejecting one that runs past the
    /// partition's end with `OutOfRange` (never letting it spill into a neighbour).
    fn absolute(&self, lba: u64, span: u64) -> BlockResult<u64> {
        let end = lba.checked_add(span).ok_or(BlockError::OutOfRange)?;
        if end > self.sector_count {
            return Err(BlockError::OutOfRange);
        }
        self.start_lba.checked_add(lba).ok_or(BlockError::OutOfRange)
    }
}

impl<D: BlockDevice> BlockDevice for PartitionBlockDevice<D> {
    fn sector_count(&mut self) -> BlockResult<u64> {
        Ok(self.sector_count)
    }

    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> BlockResult<()> {
        let span = Self::sectors_of(buf.len())?;
        let absolute = self.absolute(lba, span)?;
        self.inner.read_sectors(absolute, buf)
    }

    fn write_sectors(&mut self, lba: u64, buf: &[u8]) -> BlockResult<()> {
        let span = Self::sectors_of(buf.len())?;
        let absolute = self.absolute(lba, span)?;
        self.inner.write_sectors(absolute, buf)
    }

    fn flush(&mut self) -> BlockResult<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cil_runtime::block::MemoryBlockDevice;

    /// A sector0 with a single FAT16-LBA partition entry (the shape a real SD card carries).
    fn mbr_with_partition(type_byte: u8, start_lba: u32, sectors: u32) -> [u8; SECTOR_SIZE] {
        let mut s = [0u8; SECTOR_SIZE];
        let base = 446;
        s[base] = 0x00;
        s[base + 4] = type_byte;
        s[base + 8..base + 12].copy_from_slice(&start_lba.to_le_bytes());
        s[base + 12..base + 16].copy_from_slice(&sectors.to_le_bytes());
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    #[test]
    fn finds_a_fat_partition_and_rejects_non_mbr_sectors() {
        let mbr = mbr_with_partition(0x0E, 8192, 3_862_528);
        assert_eq!(
            first_fat_partition(&mbr),
            Some(Partition { start_lba: 8192, sector_count: 3_862_528, type_byte: 0x0E })
        );
        let mut no_sig = mbr;
        no_sig[510] = 0;
        assert_eq!(first_fat_partition(&no_sig), None);
        assert_eq!(first_fat_partition(&mbr_with_partition(0x0F, 2048, 1000)), None);
        assert_eq!(first_fat_partition(&mbr_with_partition(0x06, 2048, 0)), None);
        let mut vbr = [0u8; SECTOR_SIZE];
        vbr[0] = 0xEB;
        vbr[510] = 0x55;
        vbr[511] = 0xAA;
        assert_eq!(first_fat_partition(&vbr), None);
    }

    #[test]
    fn the_window_offsets_and_bounds_every_access() {
        let mut backing = MemoryBlockDevice::new(100);
        backing.bytes()[10 * SECTOR_SIZE] = 0xAB;
        backing.bytes()[39 * SECTOR_SIZE] = 0xCD;
        let mut part = PartitionBlockDevice::new(backing, 10, 30);

        assert_eq!(part.sector_count().unwrap(), 30);
        let mut buf = [0u8; SECTOR_SIZE];
        part.read_sectors(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAB);
        part.read_sectors(29, &mut buf).unwrap();
        assert_eq!(buf[0], 0xCD);
        assert_eq!(part.read_sectors(30, &mut buf).unwrap_err(), BlockError::OutOfRange);
        let mut two = [0u8; SECTOR_SIZE * 2];
        assert_eq!(part.read_sectors(29, &mut two).unwrap_err(), BlockError::OutOfRange);

        part.write_sectors(0, &[0x77; SECTOR_SIZE]).unwrap();
        let mut backing = part.into_inner();
        assert_eq!(backing.bytes()[10 * SECTOR_SIZE], 0x77);
        assert_eq!(backing.bytes()[9 * SECTOR_SIZE], 0x00);
    }
}
