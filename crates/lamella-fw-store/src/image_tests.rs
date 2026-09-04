//! The image store against a flash that refuses what real flash refuses -- and against one that
//! LIES, because a read-back verify is only worth what it catches.

use super::*;
use crate::{FirmwareFlash, FlashError};

const REGION: usize = 1024;

/// How the fake should misbehave, so each failure the status vocabulary names can be produced.
#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    /// The controller refuses the program outright.
    RefuseWrite,
    /// **The write reports success and one granule does not take.** This is the failure a
    /// read-back exists for and the one that cannot be seen any other way.
    DropOneGranule,
}

struct Fake {
    unit: usize,
    erased: u8,
    cells: [u8; REGION],
    fault: Fault,
    writes: usize,
}

impl Fake {
    fn new(unit: usize, erased: u8) -> Self {
        Fake { unit, erased, cells: [erased; REGION], fault: Fault::None, writes: 0 }
    }
}

impl FirmwareFlash for Fake {
    fn write_unit(&self) -> usize {
        self.unit
    }

    fn erased_byte(&self) -> u8 {
        self.erased
    }

    fn erase(&mut self, offset: usize, len: usize) -> Result<(), FlashError> {
        for at in offset..offset + len {
            self.cells[at] = self.erased;
        }
        Ok(())
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<(), FlashError> {
        assert_eq!(offset % self.unit, 0, "a program must start on a write-unit boundary");
        assert_eq!(data.len() % self.unit, 0, "a program must be whole write units");
        if self.fault == Fault::RefuseWrite {
            return Err(FlashError::Refused);
        }
        self.writes += 1;
        let drop_at = if self.fault == Fault::DropOneGranule && self.writes == 2 { Some(0) } else { None };
        for unit in 0..data.len() / self.unit {
            if drop_at == Some(unit) {
                continue;
            }
            let at = offset + unit * self.unit;
            self.cells[at..at + self.unit]
                .copy_from_slice(&data[unit * self.unit..(unit + 1) * self.unit]);
        }
        Ok(())
    }

    fn read(&self, offset: usize, out: &mut [u8]) -> Result<(), FlashError> {
        out.copy_from_slice(&self.cells[offset..offset + out.len()]);
        Ok(())
    }
}

/// The measured write units again, with the L0's inverted erased value among them.
const FAMILIES: &[(&str, usize, u8)] = &[
    ("stm32f091", 2, 0xFF),
    ("stm32l0", 4, 0x00),
    ("stm32c0", 8, 0xFF),
    ("stm32u5a5", 16, 0xFF),
    ("stm32h7", 32, 0xFF),
    ("samd21", 64, 0xFF),
];

/// A distinct byte at every offset, as a fixed array -- this crate is `no_std` with no allocator,
/// which is the point of it, so the tests do not get one either.
fn image<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let mut i = 0;
    while i < N {
        bytes[i] = (i * 7 + 3) as u8;
        i += 1;
    }
    bytes
}

#[test]
fn a_whole_image_commits_on_every_family() {
    for &(name, unit, erased) in FAMILIES {
        let mut flash = Fake::new(unit, erased);
        let mut store = ImageStore::new(REGION);
        let mut scratch = [0u8; 256];
        store.prepare(&mut flash).expect("prepare");

        let payload = image::<130>();
        let chunk = 64.min(payload.len());
        let mut offset = 0;
        while offset < payload.len() {
            let end = (offset + chunk).min(payload.len());
            let out = store.write_chunk(&mut flash, offset, &payload[offset..end], &mut scratch);
            assert_eq!(out.status, fw_write_status::WRITTEN, "{name} at {offset}");
            offset = end;
        }
        let host_crc = crc::of(&payload);
        let done = store.commit(payload.len() as u32, host_crc);
        assert_eq!(done.status, fw_commit_status::COMMITTED, "{name}: {done:?}");
        assert_eq!(done.image_crc, host_crc, "{name}: the two ends must agree");
    }
}

#[test]
fn the_running_checksum_equals_what_the_host_would_compute_at_every_step() {
    let mut flash = Fake::new(8, 0xFF);
    let mut store = ImageStore::new(REGION);
    let mut scratch = [0u8; 64];
    store.prepare(&mut flash).expect("prepare");
    let payload = image::<96>();
    let mut offset = 0;
    while offset < payload.len() {
        let end = (offset + 24).min(payload.len());
        let out = store.write_chunk(&mut flash, offset, &payload[offset..end], &mut scratch);
        assert_eq!(out.status, fw_write_status::WRITTEN);
        assert_eq!(
            out.running_crc,
            crc::of(&payload[..end]),
            "the running value diverged from the host's at {end}"
        );
        offset = end;
    }
}

#[test]
fn the_pad_is_not_folded_into_the_checksum() {
    let mut flash = Fake::new(64, 0xFF);
    let mut store = ImageStore::new(REGION);
    let mut scratch = [0u8; 128];
    store.prepare(&mut flash).expect("prepare");
    let payload = image::<65>();
    let out = store.write_chunk(&mut flash, 0, &payload, &mut scratch);
    assert_eq!(out.status, fw_write_status::WRITTEN);
    assert_eq!(out.running_crc, crc::of(&payload), "the pad must not be in the checksum");
    assert_eq!(store.commit(65, crc::of(&payload)).status, fw_commit_status::COMMITTED);
}

