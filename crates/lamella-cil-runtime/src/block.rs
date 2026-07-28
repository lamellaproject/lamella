//! The block-device seam: fixed-size sector I/O behind a trait the embedder supplies (a device =
//! a memory card over SPI; a host or a test = a file-backed or in-memory image). This sits BELOW
//! the file-system seam ([`crate::fs`]): that one speaks files and directories, this one speaks
//! numbered sectors, and a FAT driver is what turns the second into the first.

use crate::fs::FsError;

/// The sector size every implementation of this trait presents, in bytes.
///
/// It is a CONSTANT rather than a per-device query on purpose: it is the unit the file-system
/// layer above addresses in, and 512 is that unit for the FAT tier this runtime targets. A medium
/// whose native block differs (a larger physical page, a smaller emulated one) adapts BENEATH the
/// trait -- read-modify-write in its driver -- so nothing above ever branches on geometry.
pub const SECTOR_SIZE: usize = 512;

/// Why a block-device operation failed. Deliberately coarse: a file system can act on "the medium
/// is absent" and "the medium refused the write", and everything finer is diagnostic detail that
/// belongs in the driver rather than in a code the FAT layer would have to interpret.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockError {
    /// No medium is present (no card in the slot). Distinct from [`BlockError::NotReady`]:
    /// this one does not become true by waiting.
    NoMedia,
    /// A medium is present but is not usable yet, or stopped responding -- initialization has
    /// not run, or it did not answer in time. A retry or a re-initialization may fix it.
    NotReady,
    /// The medium is present and readable but refuses writes (a mechanical lock, or a card that
    /// has retired into read-only after wear).
    WriteProtected,
    /// The requested range runs past the end of the medium.
    OutOfRange,
    /// The caller's buffer is not a whole number of [`SECTOR_SIZE`] sectors, or is empty. A
    /// CALLER bug rather than a medium failure, kept separate so it cannot be mistaken for one.
    BadLength,
    /// Any other failure of the medium or the bus (a CRC mismatch, a bus error, a timeout that
    /// is not a readiness problem).
    Io,
}

impl BlockError {
    /// The default projection onto the file-system seam's error codes, for a file system layered
    /// on this trait.
    ///
    /// It lives here so both layers share ONE mapping rather than each inventing its own. A file
    /// system with a reason to be finer (distinguishing a missing card from a bus fault in its own
    /// diagnostics) may of course map differently -- this is the default, not a constraint.
    ///
    /// Everything except a write refusal lands on [`FsError::Io`]: to a caller of `File.Read`
    /// there is no useful distinction between an absent card and a dead one, and .NET's file API
    /// has no vocabulary for either. A write refusal is genuinely
    /// [`FsError::AccessDenied`] -- the operation was understood and declined.
    #[must_use]
    pub fn to_fs_error(self) -> FsError {
        match self {
            BlockError::WriteProtected => FsError::AccessDenied,
            BlockError::NoMedia
            | BlockError::NotReady
            | BlockError::OutOfRange
            | BlockError::BadLength
            | BlockError::Io => FsError::Io,
        }
    }
}

/// The outcome of a block-device operation.
pub type BlockResult<T> = Result<T, BlockError>;

/// A medium addressed as an array of fixed-size sectors: the seam a file system reads and writes
/// through, and the whole of what it may assume about storage.
///
/// Addresses are LOGICAL BLOCK ADDRESSES -- sector indices from 0, not byte offsets -- because
/// that is what the media this targets take on the wire, and converting in one place (here) beats
/// converting at every call site.
///
/// Buffers carry a WHOLE NUMBER OF SECTORS and their length selects the count: a 2048-byte buffer
/// reads four consecutive sectors starting at `lba`. Multi-sector transfers are the point rather
/// than an optimization -- a file system reads a cluster at a time, and a per-sector loop pays a
/// command round trip per 512 bytes on a medium where the command dominates the transfer.
pub trait BlockDevice: core::fmt::Debug {
    /// The number of [`SECTOR_SIZE`] sectors on the medium; multiply by [`SECTOR_SIZE`] for the
    /// capacity in bytes. Takes `&mut self` because a driver may have to ask the medium (and
    /// cache the answer) rather than knowing it statically.
    fn sector_count(&mut self) -> BlockResult<u64>;

