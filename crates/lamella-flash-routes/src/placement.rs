//! Where a write puts an image on a board whose product ships a bootloader in flash: behind the
//! bootloader, which keeps it, or over it.
//!
//! The bootloader is the `[bootloader]` section of the board's facts, which describes the product
//! rather than one unit, so whether a given part still holds it is read from the part when the
//! image is written. Keeping it is the default. Replacing it is asked for by name, and nothing here
//! carries a bootloader of its own: the way back is the same write with the bootloader's own file.

use crate::{Programmer, Programming, SamFamily};
use lamella_bsp_gen::strata::BootloaderLayout;

/// What a write does with the bootloader a board's product ships in flash.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BootloaderChoice {
    /// Leave it in place and write the image behind it, where the bootloader starts it.
    ///
    /// The default, so a board keeps what its owner can do through its bootloader -- an upload
    /// over USB, and a double-tap reset into the bootloader -- unless they ask otherwise.
    #[default]
    Keep,
    /// Write the image at the start of flash, over the bootloader.
    Replace,
}

/// The bootloader a board's product ships in flash, as the board's facts state it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShippedBootloader {
    /// Its name, as its own source gives it.
    pub name: String,
    /// Where it sits in flash.
    pub base: u32,
    /// How many bytes of flash it occupies from [`Self::base`].
    pub size: u32,
    /// How long it waits after a reset before it starts the image, where the facts state a wait.
    pub reset_wait_ms: Option<u32>,
}

impl ShippedBootloader {
    /// The address one past its last byte, which is where an image kept behind it starts.
    #[must_use]
    pub fn end(&self) -> u32 {
        self.base.saturating_add(self.size)
    }
}

/// Where a write puts an image on one board, and what it does with a bootloader there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Where the route writes from, on a board whose facts state no bootloader in flash.
    Start {
        /// The address the image is written from.
        base: u32,
    },
    /// Behind the bootloader, which stays and starts the image.
    Behind(ShippedBootloader),
    /// At the start of flash, over the bootloader.
    Over(ShippedBootloader),
}

impl Placement {
    /// The address the image's first byte is written to.
    #[must_use]
    pub fn base(&self) -> u32 {
        match self {
            Placement::Start { base } => *base,
            Placement::Behind(bootloader) => bootloader.end(),
            Placement::Over(bootloader) => bootloader.base,
        }
    }

    /// The bootloader the write keeps in front of the image, which the part must hold.
    #[must_use]
    pub fn kept(&self) -> Option<&ShippedBootloader> {
        match self {
            Placement::Behind(bootloader) => Some(bootloader),
            Placement::Start { .. } | Placement::Over(_) => None,
        }
    }

    /// The bootloader the write replaces.
    #[must_use]
    pub fn replaced(&self) -> Option<&ShippedBootloader> {
        match self {
            Placement::Over(bootloader) => Some(bootloader),
            Placement::Start { .. } | Placement::Behind(_) => None,
        }
    }

    /// How many bytes of flash the write keeps in front of the image, which the image cannot have.
    #[must_use]
    pub fn kept_bytes(&self) -> u32 {
        self.kept().map_or(0, |bootloader| bootloader.size)
    }
}

/// Whether a probe route to a `family` part can keep a bootloader in front of an image.
///
/// Only the SAM D21's is built for it: the write must start on an erase row, read the part's
/// vector table before it erases, and refuse a row the part's boot protection covers, and those are
/// the D21's rows and the D21's user row.
#[must_use]
pub fn keeps_a_bootloader(family: SamFamily) -> bool {
    family == SamFamily::Samd21
}

/// Where a write through `route` puts an image on the board `row` names, when the write does
/// `choice` with the bootloader the board's product ships in flash.
///
/// A board whose facts state no bootloader is written from where its route writes from, whatever
/// the choice, except that asking it to replace a bootloader it does not have is refused.
///
/// # Errors
/// [`BootloaderChoice::Replace`] on a board whose facts state no bootloader in flash; a bootloader
/// that does not sit where the route writes from; a route that cannot put an image anywhere else;
/// and a board whose facts do not load. Each is worded for a reader.
pub fn placement_for(
    row: &Programming,
    route: Programmer,
    choice: BootloaderChoice,
) -> Result<Placement, String> {
    let board = lamella_catalog::load_board(row.board)
        .ok_or_else(|| format!("the facts for {} did not load", row.board))?;
    place(row.board, board.bootloader.as_ref(), route, choice)
}

