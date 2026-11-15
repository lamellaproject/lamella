//! A write of flash regions, and of the image beside them, to an RP2350 waiting in BOOTSEL: each region
//! through the boot ROM's PICOBOOT interface, compared with the flash, written as
//! [`write_through`] writes it and read back; then the image's UF2 copied to the board's bootloader
//! drive, whose completion starts the board. A write of regions alone starts the board through
//! PICOBOOT once they are written.
//!
//! **THE BOARD IS THE ONE BOARD WAITING IN BOOTSEL.** Two RP2350 drives are labeled alike and hold
//! byte-identical files, so nothing on a drive says which board's PICOBOOT interface it shares; with
//! several boards waiting, a region written through one and an image copied to another's drive would
//! each succeed, so the write is refused before anything is opened.
//!
//! **ONLY WHEN THE PICOBOOT INTERFACE CANNOT BE OPENED** do the regions go to the drive instead, in one
//! UF2 with the image, every region's directory in the file's last blocks. The boot ROM writes a UF2's
//! blocks in the order they arrive (RP2350 datasheet 5.5.2), which nothing promises, and that route
//! reads nothing back, so the write says both.

use crate::picoboot::{BootselSession, InBootsel};
use crate::region::{RegionOutcome, RegionSpan, RegionsReport, write_through};

/// Where the addresses the boot ROM's commands take begin the flash, as [`crate::picoboot`] counts it.
const FLASH_BASE: u32 = 0x1000_0000;

/// The flash a region or an image is written in whole sectors of, in the fallback's UF2: a sector the
/// file touches holds no page the file leaves "erased but undefined" (datasheet 5.5.2).
const SECTOR: usize = 4096;

/// What a write to a board waiting in BOOTSEL needs of the computer it runs on.
pub(crate) trait Bootsel {
    /// The RP2350 boards waiting in BOOTSEL. A listing opens nothing.
    fn waiting(&mut self) -> Vec<InBootsel>;

    /// A session with the flash of the board `serial` names, through its PICOBOOT interface, with
    /// exclusive access claimed.
    ///
    /// # Errors
    /// The interface could not be opened, or the boot ROM did not answer; the text says why.
    fn open(&mut self, serial: &str) -> Result<Box<dyn BootselSession>, String>;

    /// Settles which bootloader drive takes the UF2, as the drive route settles it.
    ///
    /// # Errors
    /// No drive, a drive named that is none, or several, each refused as the drive route refuses it.
    fn drive(&mut self) -> Result<(), String>;

    /// Copies `uf2` to the drive [`drive`](Self::drive) settled and waits for the board to start.
    ///
    /// # Errors
    /// The copy failed, or the board did not start; the text says which.
    fn copy(&mut self, uf2: &[u8], say: &mut dyn FnMut(&str)) -> Result<lamella_flash_backend::Report, String>;

    /// Waits for the board `serial` names to leave BOOTSEL once it has been started.
    ///
    /// # Errors
    /// The board still waits in BOOTSEL after the wait's bound.
    fn left(&mut self, serial: &str) -> Result<(), String>;
}

/// Writes `regions`, and `image` when there is one, to the board waiting in BOOTSEL that `bench`
/// reaches. `image` is the UF2 the drive takes, for the chip family `family`.
///
/// # Errors
/// Several boards waiting in BOOTSEL, refused before anything is opened; a drive the drive route
/// refuses, refused before any region is written; and anything that stops a region or the copy,
/// saying what was written before it.
pub(crate) fn write(
    bench: &mut dyn Bootsel,
    image: Option<&[u8]>,
    regions: &[RegionSpan],
    family: u32,
    say: &mut dyn FnMut(&str),
) -> Result<RegionsReport, String> {
    let mut ordered: Vec<&RegionSpan> = regions.iter().collect();
    ordered.sort_by_key(|span| span.base);
    if image.is_some() {
        bench.drive()?;
    }
    let waiting = bench.waiting();
    let serial = match waiting.as_slice() {
        [one] => one.serial.clone(),
        [] => None,
        several => return Err(several_waiting(several)),
    };
    let opened = match &serial {
        Some(serial) => bench.open(serial),
        None if waiting.is_empty() => Err(String::from("no board waiting in BOOTSEL lists a PICOBOOT interface")),
        None => Err(String::from("the board waiting in BOOTSEL reports no serial number to open it by")),
    };
    let mut session = match opened {
        Ok(session) => session,
        Err(why) => return fallback(bench, image, &ordered, family, &why, say),
    };
    let serial = serial.unwrap_or_default();
    say(&format!("writing each region through the PICOBOOT interface of the board waiting in BOOTSEL as {serial}..."));
    let mut report = RegionsReport { image: None, image_held: false, part: None, regions: Vec::new() };
    for span in &ordered {
        match write_through(session.as_mut(), span, FLASH_BASE, say) {
            Ok(outcome) => report.regions.push((span.name.clone(), outcome)),
            Err(why) => {
                let _ = session.release();
                return Err(format!("{why}\n\n{}", stopped_after(&report)));
            }
        }
    }
    match image {
        Some(uf2) => {
            session.release().map_err(|why| format!("letting the board's drive go: {why}\n\n{}", stopped_after(&report)))?;
            let copied = bench.copy(uf2, say).map_err(|why| {
                format!("{why}\n\nEach region was written and read back before the image's UF2 was copied.")
            })?;
            report.image = Some(copied);
        }
        None => {
            session.restart().map_err(|why| format!("starting the board: {why}"))?;
            bench.left(&serial)?;
        }
    }
    Ok(report)
}