    /// Reads `buf.len() / SECTOR_SIZE` consecutive sectors starting at `lba` into `buf`.
    ///
    /// Reads the whole buffer or fails -- there is no short read. A partial transfer would leave
    /// the caller unable to say WHICH sectors are valid, so a driver that gets a partial answer
    /// from the medium reports the failure rather than the fragment.
    ///
    /// [`BlockError::BadLength`] if `buf` is empty or not a whole number of sectors;
    /// [`BlockError::OutOfRange`] if the range ends past [`sector_count`](Self::sector_count).
    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> BlockResult<()>;

    /// Writes `buf.len() / SECTOR_SIZE` consecutive sectors starting at `lba`.
    ///
    /// Writes the whole buffer or fails, for the same reason reads do. A driver may buffer, so a
    /// write is not durable until [`flush`](Self::flush) returns.
    ///
    /// Same length and range errors as [`read_sectors`](Self::read_sectors), plus
    /// [`BlockError::WriteProtected`].
    fn write_sectors(&mut self, lba: u64, buf: &[u8]) -> BlockResult<()>;

    /// Forces any buffered writes onto the medium and waits for it to report itself idle.
    ///
    /// Load-bearing rather than a courtesy: removable media are removed, and a file system that
    /// has updated a directory entry needs to know the sectors actually landed before it reports
    /// the file written. A driver that never buffers implements this as waiting for the medium to
    /// finish its current program cycle -- never as an unconditional `Ok`.
    fn flush(&mut self) -> BlockResult<()>;
}

/// Checks a caller's buffer and range against the medium, returning the sector count the transfer
/// covers. Every driver needs exactly these two checks, and a driver that forgot the range check
/// would hand a wild LBA to the medium -- so they live here once rather than in each backend.
///
/// `Err(BadLength)` for an empty or part-sector buffer, `Err(OutOfRange)` if the transfer would
/// end past `sector_count`.
pub fn transfer_sectors(len: usize, lba: u64, sector_count: u64) -> BlockResult<u64> {
    if len == 0 || len % SECTOR_SIZE != 0 {
        return Err(BlockError::BadLength);
    }
    let sectors = (len / SECTOR_SIZE) as u64;
    match lba.checked_add(sectors) {
        Some(end) if end <= sector_count => Ok(sectors),
        _ => Err(BlockError::OutOfRange),
    }
}

/// A RAM-backed [`BlockDevice`]: the host and test backend, and the one a file system is developed
/// against before any hardware exists.
///
/// This is what makes the FAT layer provable with no card, no bus and no board -- the separable
/// half of the storage stack, testable on its own terms. It is also the reference for what the
/// trait's contracts MEAN, since its behavior is inspectable byte for byte.
#[derive(Debug)]
pub struct MemoryBlockDevice {
    sectors: alloc::vec::Vec<u8>,
    write_protected: bool,
    /// Counts [`BlockDevice::flush`] calls, so a test can assert a file system actually flushed
    /// rather than merely returning success.
    pub flushes: u32,
}

impl MemoryBlockDevice {
    /// A zero-filled medium of `sector_count` sectors.
    #[must_use]
    pub fn new(sector_count: usize) -> Self {
        MemoryBlockDevice {
            sectors: alloc::vec![0u8; sector_count * SECTOR_SIZE],
            write_protected: false,
            flushes: 0,
        }
    }

    /// Makes the medium refuse writes, for exercising the read-only path.
    pub fn set_write_protected(&mut self, protected: bool) {
        self.write_protected = protected;
    }

