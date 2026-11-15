//! A board's stored settings, such as the network it joins, reached through a debug probe wired to
//! the board: where they lie as the board's probe route addresses its flash, and that flash, read,
//! erased and programmed a page at a time.
//!
//! **THE ROUTE COMES FROM THE BOARD, NEVER FROM ITS CHIP's NAME.** Where a board keeps its settings
//! is its own fact (`[stored_settings]`), and the way a probe reaches its flash is its programming
//! row's probe route. A family whose probe route reaches its settings adds its arm here, and no
//! command changes.

use crate::{PROGRAMMING, Programmer};
use lamella_bsp_gen::strata::StoredSettings;
use lamella_probe_core::TargetAccess;
use lamella_stored_settings::{PAGE, RegionFlash, SLOTS};

/// Where a board keeps its stored settings, as the probe route that reaches them addresses its flash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingsPlace {
    /// The probe route that reaches them.
    pub route: Programmer,
    /// Where the first slot starts, in bytes from the start of the flash the route writes.
    pub offset: u32,
    /// The bytes of one slot: one erase unit of the flash.
    pub erase_unit: u32,
}

/// Where `settings`, a fact of `board`'s, lie through the board's probe route.
///
/// # Errors
/// A board with no probe route that reaches its settings; a place outside the flash that route
/// writes; and a layout other than the one the stored network is kept in, two slots of one erase
/// unit each. Each is a sentence about the board, for a reader.
pub fn place(board: &str, settings: &StoredSettings) -> Result<SettingsPlace, String> {
    let route = route_of(board, reaches_settings)
        .ok_or_else(|| format!("this tool cannot write the stored settings of {board} through a debug probe"))?;
    place_through(board, settings, route)
}

/// The first of `board`'s routes, its own and then its alternate, that `reaches` accepts.
fn route_of(board: &str, reaches: fn(Programmer) -> bool) -> Option<Programmer> {
    PROGRAMMING
        .iter()
        .find(|row| row.board == board)
        .and_then(|row| [Some(row.programmer), row.alternate].into_iter().flatten().find(|route| reaches(*route)))
}

/// Where `settings`, a fact of `board`'s, lie through `route`, which reaches them.
fn place_through(board: &str, settings: &StoredSettings, route: Programmer) -> Result<SettingsPlace, String> {
    let slots = i64::try_from(SLOTS).unwrap_or(i64::MAX);
    if settings.slots != slots || settings.size != settings.slots.saturating_mul(settings.erase_unit) {
        return Err(format!(
            "{board}'s facts place its stored settings in {} slots over {} bytes, and the stored network is kept in \
             two slots of one erase unit each",
            settings.slots, settings.size
        ));
    }
    let table = lamella_catalog::load_board(board).ok_or_else(|| format!("{board} is not a board this build knows"))?;
    let region = table
        .memory
        .iter()
        .find(|region| region.name == settings.memory && region.kind == "flash" && region.controller.is_empty())
        .ok_or_else(|| {
            format!(
                "{board}'s facts place its stored settings in \"{}\", which is not the flash its probe route writes",
                settings.memory
            )
        })?;
    let base = i64::from(route.flash_base());
    let start = if region.base < 0 { base } else { region.base };
    let outside = || format!("{board}'s stored settings lie outside the flash its probe route writes");
    let offset = u32::try_from(start - base + settings.offset).map_err(|_| outside())?;
    let erase_unit = u32::try_from(settings.erase_unit).map_err(|_| outside())?;
    Ok(SettingsPlace { route, offset, erase_unit })
}

/// Whether `route` is a probe route this build reaches a board's stored settings through.
fn reaches_settings(route: Programmer) -> bool {
    matches!(route, Programmer::Rp2350Probe { .. })
}