#[test]
fn the_pad_reads_as_erased_rather_than_as_zero() {
    for &(name, unit, erased) in FAMILIES {
        let mut flash = Fake::new(unit, erased);
        let mut store = ImageStore::new(REGION);
        let mut scratch = [0u8; 256];
        store.prepare(&mut flash).expect("prepare");
        let payload = image::<1>();
        store.write_chunk(&mut flash, 0, &payload, &mut scratch);
        for at in 1..unit {
            assert_eq!(flash.cells[at], erased, "{name}: byte {at} of the pad is not the erased value");
        }
    }
}

#[test]
fn a_granule_that_did_not_take_is_caught_by_the_read_back() {
    let mut flash = Fake::new(8, 0xFF);
    flash.fault = Fault::DropOneGranule;
    let mut store = ImageStore::new(REGION);
    let mut scratch = [0u8; 64];
    store.prepare(&mut flash).expect("prepare");
    let payload = image::<32>();
    assert_eq!(store.write_chunk(&mut flash, 0, &payload[..16], &mut scratch).status, fw_write_status::WRITTEN);
    let out = store.write_chunk(&mut flash, 16, &payload[16..], &mut scratch);
    assert_eq!(out.status, fw_write_status::READBACK_MISMATCH, "a lost granule must be caught");
    assert_eq!(out.accepted, 0, "and nothing may be counted as accepted");
}

#[test]
fn every_refusal_the_vocabulary_names_can_actually_happen() {
    let mut scratch = [0u8; 256];

    let mut flash = Fake::new(8, 0xFF);
    let mut store = ImageStore::new(REGION);
    assert_eq!(store.write_chunk(&mut flash, 0, &[1, 2, 3], &mut scratch).status, fw_write_status::NOT_READY);

    let mut store = ImageStore::new(REGION);
    store.prepare(&mut flash).expect("prepare");
    assert_eq!(store.write_chunk(&mut flash, 4, &[1, 2, 3], &mut scratch).status, fw_write_status::MISALIGNED);

    let mut store = ImageStore::new(16);
    store.prepare(&mut flash).expect("prepare");
    assert_eq!(store.write_chunk(&mut flash, 8, &[0u8; 9], &mut scratch).status, fw_write_status::OUT_OF_REGION);

    let mut refusing = Fake::new(8, 0xFF);
    let mut store = ImageStore::new(REGION);
    store.prepare(&mut refusing).expect("prepare");
    refusing.fault = Fault::RefuseWrite;
    assert_eq!(store.write_chunk(&mut refusing, 0, &[1, 2, 3], &mut scratch).status, fw_write_status::PROGRAM_FAILED);
}

#[test]
fn a_refusal_leaves_the_running_value_where_the_two_ends_still_agree() {
    let mut flash = Fake::new(8, 0xFF);
    let mut store = ImageStore::new(REGION);
    let mut scratch = [0u8; 64];
    store.prepare(&mut flash).expect("prepare");
    let payload = image::<16>();
    let good = store.write_chunk(&mut flash, 0, &payload, &mut scratch);
    let refused = store.write_chunk(&mut flash, 3, &payload, &mut scratch);
    assert_eq!(refused.status, fw_write_status::MISALIGNED);
    assert_eq!(refused.running_crc, good.running_crc, "a refusal must not move the agreed point");
}

#[test]
fn commit_separates_a_short_transfer_from_a_corrupt_one() {
    let mut flash = Fake::new(8, 0xFF);
    let mut store = ImageStore::new(REGION);
    let mut scratch = [0u8; 64];

    assert_eq!(store.commit(0, 0).status, fw_commit_status::NOTHING_WRITTEN);

    store.prepare(&mut flash).expect("prepare");
    let payload = image::<32>();
    store.write_chunk(&mut flash, 0, &payload, &mut scratch);

    assert_eq!(store.commit(64, crc::of(&payload)).status, fw_commit_status::SHORT);
    assert_eq!(store.commit(32, 0xDEAD_BEEF).status, fw_commit_status::CHECKSUM_MISMATCH);
    assert_eq!(store.commit(32, crc::of(&payload)).status, fw_commit_status::COMMITTED);
}

#[test]
fn a_mismatch_reports_the_targets_own_number_beside_the_refusal() {
    let mut flash = Fake::new(8, 0xFF);
    let mut store = ImageStore::new(REGION);
    let mut scratch = [0u8; 64];
    store.prepare(&mut flash).expect("prepare");
    let payload = image::<24>();
    store.write_chunk(&mut flash, 0, &payload, &mut scratch);
    let out = store.commit(24, 0x1234_5678);
    assert_eq!(out.status, fw_commit_status::CHECKSUM_MISMATCH);
    assert_eq!(out.image_crc, crc::of(&payload), "the target's own value must come back");
}

#[test]
fn preparing_again_forgets_the_previous_transfer() {
    let mut flash = Fake::new(8, 0xFF);
    let mut store = ImageStore::new(REGION);
    let mut scratch = [0u8; 64];
    store.prepare(&mut flash).expect("prepare");
    store.write_chunk(&mut flash, 0, &image::<16>(), &mut scratch);
    store.prepare(&mut flash).expect("prepare again");
    assert_eq!(store.written(), 0);
    let payload = image::<8>();
    let out = store.write_chunk(&mut flash, 0, &payload, &mut scratch);
    assert_eq!(out.running_crc, crc::of(&payload), "the running value must have restarted");
}
