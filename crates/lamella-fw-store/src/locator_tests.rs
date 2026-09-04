//! A fake flash that behaves like flash, and a power cut that can land anywhere.

use super::*;
use crate::{FirmwareFlash, FlashError};
use lamella_wire::msg::fw_intent;

/// Room for two slots on the widest granule this crate supports, and then some.
const UNIT_TEST_REGION: usize = 512;

/// A fake flash for one family's geometry, over ONE FLAT REGION holding both locator slots --
/// which is the shape the seam took when the image store arrived and needed the same primitives.
struct Fake {
    unit: usize,
    erased: u8,
    cells: [u8; UNIT_TEST_REGION],
    /// Which units have been programmed since the last erase, so a second program to one unit can
    /// be refused the way a controller refuses it.
    dirty: [bool; UNIT_TEST_REGION],
    /// Writes still permitted before the power goes out. `None` means it never does.
    cut_after: Option<usize>,
    writes: usize,
}

impl Fake {
    fn new(unit: usize, erased: u8) -> Self {
        Fake {
            unit,
            erased,
            cells: [erased; UNIT_TEST_REGION],
            dirty: [false; UNIT_TEST_REGION],
            cut_after: None,
            writes: 0,
        }
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
        if self.cut_after.is_some_and(|n| self.writes >= n) {
            return Err(FlashError::Refused);
        }
        for at in offset..offset + len {
            self.cells[at] = self.erased;
            self.dirty[at] = false;
        }
        Ok(())
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<(), FlashError> {
        assert_eq!(offset % self.unit, 0, "a program must start on a write-unit boundary");
        assert_eq!(data.len() % self.unit, 0, "a program must be whole write units");
        if self.cut_after.is_some_and(|n| self.writes >= n) {
            return Err(FlashError::Refused);
        }
        self.writes += 1;
        for unit in 0..data.len() / self.unit {
            let at = offset + unit * self.unit;
            assert!(!self.dirty[at], "a flash word cannot be programmed twice without an erase");
            self.dirty[at] = true;
        }
        self.cells[offset..offset + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn read(&self, offset: usize, out: &mut [u8]) -> Result<(), FlashError> {
        out.copy_from_slice(&self.cells[offset..offset + out.len()]);
        Ok(())
    }
}

/// Every write unit this tree has actually read out of a reference manual, plus the erased value
/// that goes with it. **Not a plausible range** -- every row is sourced, so a test that passes
/// here passes on every family this tree models.
const FAMILIES: &[(&str, usize, u8)] = &[
    ("stm32f091", 2, 0xFF),
    ("stm32l0", 4, 0x00),
    ("stm32c0", 8, 0xFF),
    ("stm32l476", 8, 0xFF),
    ("stm32u5a5", 16, 0xFF),
    ("stm32h7", 32, 0xFF),
    ("samd21", 64, 0xFF),
];

#[test]
fn a_record_is_at_least_two_write_units_on_every_family() {
    for &(name, unit, _) in FAMILIES {
        let size = slot_size(unit);
        assert!(size >= 2 * unit, "{name}: {size} is not two whole units of {unit}");
        assert_eq!(size % unit, 0, "{name}: a slot must be whole write units");
        assert!(magic_offset(unit) >= PAYLOAD_LEN, "{name}: the magic must land AFTER the payload");
        assert_eq!(magic_offset(unit) % unit, 0, "{name}: the magic starts on a unit boundary");
    }
}

#[test]
fn the_cost_is_the_write_unit_and_not_the_payload() {
    assert_eq!(slot_size(2), 6);
    assert_eq!(slot_size(4), 8);
    assert_eq!(slot_size(8), 16);
    assert_eq!(slot_size(32), 64);
    assert_eq!(slot_size(64), 128);
    assert_eq!(2 * slot_size(64), 256, "two ping-ponged slots on a SAM D21");
}

#[test]
fn the_magic_is_not_what_an_erased_slot_reads_as() {
    for erased in [0x00u8, 0xFFu8] {
        for &(name, unit, _) in FAMILIES {
            let len = magic_len(unit);
            assert_ne!(
                MAGIC[..len],
                [erased; MAGIC.len()][..len],
                "{name}: the stored magic must differ from an erased unit"
            );
        }
    }
}

#[test]
fn a_blank_region_holds_no_choice_on_every_family() {
    for &(name, unit, erased) in FAMILIES {
        let flash = Fake::new(unit, erased);
        assert_eq!(current(&flash), None, "{name}: a blank region must hold no choice");
    }
}

#[test]
fn a_written_choice_reads_back_on_every_family() {
    for &(name, unit, erased) in FAMILIES {
        let mut flash = Fake::new(unit, erased);
        let wrote = activate(&mut flash, BootChoice { next_slot: 1, intent: fw_intent::PERMANENT }, 0)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(wrote.next_slot, 1);
        let (read, _) = current(&flash).unwrap_or_else(|| panic!("{name}: nothing read back"));
        assert_eq!(read, wrote, "{name}: what was read is not what was written");
    }
}

#[test]
fn the_other_slot_is_resolved_against_what_is_running_and_never_stored() {
    for &(name, unit, erased) in FAMILIES {
        let mut flash = Fake::new(unit, erased);
        let wrote =
            activate(&mut flash, BootChoice { next_slot: fw_slot::OTHER, intent: 0 }, 0).unwrap();
        assert_eq!(wrote.next_slot, 1, "{name}: running slot 0, so the other one is 1");
        assert_eq!(current(&flash).unwrap().0.next_slot, 1, "{name}: and that is what is stored");

        let mut flash = Fake::new(unit, erased);
        let wrote =
            activate(&mut flash, BootChoice { next_slot: fw_slot::OTHER, intent: 0 }, 1).unwrap();
        assert_eq!(wrote.next_slot, 0, "{name}: running slot 1, so the other one is 0");
    }
}

#[test]
fn a_second_choice_goes_to_the_other_locator_slot_and_wins() {
    for &(name, unit, erased) in FAMILIES {
        let mut flash = Fake::new(unit, erased);
        activate(&mut flash, BootChoice { next_slot: 0, intent: 0 }, 0).unwrap();
        let (_, first_slot) = current(&flash).unwrap();
        activate(&mut flash, BootChoice { next_slot: 1, intent: fw_intent::ONE_BOOT }, 0).unwrap();
        let (choice, second_slot) = current(&flash).unwrap();
        assert_ne!(first_slot, second_slot, "{name}: the second write must ping-pong");
        assert_eq!(choice.next_slot, 1, "{name}: and the newer record must win");
        assert_eq!(choice.intent, fw_intent::ONE_BOOT);
    }
}

#[test]
fn a_power_cut_anywhere_leaves_the_previous_choice_in_force() {
    for &(name, unit, erased) in FAMILIES {
        let protected = BootChoice { next_slot: 1, intent: fw_intent::ONE_BOOT };
        assert_ne!(protected.next_slot, 0x00, "must differ from an L0's erased payload");
        assert_ne!(protected.next_slot, 0xFF, "must differ from every other part's");
        let mut established = Fake::new(unit, erased);
        activate(&mut established, protected, 0).unwrap();
        let (old, _) = current(&established).unwrap();
        assert_eq!(old, protected, "{name}: the record to protect must be the one we chose");

        for cut in 0..6 {
            let mut flash = Fake::new(unit, erased);
            flash.cells = established.cells;
            flash.dirty = established.dirty;
            flash.cut_after = Some(cut);

            let wanted = BootChoice { next_slot: 2, intent: fw_intent::PERMANENT };
            let attempt = activate(&mut flash, wanted, 0);

            if attempt.is_err() {
                let (_, in_force) = current(&flash).expect("a record was established");
                assert!(
                    read_slot(&flash, in_force.other()).is_none(),
                    "{name}, cut after {cut}: the slot being written decodes as a record, so a                      torn write is visible to a boot path"
                );
            }

            match current(&flash) {
                None => panic!("{name}, cut after {cut}: the record in force DISAPPEARED"),
                Some((read, _)) => {
                    if attempt.is_ok() {
                        assert_eq!(
                            read, wanted,
                            "{name}, cut after {cut}: reported success and did not take"
                        );
                    } else {
                        assert_eq!(
                            read, old,
                            "{name}, cut after {cut}: a torn write CHANGED what boots --                              the previous choice must survive untouched"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn the_sequence_comparison_survives_wrapping() {
    let unit = 8;
    let mut flash = Fake::new(unit, 0xFF);

    for (slot, sequence, next) in
        [(LocatorSlot::First, 0xFFFFu16, 7u8), (LocatorSlot::Second, 0x0000u16, 9u8)]
    {
        let mut body = [0xFFu8; 8];
        body[0] = next;
        body[1] = fw_intent::PERMANENT;
        body[2..4].copy_from_slice(&sequence.to_le_bytes());
        let base = slot.base(unit);
        flash.write(base, &body[..magic_offset(unit)]).unwrap();
        let mut last = [0xFFu8; 8];
        last[..magic_len(unit)].copy_from_slice(&MAGIC[..magic_len(unit)]);
        flash.write(base + magic_offset(unit), &last[..unit]).unwrap();
    }

    let (choice, slot) = current(&flash).expect("both slots are valid");
    assert_eq!(slot, LocatorSlot::Second, "0x0000 is one PAST 0xFFFF, so Second is newer");
    assert_eq!(choice.next_slot, 9);
}

#[test]
fn a_slot_whose_magic_is_absent_reads_as_no_record_rather_than_as_a_choice() {
    let unit = 8;
    let mut flash = Fake::new(unit, 0xFF);
    let mut body = [0xFFu8; 8];
    body[0] = 3;
    body[1] = fw_intent::PERMANENT;
    flash.write(LocatorSlot::First.base(unit), &body[..magic_offset(unit)]).unwrap();
    assert_eq!(current(&flash), None, "a payload without its magic is not a choice");
}