/// Whether `route` is a volume whose boot ROM this build reaches a board's stored settings through:
/// the RP2350's, whose BOOTSEL also answers PICOBOOT (RP2350 datasheet 5.6).
fn reaches_settings_in_bootsel(route: Programmer) -> bool {
    matches!(route, Programmer::Uf2Volume { family: crate::RP2350_UF2_FAMILY, .. })
}

/// Where `settings`, a fact of `board`'s, lie through the board's boot ROM while the board waits in
/// BOOTSEL: through the board's own volume route, when its family's boot ROM is one this build
/// reaches them through.
///
/// # Errors
/// A board whose routes include no such volume, and every refusal of [`place`] but the first.
pub fn place_in_bootsel(board: &str, settings: &StoredSettings) -> Result<SettingsPlace, String> {
    let route = route_of(board, reaches_settings_in_bootsel)
        .ok_or_else(|| format!("this tool cannot write the stored settings of {board} while it waits in BOOTSEL"))?;
    place_through(board, settings, route)
}

/// A board's flash as its probe route reaches it, for its stored settings.
pub enum SettingsFlash<'a, A: TargetAccess> {
    /// An RP2350's, through its boot ROM's flash API on the halted core.
    Rp2350(lamella_cmsis_dap_rp2350::FlashSession<'a, A>),
}

impl<A: TargetAccess> SettingsFlash<'_, A> {
    /// Ends the session and starts the board again.
    ///
    /// # Errors
    /// The board could not be started again; the text says why.
    pub fn close(self) -> Result<(), String> {
        match self {
            SettingsFlash::Rp2350(session) => session.close(),
        }
        .map_err(not_started)
    }
}

impl<A: TargetAccess> RegionFlash for SettingsFlash<'_, A> {
    fn read(&mut self, offset: u32, len: usize) -> Result<Vec<u8>, String> {
        match self {
            SettingsFlash::Rp2350(session) => session.read(offset, len),
        }
        .map_err(|why| not_read(offset, len, why))
    }

    fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
        match self {
            SettingsFlash::Rp2350(session) => session.erase(offset, len),
        }
        .map_err(|why| not_erased(offset, len, why))
    }

    fn program(&mut self, offset: u32, page: &[u8; PAGE]) -> Result<(), String> {
        match self {
            SettingsFlash::Rp2350(session) => session.program(offset, page),
        }
        .map_err(|why| not_programmed(offset, why))
    }
}

/// A read of the `len` bytes at `offset` that failed with `why`, as every route words it.
fn not_read(offset: u32, len: usize, why: impl std::fmt::Display) -> String {
    format!("reading {len} bytes of the flash at {offset:#x}: {why}")
}

/// An erase of the `len` bytes at `offset` that failed with `why`.
fn not_erased(offset: u32, len: u32, why: impl std::fmt::Display) -> String {
    format!("erasing {len} bytes of the flash at {offset:#x}: {why}")
}

/// A program of the page at `offset` that failed with `why`.
fn not_programmed(offset: u32, why: impl std::fmt::Display) -> String {
    format!("programming the page at {offset:#x}: {why}")
}

/// A board that was not started again, for `why`.
fn not_started(why: impl std::fmt::Display) -> String {
    format!("starting the board again: {why}")
}

/// A board's flash through its boot ROM while it waits in BOOTSEL, each failure worded as a probe
/// route's is.
struct BootselFlash<P: crate::picoboot::PicobootPipe>(crate::picoboot::PicobootFlash<P>);

impl<P: crate::picoboot::PicobootPipe> RegionFlash for BootselFlash<P> {
    fn read(&mut self, offset: u32, len: usize) -> Result<Vec<u8>, String> {
        self.0.read(offset, len).map_err(|why| not_read(offset, len, why))
    }

    fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
        self.0.erase(offset, len).map_err(|why| not_erased(offset, len, why))
    }

    fn program(&mut self, offset: u32, page: &[u8; PAGE]) -> Result<(), String> {
        self.0.program(offset, page).map_err(|why| not_programmed(offset, why))
    }
}