    /// The backing bytes, for a test to seed an image or inspect the result.
    #[must_use]
    pub fn bytes(&mut self) -> &mut [u8] {
        &mut self.sectors
    }
}

impl BlockDevice for MemoryBlockDevice {
    fn sector_count(&mut self) -> BlockResult<u64> {
        Ok((self.sectors.len() / SECTOR_SIZE) as u64)
    }

    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> BlockResult<()> {
        let total = (self.sectors.len() / SECTOR_SIZE) as u64;
        transfer_sectors(buf.len(), lba, total)?;
        let start = lba as usize * SECTOR_SIZE;
        buf.copy_from_slice(&self.sectors[start..start + buf.len()]);
        Ok(())
    }

    fn write_sectors(&mut self, lba: u64, buf: &[u8]) -> BlockResult<()> {
        if self.write_protected {
            return Err(BlockError::WriteProtected);
        }
        let total = (self.sectors.len() / SECTOR_SIZE) as u64;
        transfer_sectors(buf.len(), lba, total)?;
        let start = lba as usize * SECTOR_SIZE;
        self.sectors[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> BlockResult<()> {
        self.flushes += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_multi_sector_transfer() {
        let mut device = MemoryBlockDevice::new(16);
        assert_eq!(device.sector_count(), Ok(16));

        let mut written = [0u8; SECTOR_SIZE * 4];
        for (index, byte) in written.iter_mut().enumerate() {
            *byte = (index / SECTOR_SIZE) as u8;
        }
        assert_eq!(device.write_sectors(2, &written), Ok(()));

        let mut read = [0xffu8; SECTOR_SIZE * 4];
        assert_eq!(device.read_sectors(2, &mut read), Ok(()));
        assert_eq!(read, written);

        let mut neighbour = [0xffu8; SECTOR_SIZE];
        assert_eq!(device.read_sectors(1, &mut neighbour), Ok(()));
        assert_eq!(neighbour, [0u8; SECTOR_SIZE]);
    }

    #[test]
    fn rejects_a_part_sector_buffer() {
        let mut device = MemoryBlockDevice::new(4);
        let mut buf = [0u8; SECTOR_SIZE - 1];
        assert_eq!(device.read_sectors(0, &mut buf), Err(BlockError::BadLength));
        assert_eq!(device.read_sectors(0, &mut []), Err(BlockError::BadLength));
    }

    #[test]
    fn rejects_a_range_past_the_end() {
        let mut device = MemoryBlockDevice::new(4);
        let mut buf = [0u8; SECTOR_SIZE * 2];
        assert_eq!(device.read_sectors(3, &mut buf), Err(BlockError::OutOfRange));
        assert_eq!(device.read_sectors(2, &mut buf), Ok(()));
    }

    #[test]
    fn a_wrapping_lba_is_out_of_range_not_in_range() {
        assert_eq!(transfer_sectors(SECTOR_SIZE, u64::MAX, 4), Err(BlockError::OutOfRange));
    }

    #[test]
    fn write_protection_refuses_writes_but_allows_reads() {
        let mut device = MemoryBlockDevice::new(4);
        device.set_write_protected(true);
        let buf = [1u8; SECTOR_SIZE];
        assert_eq!(device.write_sectors(0, &buf), Err(BlockError::WriteProtected));
        let mut read = [0xffu8; SECTOR_SIZE];
        assert_eq!(device.read_sectors(0, &mut read), Ok(()));
    }

    #[test]
    fn write_protection_maps_to_access_denied_and_the_rest_to_io() {
        assert_eq!(BlockError::WriteProtected.to_fs_error(), FsError::AccessDenied);
        for error in [BlockError::NoMedia, BlockError::NotReady, BlockError::OutOfRange, BlockError::BadLength, BlockError::Io] {
            assert_eq!(error.to_fs_error(), FsError::Io);
        }
    }
}
