//! A SAM D21's NVM User Row, through the route that writes its board: read it, and set its
//! BOOTPROT field.

use crate::{Programmer, SamFamily};
use lamella_cmsis_dap_sam::{SamIdentify as _, Samd21UserRowAccess as _};
use lamella_probe_core::TargetAccess;

pub use lamella_cmsis_dap_sam::{
    SAMD21_BOOTPROT_NONE, SAMD21_USER_ROW_FIELDS, SamDeviceId, Samd21UserRow, Samd21UserRowError,
    Samd21UserRowField, samd21_bootprot_bytes,
};

/// A SAM D21's user row, read twice through its board's route with the same answer both times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserRowReading {
    /// The part that answered, as its DSU names it.
    pub part: SamDeviceId,
    /// The part's own 128-bit serial number, word 0 first, which names the one part the row
    /// belongs to.
    pub serial: [u32; 4],
    /// The row both reads returned.
    pub row: Samd21UserRow,
}

/// What a write of the user row left on the part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRowWritten {
    /// The row as it read back after the write.
    pub row: Samd21UserRow,
    /// How the reset that puts the new row in force went. A written row takes effect at the part's
    /// next reset, so an `Err` here means the part still runs on its previous configuration.
    pub reset: Result<(), String>,
}

/// Whether `programmer` reaches a SAM D21 through a probe, which is the only way this module reads
/// or writes a user row.
pub fn reaches_a_samd21(programmer: Programmer) -> bool {
    matches!(
        programmer,
        Programmer::EdbgOnboard {
            family: SamFamily::Samd21,
            ..
        } | Programmer::SamExternalProbe {
            family: SamFamily::Samd21
        }
    )
}

/// Reads the NVM User Row of the SAM D21 behind `programmer` twice, and returns it when the two
/// reads agree.
///
/// Nothing is written, and the core is neither halted nor reset.
///
/// # Errors
/// A route that does not reach a SAM D21 through a probe, a probe that cannot be chosen or opened,
/// a part whose DSU does not name a SAM D21, and two reads that disagree. Each is worded for a
/// reader.
pub fn read(programmer: Programmer, probe: Option<&str>) -> Result<UserRowReading, String> {
    if !reaches_a_samd21(programmer) {
        return Err(not_a_samd21_route(programmer));
    }
    let mut target = crate::open_sam_route(programmer, probe)?;
    read_from(&mut target)
}

/// Sets BOOTPROT to `bootprot` in the user row of the SAM D21 behind `programmer`, keeping every
/// other bit as `reading` holds it, then resets the part so that the new value is in force.
///
/// `reading` is what [`read`] returned for this board, and what the caller has saved. Nothing is
/// written unless the part still identifies as the one in `reading` and its row still reads as
/// `reading.row`.
///
/// # Errors
/// [`Samd21UserRowError::Unchanged`] when nothing was erased or written, and
/// [`Samd21UserRowError::NotRewritten`] when the row was erased and did not read back as intended.
/// In both cases the part is not reset, so it keeps running on the configuration it had.
pub fn set_bootprot(
    programmer: Programmer,
    probe: Option<&str>,
    reading: &UserRowReading,
    bootprot: u8,
) -> Result<UserRowWritten, Samd21UserRowError> {
    if !reaches_a_samd21(programmer) {
        return Err(Samd21UserRowError::Unchanged(not_a_samd21_route(
            programmer,
        )));
    }
    let mut target =
        crate::open_sam_route(programmer, probe).map_err(Samd21UserRowError::Unchanged)?;
    set_on(&mut target, reading, bootprot)
}

/// Writes `saved` back as the user row of the SAM D21 behind `programmer`, then resets the part so
/// that it is in force: the way back from [`set_bootprot`], finished or not.
///
/// `saved` is a copy of this part's row from before the change, and `saved_serial` is the serial
/// number of the part it was read from. Nothing is written unless `saved_serial` is this part's,
/// the part still identifies as the one in `reading`, its row still reads as `reading.row`, and
/// that row is part-way to `saved` ([`Samd21UserRow::is_partway_to`]).
///
/// # Errors
/// As [`set_bootprot`]'s, and the part is not reset on either.
pub fn restore(
    programmer: Programmer,
    probe: Option<&str>,
    reading: &UserRowReading,
    saved: &Samd21UserRow,
    saved_serial: &[u32; 4],
) -> Result<UserRowWritten, Samd21UserRowError> {
    if !reaches_a_samd21(programmer) {
        return Err(Samd21UserRowError::Unchanged(not_a_samd21_route(
            programmer,
        )));
    }
    let mut target =
        crate::open_sam_route(programmer, probe).map_err(Samd21UserRowError::Unchanged)?;
    restore_on(&mut target, reading, saved, saved_serial)
}