/// Why a board waiting in BOOTSEL named by `serial` could not be opened, from `failure`, worded for
/// the person holding it.
fn bootsel_unopened(serial: &str, failure: &lamella_usbbulk::Error) -> String {
    match failure {
        lamella_usbbulk::Error::NotFound => format!(
            "no board waiting in BOOTSEL with the serial number {serial} is attached; a board waits in BOOTSEL when \
             it is plugged in with its BOOTSEL button held"
        ),
        other => format!("the board waiting in BOOTSEL with the serial number {serial} could not be opened: {other}"),
    }
}

/// Opens `place`'s route and gives `work` the board's flash. The board starts again when `work`
/// returns, whatever it did.
///
/// `serial` names what the route opens: for a probe route, the probe, and with none named, the one
/// `LAMELLA_PROBE_SERIAL` names or the only one attached; for a board waiting in BOOTSEL
/// ([`place_in_bootsel`]), the board's own serial number, which a board in BOOTSEL must be named by.
///
/// Returns what `work` returned, and whether the board started again.
///
/// # Errors
/// A probe or a board that cannot be chosen or opened, and a board that does not answer as its
/// route's part, each worded for a reader. Nothing was written to the board.
pub fn with_flash<R>(
    place: &SettingsPlace,
    serial: Option<&str>,
    work: impl FnOnce(&mut dyn RegionFlash) -> R,
) -> Result<(R, Result<(), String>), String> {
    match place.route {
        Programmer::Rp2350Probe { .. } => {
            let selector = lamella_probe::Selector::named_or_environment(serial);
            let session = lamella_probe::open(&selector).map_err(|why| {
                crate::nothing_written(crate::describe_probe_choice(&why, &selector, serial, place.route))
            })?;
            let mut dap = lamella_probe_core::ArmDap::new(session.into_dap());
            lamella_cmsis_dap_rp2350::connect(&mut dap)
                .map_err(|why| crate::nothing_written(format!("connecting to the board through the probe: {why}")))?;
            let session = lamella_cmsis_dap_rp2350::FlashSession::open(&mut dap)
                .map_err(|why| crate::nothing_written(format!("opening the board's flash: {why}")))?;
            let mut flash = SettingsFlash::Rp2350(session);
            let outcome = work(&mut flash);
            let started = flash.close();
            Ok((outcome, started))
        }
        route if reaches_settings_in_bootsel(route) => {
            let serial = serial.ok_or_else(|| {
                crate::nothing_written(String::from(
                    "a board waiting in BOOTSEL is named by its serial number, and none was given",
                ))
            })?;
            let interface = lamella_usbbulk::ControlInterface::open(
                crate::picoboot::BOOTSEL_VENDOR_ID,
                crate::picoboot::BOOTSEL_PRODUCT_ID,
                Some(serial),
                crate::picoboot::INTERFACE_CLASS,
            )
            .map_err(|why| crate::nothing_written(bootsel_unopened(serial, &why)))?;
            let flash = crate::picoboot::PicobootFlash::open(crate::picoboot::UsbPicoboot::new(interface))
                .map_err(|why| crate::nothing_written(format!("opening the board's boot ROM: {why}")))?;
            let mut flash = BootselFlash(flash);
            let outcome = work(&mut flash);
            let started = flash.0.close().map_err(not_started).and_then(|()| left_bootsel(serial));
            Ok((outcome, started))
        }
        route => Err(format!("{} reaches no stored settings", route.description())),
    }
}