/// The refusal of a write while `several` boards wait in BOOTSEL.
fn several_waiting(several: &[InBootsel]) -> String {
    let named: Vec<&str> = several.iter().map(|board| board.serial.as_deref().unwrap_or("(no serial number)")).collect();
    format!(
        "{} boards wait in BOOTSEL: {}. Nothing on a bootloader drive says which board it belongs to, so a region \
         written through one board's PICOBOOT interface and an image copied to a drive could reach two boards. \
         Leave only the board to write in BOOTSEL, or write it through a probe with --via probe.\n\nNothing was \
         written.",
        several.len(),
        named.join(", ")
    )
}

/// What a write that stopped had written, by region.
fn stopped_after(report: &RegionsReport) -> String {
    let written: Vec<&str> = report.regions.iter().map(|(name, _)| name.as_str()).collect();
    match written.as_slice() {
        [] => String::from("The board still waits in BOOTSEL, and nothing was copied to its drive."),
        names => format!(
            "The board still waits in BOOTSEL, and nothing was copied to its drive. Written and read back before it: {}.",
            names.join(", ")
        ),
    }
}

/// The write when the board's PICOBOOT interface could not be opened, for `why`: every region in one
/// UF2 with the image, each region's directory in the file's last blocks, copied to the drive.
fn fallback(
    bench: &mut dyn Bootsel,
    image: Option<&[u8]>,
    regions: &[&RegionSpan],
    family: u32,
    why: &str,
    say: &mut dyn FnMut(&str),
) -> Result<RegionsReport, String> {
    if image.is_none() {
        bench.drive()?;
    }
    let uf2 = combined_uf2(image, regions, family)?;
    let carrying = if image.is_some() { "with the image" } else { "of its own" };
    say(&format!(
        "the board's PICOBOOT interface could not be opened ({why}), so each region goes to the bootloader drive in \
         one UF2 {carrying}, every directory in the file's last blocks. The boot ROM writes a UF2's blocks in the \
         order they arrive, which nothing promises, and this route cannot read them back."
    ));
    let copied = bench.copy(&uf2, say)?;
    Ok(RegionsReport {
        image: image.map(|_| copied),
        image_held: false,
        part: None,
        regions: regions.iter().map(|span| (span.name.clone(), RegionOutcome::Copied { bytes: span.bytes.len() })).collect(),
    })
}

/// One UF2 holding `image`'s blocks for `family`, every page of its last sector included, then each
/// region's blocks after its directory, then every region's directory: one numbering, so the boot ROM
/// takes the file whole or not at all.
///
/// # Errors
/// An image that holds no whole image for `family`, which nothing else here would catch: the drive
/// route copies a `.uf2` as it stands.
fn combined_uf2(image: Option<&[u8]>, regions: &[&RegionSpan], family: u32) -> Result<Vec<u8>, String> {
    let laid = match image {
        Some(uf2) => {
            let held = lamella_flash_format::uf2::image_for_family(uf2, family)
                .map_err(|why| format!("the image's UF2 cannot carry a region beside it: {why}"))?;
            let mut bytes = held.to_bytes();
            bytes.resize(bytes.len().div_ceil(SECTOR) * SECTOR, 0xFF);
            Some((held.base(), bytes))
        }
        None => None,
    };
    let mut spans: Vec<(u32, &[u8])> = laid.iter().map(|(base, bytes)| (*base, bytes.as_slice())).collect();
    spans.extend(regions.iter().filter(|span| span.bytes.len() > span.header as usize).map(|span| {
        (span.base + span.header, &span.bytes[span.header as usize..])
    }));
    spans.extend(regions.iter().map(|span| (span.base, &span.bytes[..(span.header as usize).min(span.bytes.len())])));
    lamella_flash_format::uf2::to_uf2_spans(&spans, family).map_err(|why| why.to_string())
}