fn read_from<A: TargetAccess>(target: &mut A) -> Result<UserRowReading, String> {
    let part = identify(target)?;
    let first = target
        .read_samd21_user_row()
        .map_err(|why| format!("reading the user row: {why}"))?;
    let second = target
        .read_samd21_user_row()
        .map_err(|why| format!("reading the user row a second time: {why}"))?;
    if first != second {
        return Err(
            "two reads of the user row disagreed, so neither is used; check the probe's \
                    connection to the board and read it again"
                .to_owned(),
        );
    }
    let serial = target
        .read_samd21_serial_number()
        .map_err(|why| format!("reading the part's serial number: {why}"))?;
    Ok(UserRowReading {
        part,
        serial,
        row: first,
    })
}

fn set_on<A: TargetAccess>(
    target: &mut A,
    reading: &UserRowReading,
    bootprot: u8,
) -> Result<UserRowWritten, Samd21UserRowError> {
    if reading.row.bootprot() == bootprot {
        return Err(Samd21UserRowError::Unchanged(format!(
            "BOOTPROT already holds {bootprot}, so there is nothing to change"
        )));
    }
    write_on(target, reading, |target| {
        target.rewrite_samd21_bootprot(&reading.row, bootprot)
    })
}

fn restore_on<A: TargetAccess>(
    target: &mut A,
    reading: &UserRowReading,
    saved: &Samd21UserRow,
    saved_serial: &[u32; 4],
) -> Result<UserRowWritten, Samd21UserRowError> {
    let unchanged = Samd21UserRowError::Unchanged;
    if *saved_serial != reading.serial {
        return Err(unchanged(format!(
            "the saved copy is from the part with serial number {}, and this part's is {}; a row \
             holds calibration that belongs to its own part, so it is not written here",
            serial_text(saved_serial),
            serial_text(&reading.serial)
        )));
    }
    if reading.row == *saved {
        return Err(unchanged(
            "the row already reads as the saved copy, so there is nothing to restore".to_owned(),
        ));
    }
    if !reading.row.is_partway_to(saved) {
        return Err(unchanged(
            "the row holds a bit at zero, outside BOOTPROT, that the saved copy holds at one, so \
             it is not that copy part-way through a BOOTPROT rewrite; the copy is not this row's \
             past"
                .to_owned(),
        ));
    }
    write_on(target, reading, |target| {
        target.restore_samd21_user_row(&reading.row, saved)
    })
}

/// Confirms the part is still the one `reading` came from, halts it, and runs `write`. The part is
/// reset only when the write read back as intended; on a failure, the core it halted is resumed.
fn write_on<A: TargetAccess>(
    target: &mut A,
    reading: &UserRowReading,
    write: impl FnOnce(&mut A) -> Result<Samd21UserRow, Samd21UserRowError>,
) -> Result<UserRowWritten, Samd21UserRowError> {
    let unchanged = Samd21UserRowError::Unchanged;
    let part = identify(target).map_err(unchanged)?;
    if part.raw != reading.part.raw {
        return Err(unchanged(format!(
            "the part now reports DID {:#010x} and the row was read from DID {:#010x}; read this \
             board's row again",
            part.raw, reading.part.raw
        )));
    }
    let serial = target
        .read_samd21_serial_number()
        .map_err(|why| unchanged(format!("reading the part's serial number: {why}")))?;
    if serial != reading.serial {
        return Err(unchanged(format!(
            "the part now reports serial number {} and the row was read from {}; read this board's \
             row again",
            serial_text(&serial),
            serial_text(&reading.serial)
        )));
    }
    let was_halted = target
        .is_halted()
        .map_err(|why| unchanged(format!("reading whether the core is halted: {why}")))?;
    if !was_halted {
        target
            .halt()
            .map_err(|why| unchanged(format!("halting the core: {why}")))?;
    }
    match write(target) {
        Ok(row) => {
            let reset = target.reset_and_run().map_err(|why| format!("{why}"));
            Ok(UserRowWritten { row, reset })
        }
        Err(why) => {
            if !was_halted {
                let _ = target.resume();
            }
            Err(why)
        }
    }
}