/// How long a board that REBOOT2 restarts is given to leave BOOTSEL, which it does 500 ms after the
/// boot ROM answers.
const LEAVING_BOOTSEL: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether the board named `serial` has left BOOTSEL since REBOOT2 asked it to start again, waited for
/// within [`LEAVING_BOOTSEL`], so a command run next never finds it there, resetting.
pub(crate) fn left_bootsel(serial: &str) -> Result<(), String> {
    let pause = std::time::Duration::from_millis(100);
    if crate::picoboot::until_gone(|| crate::picoboot::waiting_in_bootsel(serial), LEAVING_BOOTSEL, pause) {
        Ok(())
    } else {
        Err(not_started(format!(
            "it still waits in BOOTSEL {} s after its boot ROM took the restart",
            LEAVING_BOOTSEL.as_secs()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `[stored_settings]` record as a board states one, in its flash.
    fn stated(offset: i64, slots: i64) -> StoredSettings {
        StoredSettings {
            memory: String::from("flash"),
            offset,
            size: slots * 0x1000,
            erase_unit: 0x1000,
            slots,
            source: String::from("a test's statement"),
        }
    }

    /// **EVERY BOARD THAT KEEPS STORED SETTINGS HAS A PROBE ROUTE THAT REACHES THEM**, at the place
    /// its own facts state: a board that keeps a network can always be given one through a probe.
    #[test]
    fn every_board_that_keeps_stored_settings_has_a_probe_route_that_reaches_them() {
        let mut keepers = Vec::new();
        for row in PROGRAMMING {
            let Some(table) = lamella_catalog::load_board(row.board) else { continue };
            let Some(settings) = table.stored_settings else { continue };
            let place = place(row.board, &settings).unwrap_or_else(|why| panic!("{}: {why}", row.board));
            assert!(place.route.writes_over_a_probe(), "{}: {:?}", row.board, place.route);
            assert_eq!(i64::from(place.offset), settings.offset, "{}: the flash starts where the route writes", row.board);
            assert_eq!(i64::from(place.erase_unit), settings.erase_unit, "{}", row.board);
            keepers.push(row.board);
        }
        for board in ["raspberrypi-pico-2-w", "pimoroni-pico-plus-2-w", "pimoroni-pico-lipo-2-xl-w"] {
            assert!(keepers.contains(&board), "{board} keeps stored settings: {keepers:?}");
        }
    }

    /// The place a board states, as its route addresses it: a Pico 2 W's last two sectors of 4 MB,
    /// and a Pico Plus 2 W's of 16 MB.
    #[test]
    fn the_place_is_the_boards_own_through_its_probe_route() {
        let pico = place("raspberrypi-pico-2-w", &stated(0x3F_E000, 2)).expect("a place");
        assert!(matches!(pico.route, Programmer::Rp2350Probe { .. }), "{:?}", pico.route);
        assert_eq!((pico.offset, pico.erase_unit), (0x3F_E000, 0x1000));
        let plus = place("pimoroni-pico-plus-2-w", &stated(0xFF_E000, 2)).expect("a place");
        assert_eq!(plus.offset, 0xFF_E000);
    }

    /// A board whose probe route reaches no stored settings is refused by name, in plain words.
    #[test]
    fn a_board_whose_route_reaches_no_settings_is_refused_by_name() {
        let why = place("bbc-micro-bit-v2", &stated(0x7_E000, 2)).expect_err("refused");
        assert!(why.contains("bbc-micro-bit-v2"), "{why}");
        assert!(!why.contains("yet"), "a refusal states what this build does, never what it will: {why}");
    }

    /// The stored network is kept in two slots of one erase unit each, so another layout is refused
    /// rather than written in part.
    #[test]
    fn a_layout_other_than_two_slots_of_one_unit_each_is_refused() {
        let why = place("raspberrypi-pico-2-w", &stated(0x3F_D000, 3)).expect_err("refused");
        assert!(why.contains("raspberrypi-pico-2-w") && why.contains("two slots"), "{why}");
        let mut uneven = stated(0x3F_E000, 2);
        uneven.size = 0x3000;
        assert!(place("raspberrypi-pico-2-w", &uneven).is_err(), "a size that is not its slots' units");
    }

    /// **A BOARD WAITING IN BOOTSEL IS REACHED THROUGH ITS OWN ROUTE's BOOT ROM**, at the place its
    /// facts state: a Pico 2 W's last two sectors of 4 MB and a Pico Plus 2 W's of 16 MB, the same
    /// offsets its probe route gives, through the board's volume route for the RP2350's family.
    #[test]
    fn the_place_in_bootsel_is_the_boards_own_through_its_boot_rom() {
        let pico = place_in_bootsel("raspberrypi-pico-2-w", &stated(0x3F_E000, 2)).expect("a place");
        assert!(
            matches!(pico.route, Programmer::Uf2Volume { family: crate::RP2350_UF2_FAMILY, .. }),
            "{:?}",
            pico.route
        );
        assert_eq!((pico.offset, pico.erase_unit), (0x3F_E000, 0x1000));
        let plus = place_in_bootsel("pimoroni-pico-plus-2-w", &stated(0xFF_E000, 2)).expect("a place");
        assert_eq!(plus.offset, 0xFF_E000);
    }

    /// Every board that keeps stored settings is reached in BOOTSEL at the place its probe route
    /// reaches, so either way to the board writes the same two slots.
    #[test]
    fn every_board_that_keeps_stored_settings_is_reached_in_bootsel_at_the_same_place() {
        for row in PROGRAMMING {
            let Some(table) = lamella_catalog::load_board(row.board) else { continue };
            let Some(settings) = table.stored_settings else { continue };
            let probe = place(row.board, &settings).unwrap_or_else(|why| panic!("{}: {why}", row.board));
            let bootsel = place_in_bootsel(row.board, &settings).unwrap_or_else(|why| panic!("{}: {why}", row.board));
            assert_eq!((bootsel.offset, bootsel.erase_unit), (probe.offset, probe.erase_unit), "{}", row.board);
        }
    }

    /// A board whose own route is not an RP2350's boot ROM -- a micro:bit's on-board probe, an RP2040's
    /// volume -- is refused by name, in plain words, and never told what a later build may do.
    #[test]
    fn a_board_whose_route_is_not_an_rp2350_boot_rom_is_refused_in_bootsel_by_name() {
        for board in ["bbc-micro-bit-v2", "raspberrypi-pico"] {
            let why = place_in_bootsel(board, &stated(0x1F_E000, 2)).expect_err("refused");
            assert!(why.contains(board), "{why}");
            assert!(!why.contains("yet"), "a refusal states what this build does: {why}");
        }
    }

    /// **A BOARD IN BOOTSEL IS OPENED ONLY BY ITS OWN SERIAL NUMBER**, never as whichever board in
    /// BOOTSEL comes first, and the refusal comes before any device is opened.
    #[test]
    fn a_board_in_bootsel_is_opened_only_by_its_serial_number() {
        let place = SettingsPlace {
            route: Programmer::Uf2Volume { family: crate::RP2350_UF2_FAMILY, base: 0x1000_0000 },
            offset: 0x3F_E000,
            erase_unit: 0x1000,
        };
        let why = with_flash(&place, None, |_| ()).expect_err("refused");
        assert!(why.contains("serial number"), "{why}");
        assert!(why.contains("Nothing was written"), "{why}");
        let why = with_flash(&place, Some("NO-SUCH-BOARD-0000"), |_| ()).expect_err("not attached");
        assert!(why.contains("NO-SUCH-BOARD-0000") && why.contains("BOOTSEL button held"), "{why}");
        assert!(why.ends_with("Nothing was written."), "{why}");
    }

    /// A place in another region than the flash the route writes is refused, not written at an
    /// offset into the wrong memory.
    #[test]
    fn a_place_outside_the_flash_the_route_writes_is_refused() {
        let mut elsewhere = stated(0, 2);
        elsewhere.memory = String::from("sram");
        let why = place("raspberrypi-pico-2-w", &elsewhere).expect_err("refused");
        assert!(why.contains("sram"), "{why}");
    }
}
