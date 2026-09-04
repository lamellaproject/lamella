//! The target side of `FW_WRITE` and `FW_COMMIT`: program an image into an update region a chunk
//! at a time, and disagree with the host when the flash says something different.

use crate::crc;
use crate::{FirmwareFlash, FlashError};
use lamella_wire::msg::{fw_commit_status, fw_write_status};

/// An update region being filled, and the running checksum of what is in it.
///
/// **THE PROGRESS IS STATE ON THE TARGET AND NOT SOMETHING THE HOST RESTATES**, because the point
/// of the running checksum is to catch the two ends disagreeing -- and a value the host supplied
/// cannot disagree with the host.
pub struct ImageStore {
    /// Bytes of the region, from the seam's offset zero.
    region: usize,
    /// The highest offset written plus its length: what `commit` compares against the host's total.
    written: usize,
    /// CRC of `[0, written)` as READ BACK, folded in as each chunk landed.
    running: u32,
    /// Whether the region has been erased since the last commit.
    prepared: bool,
}

/// What one [`ImageStore::write_chunk`] did -- the target half of `FW_RESULT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    /// One of [`fw_write_status`].
    pub status: u8,
    /// How many bytes of the chunk were programmed.
    pub accepted: u32,
    /// CRC of everything programmed so far, read back out of flash.
    pub running_crc: u32,
}

/// What [`ImageStore::commit`] concluded -- the target half of `FW_COMMIT_RESULT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitOutcome {
    /// One of [`fw_commit_status`].
    pub status: u8,
    /// The image's CRC as the TARGET computed it over the whole written range.
    pub image_crc: u32,
}

impl ImageStore {
    /// A store over a region of `region` bytes, with nothing written.
    #[must_use]
    pub fn new(region: usize) -> Self {
        ImageStore { region, written: 0, running: 0, prepared: false }
    }

    /// The bytes held so far, for a `FW_STATUS` that lets a host resume.
    #[must_use]
    pub fn written(&self) -> u32 {
        self.written as u32
    }

    /// Erases the region and forgets everything about what was in it.
    ///
    /// The running checksum resets HERE and not at the first chunk, so a transfer abandoned
    /// half-way and restarted cannot fold new bytes into an old value.
    pub fn prepare<F: FirmwareFlash>(&mut self, flash: &mut F) -> Result<(), FlashError> {
        flash.erase(0, self.region)?;
        self.written = 0;
        self.running = 0;
        self.prepared = true;
        Ok(())
    }

    /// Programs one chunk at `offset` and folds the READ-BACK into the running checksum.
    ///
    /// `scratch` is the caller's read-back buffer and must be at least as long as one padded chunk;
    /// a target with no allocator cannot make one up. A short one is a caller bug and is refused
    /// rather than silently shortening the verify.
    pub fn write_chunk<F: FirmwareFlash>(
        &mut self,
        flash: &mut F,
        offset: usize,
        chunk: &[u8],
        scratch: &mut [u8],
    ) -> WriteOutcome {
        let unit = flash.write_unit();
        if !self.prepared {
            return self.refuse(fw_write_status::NOT_READY);
        }
        if unit == 0 || offset % unit != 0 {
            return self.refuse(fw_write_status::MISALIGNED);
        }
        let padded = chunk.len().next_multiple_of(unit);
        if offset.saturating_add(padded) > self.region {
            return self.refuse(fw_write_status::OUT_OF_REGION);
        }
        if scratch.len() < padded {
            return self.refuse(fw_write_status::NOT_READY);
        }

        let pad = flash.erased_byte();
        let staged = &mut scratch[..padded];
        staged[..chunk.len()].copy_from_slice(chunk);
        for byte in &mut staged[chunk.len()..] {
            *byte = pad;
        }
        if flash.write(offset, staged).is_err() {
            return self.refuse(fw_write_status::PROGRAM_FAILED);
        }

        if flash.read(offset, staged).is_err() {
            return self.refuse(fw_write_status::PROGRAM_FAILED);
        }
        if staged[..chunk.len()] != *chunk {
            return self.refuse(fw_write_status::READBACK_MISMATCH);
        }

        self.running = crc::update(self.running, &staged[..chunk.len()]);
        self.written = self.written.max(offset + chunk.len());
        WriteOutcome {
            status: fw_write_status::WRITTEN,
            accepted: chunk.len() as u32,
            running_crc: self.running,
        }
    }

    /// A refusal, carrying the running value unchanged so a host sees where the two ends still
    /// agree rather than a zero that looks like a reset.
    fn refuse(&self, status: u8) -> WriteOutcome {
        WriteOutcome { status, accepted: 0, running_crc: self.running }
    }

    /// Concludes the transfer against what the HOST says it sent.
    ///
    /// `SHORT` and `CHECKSUM_MISMATCH` are separate answers because the causes do not overlap: a
    /// truncated transfer and a corrupted one need different things looked at, and one number
    /// cannot say which happened.
    #[must_use]
    pub fn commit(&self, total: u32, expected_crc: u32) -> CommitOutcome {
        let outcome = |status| CommitOutcome { status, image_crc: self.running };
        if self.written == 0 {
            return outcome(fw_commit_status::NOTHING_WRITTEN);
        }
        if (self.written as u32) < total {
            return outcome(fw_commit_status::SHORT);
        }
        if self.running != expected_crc {
            return outcome(fw_commit_status::CHECKSUM_MISMATCH);
        }
        outcome(fw_commit_status::COMMITTED)
    }
}

#[cfg(test)]
#[path = "image_tests.rs"]
mod tests;