/// [`placement_for`], with the board's `[bootloader]` record already read.
fn place(
    board: &str,
    stated: Option<&BootloaderLayout>,
    route: Programmer,
    choice: BootloaderChoice,
) -> Result<Placement, String> {
    let start = route.flash_base();
    let Some(stated) = stated else {
        return match choice {
            BootloaderChoice::Keep => Ok(Placement::Start { base: start }),
            BootloaderChoice::Replace => Err(format!(
                "--replace-bootloader writes an image over the bootloader a board's product ships \
                 in flash, and\nthe facts for {board} state none. Its images are written from \
                 {start:#010x} and nothing there is\nreplaced, so write it without \
                 --replace-bootloader."
            )),
        };
    };
    let bootloader = shipped(board, stated)?;
    let family = match route {
        Programmer::EdbgOnboard { family, .. } | Programmer::SamExternalProbe { family } => {
            Some(family)
        }
        _ => None,
    };
    if !family.is_some_and(keeps_a_bootloader) {
        return Err(format!(
            "the facts for {board} state the {} in flash, and this build writes {board} through\n\
             {}, which puts an image only at {start:#010x}. Keeping or replacing a \
             bootloader is built\nfor a SAM D21 reached through a probe.",
            bootloader.name,
            route.description()
        ));
    }
    if bootloader.base != start {
        return Err(format!(
            "the facts for {board} state the {} at {:#010x}, and this route writes from \
             {start:#010x};\nan image is kept behind a bootloader only when the bootloader sits \
             at the start of flash.",
            bootloader.name, bootloader.base
        ));
    }
    Ok(match choice {
        BootloaderChoice::Keep => Placement::Behind(bootloader),
        BootloaderChoice::Replace => Placement::Over(bootloader),
    })
}