/// A serial number as one 32-digit hexadecimal string, word 0 first, the way it names a saved copy
/// of the row.
pub fn serial_text(serial: &[u32; 4]) -> String {
    serial.iter().map(|word| format!("{word:08x}")).collect()
}

/// The part behind the route, refused unless its DSU names a SAM D21.
fn identify<A: TargetAccess>(target: &mut A) -> Result<SamDeviceId, String> {
    let part = target
        .sam_device_id()
        .map_err(|why| format!("reading the part's identity from its DSU: {why}"))?;
    if !part.has_samd21_user_row() {
        return Err(format!(
            "the DSU reports DID {:#010x} -- processor {:#x}, family {:#x}, series {:#x} -- and \
             the user-row layout used here is the SAM D21's (series 0x1), so this part's row is \
             neither read nor written",
            part.raw, part.processor, part.family, part.series
        ));
    }
    Ok(part)
}

fn not_a_samd21_route(programmer: Programmer) -> String {
    format!(
        "a user row is read through a probe route to a SAM D21, and this board is written \
         through {}",
        programmer.description()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_probe_core::ProbeError;

    const ZERO_DID: u32 = 0x1001_0305;
    const DSU_DID: u32 = 0x4100_2018;
    const USER_ROW: u32 = 0x0080_4000;

    fn zero_row() -> Samd21UserRow {
        let mut words = [u32::MAX; 64];
        words[0] = 0xd8e0_c7ff;
        words[1] = 0xffff_fc5d;
        Samd21UserRow { words }
    }

    fn protected_row() -> Samd21UserRow {
        zero_row().with_bootprot(0x2).unwrap()
    }

    /// A part as its DSU, its user row, its core's run state, and a flash controller that erases
    /// and programs the row exactly as asked. The controller's own sequence is tested against a
    /// stricter fake in the chip's crate; what is under test here is the order around it.
    struct FakeZero {
        log: Vec<&'static str>,
        did: u32,
        /// Mixed with each serial-number word's address, so the four words differ.
        serial: u32,
        row: [u32; 64],
        /// When set, every second read of the row returns this instead, as a flaky wire would.
        flicker: Option<[u32; 64]>,
        reads: usize,
        buffer: [u32; 16],
        addr: u32,
        halted: bool,
    }

    impl FakeZero {
        fn holding(row: &Samd21UserRow) -> Self {
            FakeZero {
                log: Vec::new(),
                did: ZERO_DID,
                serial: 0x5e71_a15e,
                row: row.words,
                flicker: None,
                reads: 0,
                buffer: [u32::MAX; 16],
                addr: 0,
                halted: false,
            }
        }
    }

    impl TargetAccess for FakeZero {
        fn connect(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_idcode(&mut self) -> Result<u32, ProbeError> {
            Ok(0x0bc1_1477)
        }
        fn init_mem(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
            match address {
                DSU_DID => Ok(self.did),
                0x0080_a00c | 0x0080_a040 | 0x0080_a044 | 0x0080_a048 => Ok(self.serial ^ address),
                0x4100_4014 => Ok(1),
                0x4100_4018 => Ok(0),
                0x4100_4004 => Ok(0),
                _ if (USER_ROW..USER_ROW + 256).contains(&address) => {
                    Ok(self.row[((address - USER_ROW) / 4) as usize])
                }
                _ => Err(ProbeError::Device("a read this fake does not model")),
            }
        }
        fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
            match address {
                0x4100_4000 => match value {
                    0xa505 => {
                        self.row = [u32::MAX; 64];
                        self.log.push("EAR");
                    }
                    0xa506 => {
                        let page = ((self.addr << 1).wrapping_sub(0x4000) / 64) as usize;
                        for (word, fill) in self.row[page * 16..page * 16 + 16]
                            .iter_mut()
                            .zip(self.buffer)
                        {
                            *word &= fill;
                        }
                        self.buffer = [u32::MAX; 16];
                        self.log.push("WAP");
                    }
                    0xa544 => self.buffer = [u32::MAX; 16],
                    _ => self.log.push("another command"),
                },
                0x4100_401c => self.addr = value & 0x003f_ffff,
                0x4100_4004 | 0x4100_4018 => {}
                _ if (USER_ROW..USER_ROW + 256).contains(&address) => {
                    self.buffer[((address % 64) / 4) as usize] &= value;
                }
                _ => return Err(ProbeError::Device("a write this fake does not model")),
            }
            Ok(())
        }
        fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
            self.reads += 1;
            if address == USER_ROW
                && let Some(other) = self.flicker
                && self.reads.is_multiple_of(2)
            {
                out.copy_from_slice(&other);
                return Ok(());
            }
            for (index, word) in out.iter_mut().enumerate() {
                *word = self.read_word(address + 4 * index as u32)?;
            }
            Ok(())
        }
        fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
            for (index, word) in words.iter().enumerate() {
                self.write_word(address + 4 * index as u32, *word)?;
            }
            Ok(())
        }
        fn read_byte(&mut self, _: u32) -> Result<u8, ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn write_byte(&mut self, _: u32, _: u8) -> Result<(), ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn read_halfword(&mut self, _: u32) -> Result<u16, ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn write_halfword(&mut self, _: u32, _: u16) -> Result<(), ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn halt(&mut self) -> Result<(), ProbeError> {
            self.halted = true;
            self.log.push("halt");
            Ok(())
        }
        fn resume(&mut self) -> Result<(), ProbeError> {
            self.halted = false;
            self.log.push("resume");
            Ok(())
        }
        fn step(&mut self) -> Result<(), ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn is_halted(&mut self) -> Result<bool, ProbeError> {
            Ok(self.halted)
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            self.halted = false;
            self.log.push("reset_and_run");
            Ok(())
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn set_reset(&mut self, _: bool) -> Result<u8, ProbeError> {
            Err(ProbeError::Device(
                "the reset line is not used on this path",
            ))
        }
        fn read_core_reg(&mut self, _: u8) -> Result<u32, ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn write_core_reg(&mut self, _: u8, _: u32) -> Result<(), ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn set_breakpoint(&mut self, _: u32) -> Result<(), ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn set_breakpoints(&mut self, _: &[u32]) -> Result<(), ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
        fn call_target(
            &mut self,
            _: u32,
            _: &[u32],
            _: &lamella_probe_core::CallFrame,
        ) -> Result<u32, ProbeError> {
            Err(ProbeError::Device("not modelled"))
        }
    }

    #[test]
    fn a_read_returns_the_row_and_touches_nothing() {
        let mut part = FakeZero::holding(&zero_row());
        let reading = read_from(&mut part).unwrap();
        assert_eq!(reading.row, zero_row());
        assert_eq!(reading.part.raw, ZERO_DID);
        assert!(
            part.log.is_empty(),
            "no halt, no command, no reset: {:?}",
            part.log
        );
    }

    #[test]
    fn a_read_refuses_a_part_that_is_not_a_sam_d21() {
        let mut part = FakeZero::holding(&zero_row());
        part.did = 0x1003_0000;
        let why = read_from(&mut part).unwrap_err();
        assert!(
            why.contains("0x10030000") && why.contains("SAM D21"),
            "{why}"
        );
    }

    #[test]
    fn two_reads_that_disagree_are_refused() {
        let mut part = FakeZero::holding(&zero_row());
        part.flicker = Some(protected_row().words);
        let why = read_from(&mut part).unwrap_err();
        assert!(why.contains("disagreed"), "{why}");
    }

    /// The whole write path around the chip's rewrite: halt, erase, one page, then the reset that
    /// puts the value in force -- by a system reset request, never the reset line.
    #[test]
    fn a_bootprot_change_halts_rewrites_and_resets() {
        let mut part = FakeZero::holding(&protected_row());
        let reading = read_from(&mut part).unwrap();
        let set = set_on(&mut part, &reading, 7).unwrap();
        assert_eq!(set.row, zero_row());
        assert_eq!(set.reset, Ok(()));
        assert_eq!(part.log, ["halt", "EAR", "WAP", "reset_and_run"]);
    }

    #[test]
    fn a_change_to_the_value_already_held_is_refused_before_the_core_is_touched() {
        let mut part = FakeZero::holding(&zero_row());
        let reading = read_from(&mut part).unwrap();
        assert!(matches!(
            set_on(&mut part, &reading, 7),
            Err(Samd21UserRowError::Unchanged(_))
        ));
        assert!(part.log.is_empty(), "{:?}", part.log);
    }

    #[test]
    fn a_different_part_is_refused_before_the_core_is_touched() {
        let mut part = FakeZero::holding(&protected_row());
        let reading = read_from(&mut part).unwrap();
        part.did = 0x1001_0300;
        match set_on(&mut part, &reading, 7) {
            Err(Samd21UserRowError::Unchanged(why)) => {
                assert!(why.contains("read this board's row again"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        assert!(part.log.is_empty(), "{:?}", part.log);
    }

    /// A second SAM D21 answering the same DID is still another part, and its serial number says
    /// so before anything is touched.
    #[test]
    fn a_part_with_another_serial_number_is_refused_before_the_core_is_touched() {
        let mut part = FakeZero::holding(&protected_row());
        let reading = read_from(&mut part).unwrap();
        assert_eq!(
            serial_text(&reading.serial),
            "5ef101525ef1011e5ef1011a5ef10116",
            "each word is its own"
        );
        part.serial ^= 1;
        match set_on(&mut part, &reading, 7) {
            Err(Samd21UserRowError::Unchanged(why)) => {
                assert!(why.contains("serial number"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        assert!(part.log.is_empty(), "{:?}", part.log);
    }

    /// A refusal from the chip's rewrite leaves the core running as it was found, and unreset.
    #[test]
    fn a_refused_rewrite_resumes_the_core_it_halted_and_does_not_reset() {
        let mut part = FakeZero::holding(&protected_row());
        let reading = read_from(&mut part).unwrap();
        part.row[40] = 0;
        assert!(matches!(
            set_on(&mut part, &reading, 7),
            Err(Samd21UserRowError::Unchanged(_))
        ));
        assert_eq!(part.log, ["halt", "resume"]);
    }

    #[test]
    fn only_a_probe_route_to_a_sam_d21_reaches_a_user_row() {
        let zero = crate::programmer_for("arduino-zero").unwrap().programmer;
        assert!(reaches_a_samd21(zero), "{zero:?}");
        assert!(!reaches_a_samd21(Programmer::SamExternalProbe {
            family: SamFamily::Sam3x
        }));
        assert!(!reaches_a_samd21(Programmer::EdbgOnboard {
            family: SamFamily::Same54,
            probe_id: 0x2111
        }));
        assert!(!reaches_a_samd21(Programmer::MicrobitV2Daplink));
        assert!(
            read(Programmer::MicrobitV2Daplink, None)
                .unwrap_err()
                .contains("SAM D21")
        );
    }

    /// The way back, around the chip's restore: the same halt, erase, page and reset as the change
    /// it undoes, and the row as it was before the change.
    #[test]
    fn a_restore_puts_back_the_row_a_change_replaced() {
        let mut part = FakeZero::holding(&protected_row());
        let before = read_from(&mut part).unwrap();
        set_on(&mut part, &before, 7).unwrap();
        let cleared = read_from(&mut part).unwrap();
        part.log.clear();
        let restored = restore_on(&mut part, &cleared, &before.row, &before.serial).unwrap();
        assert_eq!(restored.row, protected_row());
        assert_eq!(restored.reset, Ok(()));
        assert_eq!(part.log, ["halt", "EAR", "WAP", "reset_and_run"]);
    }

    /// Each refusal comes before the core is touched: a copy from another part, a copy the row
    /// already holds, and a copy the row cannot have come from.
    #[test]
    fn a_restore_refuses_before_the_core_is_touched() {
        let mut part = FakeZero::holding(&zero_row());
        let reading = read_from(&mut part).unwrap();
        let mut other_part = reading.serial;
        other_part[3] ^= 1;
        let mut not_its_past = zero_row();
        not_its_past.words[0] |= 1 << 17;
        let cases = [
            ("serial number", protected_row(), other_part),
            ("nothing to restore", zero_row(), reading.serial),
            ("not this row's past", not_its_past, reading.serial),
        ];
        for (expected, saved, serial) in cases {
            match restore_on(&mut part, &reading, &saved, &serial) {
                Err(Samd21UserRowError::Unchanged(why)) => {
                    assert!(why.contains(expected), "{expected:?} in {why:?}")
                }
                other => panic!("{expected}: {other:?}"),
            }
            assert!(part.log.is_empty(), "{expected}: {:?}", part.log);
        }
    }
}