/// The computer's own USB, BOOTSEL drives and boot ROMs, as a write through `programmer`, a board's
/// RP2350 drive route, reaches them.
pub(crate) struct Attached<'a> {
    /// The drive route.
    pub(crate) programmer: crate::Programmer,
    /// Where it writes an image from.
    pub(crate) placement: &'a crate::placement::Placement,
    /// The drive `--volume` named, if one was.
    pub(crate) volume: Option<&'a str>,
    /// The parts the write may reach.
    pub(crate) allow: &'a lamella_flash_backend::Allow,
}

impl Bootsel for Attached<'_> {
    fn waiting(&mut self) -> Vec<InBootsel> {
        crate::picoboot::waiting_boards()
    }

    fn open(&mut self, serial: &str) -> Result<Box<dyn BootselSession>, String> {
        let interface = lamella_usbbulk::ControlInterface::open(
            crate::picoboot::BOOTSEL_VENDOR_ID,
            crate::picoboot::BOOTSEL_PRODUCT_ID,
            Some(serial),
            crate::picoboot::INTERFACE_CLASS,
        )
        .map_err(|why| why.to_string())?;
        let flash = crate::picoboot::PicobootFlash::open(crate::picoboot::UsbPicoboot::new(interface))?;
        Ok(Box::new(flash))
    }

    fn drive(&mut self) -> Result<(), String> {
        use lamella_flash_backend::FlashBackend as _;
        let crate::Programmer::Uf2Volume { family, base } = self.programmer else {
            return Err(format!("{} has no bootloader drive", self.programmer.description()));
        };
        crate::backends::Uf2Volume::new(self.volume, base, family).identify().map(|_| ()).map_err(|why| why.to_string())
    }

    fn copy(&mut self, uf2: &[u8], say: &mut dyn FnMut(&str)) -> Result<lamella_flash_backend::Report, String> {
        crate::write_scoped(self.programmer, self.placement, uf2, self.volume, self.allow, say)
    }

    fn left(&mut self, serial: &str) -> Result<(), String> {
        crate::settings::left_bootsel(serial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_stored_settings::{PAGE, RegionFlash};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// What the stand-in computer did, in order, as the tests read it back.
    type Log = Rc<RefCell<Vec<String>>>;

    /// A board's 16 MB of flash behind a PICOBOOT session that logs each step.
    struct Session {
        flash: Rc<RefCell<Vec<u8>>>,
        log: Log,
    }

    impl RegionFlash for Session {
        fn read(&mut self, offset: u32, len: usize) -> Result<Vec<u8>, String> {
            self.log.borrow_mut().push(format!("read {offset:#x}"));
            Ok(self.flash.borrow()[offset as usize..offset as usize + len].to_vec())
        }

        fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
            self.log.borrow_mut().push(format!("erase {offset:#x}"));
            self.flash.borrow_mut()[offset as usize..(offset + len) as usize].fill(0xFF);
            Ok(())
        }

        fn program(&mut self, offset: u32, page: &[u8; PAGE]) -> Result<(), String> {
            self.flash.borrow_mut()[offset as usize..offset as usize + PAGE].copy_from_slice(page);
            Ok(())
        }
    }

    impl BootselSession for Session {
        fn release(self: Box<Self>) -> Result<(), String> {
            self.log.borrow_mut().push(String::from("release"));
            Ok(())
        }

        fn restart(self: Box<Self>) -> Result<(), String> {
            self.log.borrow_mut().push(String::from("restart"));
            Ok(())
        }
    }

    /// A computer with `waiting` boards in BOOTSEL, whose PICOBOOT interface opens unless `unopened`,
    /// and whose drive is there unless `no_drive`.
    struct Bench {
        waiting: Vec<InBootsel>,
        unopened: bool,
        no_drive: bool,
        flash: Rc<RefCell<Vec<u8>>>,
        log: Log,
        copied: Option<Vec<u8>>,
    }

    impl Bench {
        fn with(serials: &[&str]) -> Self {
            Bench {
                waiting: serials.iter().map(|serial| InBootsel { serial: Some((*serial).to_owned()), product: None }).collect(),
                unopened: false,
                no_drive: false,
                flash: Rc::new(RefCell::new(vec![0xFF; 0x100_0000])),
                log: Rc::new(RefCell::new(Vec::new())),
                copied: None,
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
    }

    impl Bootsel for Bench {
        fn waiting(&mut self) -> Vec<InBootsel> {
            self.log.borrow_mut().push(String::from("list"));
            self.waiting.clone()
        }

        fn open(&mut self, serial: &str) -> Result<Box<dyn BootselSession>, String> {
            self.log.borrow_mut().push(format!("open {serial}"));
            if self.unopened {
                return Err(String::from("the interface is not there"));
            }
            Ok(Box::new(Session { flash: Rc::clone(&self.flash), log: Rc::clone(&self.log) }))
        }

        fn drive(&mut self) -> Result<(), String> {
            self.log.borrow_mut().push(String::from("drive"));
            if self.no_drive { Err(String::from("no board is in its bootloader.\n\nNothing was written.")) } else { Ok(()) }
        }

        fn copy(&mut self, uf2: &[u8], _say: &mut dyn FnMut(&str)) -> Result<lamella_flash_backend::Report, String> {
            self.log.borrow_mut().push(String::from("copy"));
            self.copied = Some(uf2.to_vec());
            Ok(lamella_flash_backend::Report {
                mechanism: "the board's bootloader volume, by copying the image",
                identity: lamella_flash_backend::PartIdentity { value: u64::from(FAMILY), what: "a UF2 family" },
                base: FLASH_BASE,
                bytes: uf2.len(),
                verification: lamella_flash_backend::Verification::NotPossible("a drive cannot be read"),
            })
        }

        fn left(&mut self, serial: &str) -> Result<(), String> {
            self.log.borrow_mut().push(format!("left {serial}"));
            Ok(())
        }
    }

    /// The RP2350's UF2 family.
    const FAMILY: u32 = crate::RP2350_UF2_FAMILY;

    /// A corner of two sectors on a 16 MB board, at 0x10FC0000: a directory, then one image.
    fn region() -> RegionSpan {
        let mut bytes = vec![0xFF; 0x2000];
        bytes[..4].copy_from_slice(b"LWFD");
        bytes[0x1000..0x1100].fill(0x5A);
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&lamella_pe::sha256::sha256(&bytes));
        RegionSpan { name: String::from("wifi.region.bin"), base: 0x10FC_0000, bytes, sha256, header: 0x1000, notices: Vec::new() }
    }

    /// An image's UF2, as the drive route hands it on: 300 bytes at the flash's start.
    fn image_uf2() -> Vec<u8> {
        lamella_flash_format::uf2::to_uf2(&[0x42; 300], FLASH_BASE, FAMILY).expect("a UF2")
    }

    /// **THE REGION GOES THROUGH PICOBOOT FIRST, THEN THE DRIVE IS LET GO OF, THEN THE UF2 IS COPIED**:
    /// the drive settled before anything opens, the one board waiting opened by its serial, the region
    /// written and read back, exclusive access released, and only then the image copied.
    #[test]
    fn the_region_goes_through_picoboot_before_the_image_is_copied() {
        let mut bench = Bench::with(&["9B1F2A3C4D5E6F70"]);
        let uf2 = image_uf2();
        let report = write(&mut bench, Some(&uf2), &[region()], FAMILY, &mut |_| {}).expect("written");
        assert_eq!(report.regions, [(String::from("wifi.region.bin"), RegionOutcome::Written { sectors: 2, bytes: 0x2000 })]);
        assert!(report.image.is_some());
        let log = bench.log();
        let steps: Vec<&str> = log.iter().map(String::as_str).filter(|step| !step.starts_with("erase") && !step.starts_with("read")).collect();
        assert_eq!(steps, ["drive", "list", "open 9B1F2A3C4D5E6F70", "release", "copy"], "{log:?}");
        assert_eq!(bench.copied.as_deref(), Some(&uf2[..]), "the image's own UF2, nothing added");
        assert_eq!(&bench.flash.borrow()[0xFC_0000..0xFC_2000], &region().bytes[..]);
    }

    /// **A REGION ALONE NEEDS NO DRIVE:** it is written through PICOBOOT and the board is started, then
    /// waited for until it has left BOOTSEL.
    #[test]
    fn a_region_alone_is_written_through_picoboot_and_the_board_started() {
        let mut bench = Bench::with(&["9B1F2A3C4D5E6F70"]);
        bench.no_drive = true;
        write(&mut bench, None, &[region()], FAMILY, &mut |_| {}).expect("written");
        let log = bench.log();
        let steps: Vec<&str> = log.iter().map(String::as_str).filter(|step| !step.starts_with("erase") && !step.starts_with("read")).collect();
        assert_eq!(steps, ["list", "open 9B1F2A3C4D5E6F70", "restart", "left 9B1F2A3C4D5E6F70"], "{log:?}");
    }

    /// **WITH SEVERAL BOARDS WAITING IN BOOTSEL THE WRITE IS REFUSED BEFORE ANYTHING IS OPENED**, each
    /// named; and a drive the drive route refuses leaves no region written.
    #[test]
    fn several_boards_in_bootsel_or_no_drive_are_refused_before_anything_is_opened() {
        let mut bench = Bench::with(&["9B1F2A3C4D5E6F70", "E66380000000000B"]);
        let uf2 = image_uf2();
        let refusal = write(&mut bench, Some(&uf2), &[region()], FAMILY, &mut |_| {}).expect_err("refused");
        assert!(refusal.contains("2 boards wait in BOOTSEL: 9B1F2A3C4D5E6F70, E66380000000000B"), "{refusal}");
        assert!(refusal.ends_with("Nothing was written."), "{refusal}");
        assert_eq!(bench.log(), ["drive", "list"]);
        let mut bench = Bench::with(&["9B1F2A3C4D5E6F70"]);
        bench.no_drive = true;
        assert!(write(&mut bench, Some(&uf2), &[region()], FAMILY, &mut |_| {}).is_err());
        assert_eq!(bench.log(), ["drive"], "refused before the board is listed");
    }

    /// **ONLY WHEN PICOBOOT CANNOT BE OPENED does a region ride the UF2**: one file, one numbering, the
    /// image first, the region's body, then its directory's blocks last; said so; and the region is
    /// reported copied, never verified.
    #[test]
    fn when_picoboot_cannot_be_opened_the_region_rides_the_uf2_with_its_directory_last() {
        let mut bench = Bench::with(&["9B1F2A3C4D5E6F70"]);
        bench.unopened = true;
        let mut said = Vec::new();
        let uf2 = image_uf2();
        let report = write(&mut bench, Some(&uf2), &[region()], FAMILY, &mut |line| said.push(line.to_owned())).expect("copied");
        assert_eq!(report.regions, [(String::from("wifi.region.bin"), RegionOutcome::Copied { bytes: 0x2000 })]);
        assert!(said.iter().any(|line| line.contains("could not be opened (the interface is not there)") && line.contains("nothing promises")), "{said:?}");
        let copied = bench.copied.expect("a UF2 was copied");
        let addresses: Vec<u32> = copied.chunks(512).map(|block| u32::from_le_bytes(block[12..16].try_into().unwrap())).collect();
        let counts: Vec<u32> = copied.chunks(512).map(|block| u32::from_le_bytes(block[24..28].try_into().unwrap())).collect();
        assert_eq!(addresses.len(), 16 + 16 + 16, "the image's whole sector, the body's sector, the directory's sector");
        assert!(counts.iter().all(|&count| count == 48), "one numbering");
        assert_eq!(addresses[0], FLASH_BASE);
        assert_eq!(addresses[16], 0x10FC_1000, "the region's body after the image");
        assert_eq!(&addresses[32..], (0..16).map(|page| 0x10FC_0000 + page * 256).collect::<Vec<u32>>(), "the directory last");
        let held = lamella_flash_format::uf2::image_for_family(&copied, FAMILY).expect("one whole file").to_bytes();
        assert_eq!(&held[..300], &[0x42; 300]);
        assert_eq!(&held[0xFC_0000..0xFC_2000], &region().bytes[..]);
    }
}