/// The `[bootloader]` record as addresses, refused where it does not fit this part's 32-bit map.
fn shipped(board: &str, stated: &BootloaderLayout) -> Result<ShippedBootloader, String> {
    let base = u32::try_from(stated.base).ok();
    let size = u32::try_from(stated.size).ok();
    let (Some(base), Some(size)) = (base, size) else {
        return Err(format!(
            "the facts for {board} state the {} at {:#x}, {:#x} bytes long, which is not a span \
             of a 32-bit address map",
            stated.name, stated.base, stated.size
        ));
    };
    if base.checked_add(size).is_none() {
        return Err(format!(
            "the facts for {board} state the {} at {base:#010x}, {size:#x} bytes long, which runs \
             past the end of a 32-bit address map",
            stated.name
        ));
    }
    Ok(ShippedBootloader {
        name: stated.name.clone(),
        base,
        size,
        reset_wait_ms: stated.reset_wait_ms.and_then(|ms| u32::try_from(ms).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROGRAMMING;

    fn zero() -> &'static Programming {
        crate::programmer_for("arduino-zero").expect("the Zero is routed")
    }

    /// A record as the Zero's facts state it, for the cases no real board states.
    fn record(base: i64, size: i64) -> BootloaderLayout {
        BootloaderLayout {
            name: "A Bootloader".to_owned(),
            base,
            size,
            reserved_ram: Some(Vec::new()),
            reset_wait_ms: Some(500),
            interpreter: "bare".to_owned(),
            source: "a test".to_owned(),
        }
    }

    /// The Zero keeps its bootloader unless it is asked not to, and the image goes behind it at
    /// 0x2000, where the bootloader's own source starts the application.
    #[test]
    fn the_zero_writes_behind_its_bootloader_by_default() {
        let row = zero();
        let placement = placement_for(row, row.programmer, BootloaderChoice::default())
            .expect("the Zero states its bootloader");
        let Placement::Behind(bootloader) = &placement else {
            panic!("kept by default: {placement:?}");
        };
        assert_eq!(bootloader.name, "Arduino Zero Bootloader");
        assert_eq!((bootloader.base, bootloader.size), (0x0, 0x2000));
        assert_eq!(bootloader.reset_wait_ms, Some(500));
        assert_eq!(placement.base(), 0x2000);
        assert_eq!(placement.kept_bytes(), 0x2000);
        assert_eq!(placement.replaced(), None);
    }

    /// Asked to replace it, the Zero is written from the start of flash, over the bootloader.
    #[test]
    fn the_zero_writes_over_its_bootloader_only_when_asked() {
        let row = zero();
        let placement = placement_for(row, row.programmer, BootloaderChoice::Replace)
            .expect("the Zero states its bootloader");
        assert!(matches!(&placement, Placement::Over(b) if b.name == "Arduino Zero Bootloader"));
        assert_eq!(placement.base(), row.programmer.flash_base());
        assert_eq!(placement.kept(), None);
        assert_eq!(placement.kept_bytes(), 0);
    }

    /// A board whose facts state no bootloader is written where its route writes from, and asking
    /// it to replace one is refused by name rather than ignored.
    #[test]
    fn a_board_that_states_no_bootloader_is_written_where_its_route_starts() {
        let row = crate::programmer_for("microchip-samd21-xpro").expect("the XPro is routed");
        assert_eq!(
            placement_for(row, row.programmer, BootloaderChoice::Keep),
            Ok(Placement::Start {
                base: row.programmer.flash_base()
            })
        );
        let why = placement_for(row, row.programmer, BootloaderChoice::Replace)
            .expect_err("nothing to replace");
        assert!(
            why.contains("--replace-bootloader") && why.contains("state none"),
            "{why}"
        );
    }

    /// Every routed board has a placement for the default write, and only a board that states a
    /// bootloader moves its image off the route's base.
    #[test]
    fn every_routed_board_has_a_default_placement() {
        let mut moved = Vec::new();
        for row in PROGRAMMING {
            for route in [Some(row.programmer), row.alternate].into_iter().flatten() {
                let placement = placement_for(row, route, BootloaderChoice::Keep)
                    .unwrap_or_else(|why| panic!("{}: {why}", row.board));
                if placement.base() != route.flash_base() {
                    moved.push(row.board);
                }
            }
        }
        assert_eq!(
            moved,
            ["arduino-zero"],
            "only a stated bootloader moves an image"
        );
    }

    /// A bootloader stated on a route that writes only at its own base is refused, whichever way
    /// the write was asked for, because neither can be honored.
    #[test]
    fn a_bootloader_on_a_route_that_cannot_place_an_image_is_refused() {
        let routes = [
            Programmer::EdbgOnboard {
                family: SamFamily::Same54,
                probe_id: 0x2111,
            },
            Programmer::SamExternalProbe {
                family: SamFamily::Sam4Eefc,
            },
            Programmer::MicrobitV2Daplink,
        ];
        for route in routes {
            for choice in [BootloaderChoice::Keep, BootloaderChoice::Replace] {
                let why = place("a-board", Some(&record(0, 0x2000)), route, choice)
                    .expect_err("this route cannot keep a bootloader");
                assert!(
                    why.contains("built\nfor a SAM D21"),
                    "{route:?} {choice:?}: {why}"
                );
            }
        }
    }

    /// A bootloader stated anywhere but where the route starts is refused, since "behind it" is
    /// base plus size only at the start of flash.
    #[test]
    fn a_bootloader_away_from_the_start_of_flash_is_refused() {
        let route = zero().programmer;
        let why = place(
            "a-board",
            Some(&record(0x3_e000, 0x2000)),
            route,
            BootloaderChoice::Keep,
        )
        .expect_err("not at the start of flash");
        assert!(
            why.contains("0x0003e000") && why.contains("start of flash"),
            "{why}"
        );
    }

    /// A record that is not a span of a 32-bit map is refused rather than truncated into one.
    #[test]
    fn a_record_outside_a_32_bit_map_is_refused() {
        let route = zero().programmer;
        for (base, size) in [(-1, 0x2000), (0, 0x1_0000_0000), (0xffff_f000, 0x2000)] {
            let why = place(
                "a-board",
                Some(&record(base, size)),
                route,
                BootloaderChoice::Keep,
            )
            .expect_err("not a span of the map");
            assert!(
                why.contains("32-bit address map"),
                "{base:#x} {size:#x}: {why}"
            );
        }
    }
}
