//! Which mechanism writes which board, and the backends that drive them.

pub mod artifact;
pub mod backends;
pub mod bootsel;
pub mod manifest;

use lamella_catalog as catalog;

/// How a board takes an image, and what the ahead-of-time backend calls its chip.
///
/// **HOW A BOARD IS PROGRAMMED IS NOT WHAT ITS `[[carriers]]` DECLARES, AND THE TWO READ ALIKE.**
/// A carrier records the path the Lamella Link console takes through a RUNNING board; this table
/// records how firmware reaches a BLANK one. A bridge usually offers both, which is why one is so
/// easily mistaken for the other.
///
/// The table lives here in ONE place with a census over it, so a board missing from it fails
/// loudly rather than quietly.
pub struct Programming {
    /// The board id, as `lamella boards` lists it.
    pub board: &'static str,
    /// What the ahead-of-time backend calls this chip, where it can build for it at all.
    ///
    /// **`None` MEANS FLASHABLE BUT NOT BUILDABLE, AND THAT IS A REAL STATE RATHER THAN A GAP IN
    /// THE TABLE.** `flash` takes an image that ALREADY EXISTS -- a published firmware, a release
    /// artifact, something another toolchain linked -- and needs no compiler to write it. `deploy`
    /// compiles first and therefore does need one. A board somebody else's toolchain can build for
    /// and ours cannot is exactly the case the `flash` verb was separated out to serve, so the
    /// table has to be able to say it.
    ///
    pub aot_target: Option<&'static str>,
    /// The route this board takes when nobody asks for another: its own mechanism.
    pub programmer: Programmer,
    /// The route `--via probe` selects, where the board has a second one.
    ///
    /// **THE DEFAULT IS WHATEVER NEEDS NO HARDWARE THE OWNER DOES NOT ALREADY HAVE**, which is one
    /// rule covering every board rather than a preference stated per family:
    ///
    /// - a micro:bit has a DAPLink soldered to it, so its default is that probe
    /// - a NUCLEO has an ST-LINK soldered to it, so its default would be that
    /// - a Pico has no debug hardware at all, so its default is the bootloader drive
    ///
    /// An external probe is therefore never a default anywhere -- not because probes are worse
    /// (the Pico's probe route is the only one that can verify) but because reaching for hardware
    /// the owner may not have turns "does this tool work" into "buy something first".
    ///
    /// Where this is `None` the board has exactly one route and `--via` is refused by name rather
    /// than ignored.
    pub alternate: Option<Programmer>,
}
/// The ways an image reaches a board. One variant per mechanism, not per board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Programmer {
    /// The micro:bit v1's on-board DAPLink probe, over SWD.
    MicrobitV1Daplink,
    /// The micro:bit v2's on-board DAPLink probe, over SWD. A separate variant from the v1's
    /// because the part differs: the write path checks the debug port's IDCODE BEFORE it erases,
    /// so pointing a v2 image at a v1 board stops at a message rather than erasing the board and
    /// then writing an image its core cannot run.
    MicrobitV2Daplink,
    /// A UF2 bootloader volume: the image is COPIED to a drive the halted chip presents. Needs no
    /// probe at all, which is why it is the shortest path from nothing to a running board -- and
    /// why it is the one a person with a brand-new board can follow.
    Uf2Volume {
        /// The chip family the bootloader checks the image against, so one built for another part
        /// is refused instead of run.
        family: u32,
        /// Where the image belongs in the chip's address space.
        base: u32,
    },
    /// An RP2350 over a general-purpose SWD probe, by the chip's own bootrom flash API.
    ///
    /// **NEVER A DEFAULT AND ALWAYS ASKED FOR.** A person who owns no probe has to be able to
    /// flash these boards, so the volume stays the route a board takes when nobody says otherwise.
    /// This is what `--via probe` selects, and it offers the one thing the volume cannot: every
    /// byte read back and compared.
    Rp2350Probe {
        /// Where the image belongs in the chip's address space.
        base: u32,
    },
    /// The ST-LINK soldered to a NUCLEO or Discovery board, driving the part's own flash controller.
    ///
    /// **THIS IS THE BOARD'S OWN MECHANISM, SO IT IS A DEFAULT AND NOT AN ALTERNATE.** The rule is
    /// the one every other row follows -- whatever needs no hardware the owner does not already
    /// have -- and somebody holding a NUCLEO already has the debugger, because it is soldered to
    /// the board. `--via probe` still means an EXTERNAL probe and is a different route.
    StlinkOnboard {
        /// Which family's flash controller to drive. A key into constants, not a mechanism: the
        /// sequencing is identical and only the register addresses and page size differ.
        family: StFamily,
        /// Which ST-LINK generation is fitted, from [`lamella_stlink::product_id`].
        ///
        probe_id: u16,
    },
}

/// An STM32 family, as far as flashing is concerned.
///
/// **IT GROWS WITH BACKENDS, NEVER WITH PRIMITIVES.** A variant added ahead of that would let a caller 
/// NAME a route nobody has run, which is the thing `alternate: None` refuses to do one level up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StFamily {
    /// STM32L0. Erases to ZERO, unlike every other part this tree programs.
    L0,
}
impl Programmer {
    /// What this mechanism is, for a person reading the output.
    pub fn description(self) -> &'static str {
        match self {
            Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => {
                "the board's on-board DAPLink probe, over SWD"
            }
            Programmer::Uf2Volume { .. } => "the board's bootloader volume, by copying the image",
            Programmer::Rp2350Probe { .. } => "an SWD probe, by the chip's own bootrom flash API",
            Programmer::StlinkOnboard { .. } => {
                "the ST-LINK on the board, by the part's own flash controller"
            }
        }
    }

    /// The address this mechanism writes an image from.
    ///
    /// **ONE PLACE STATES IT, so the address a `build --format` file declares is the address a
    /// write actually uses.** A file that said one thing while the writer did another would be
    /// wrong in the way nothing catches: it would flash correctly here and be rejected, or
    /// misplaced, by somebody else's programmer.
    pub fn flash_base(self) -> u32 {
        match self {
            Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => 0,
            Programmer::Uf2Volume { base, .. } => base,
            Programmer::Rp2350Probe { base } => base,
            Programmer::StlinkOnboard { family: StFamily::L0, .. } => {
                lamella_cmsis_dap_stm32::STM32L0_FLASH_BASE
            }
        }
    }

    /// The format an image must be written in to reach this mechanism, where the mechanism decides
    /// it. A probe takes raw bytes; a bootloader volume takes a file, and which file matters.
    pub fn required_format(self) -> Option<crate::artifact::Format> {
        match self {
            Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => None,
            Programmer::Uf2Volume { family, .. } => {
                Some(crate::artifact::Format::Uf2 { family })
            }
            Programmer::Rp2350Probe { .. } => None,
            Programmer::StlinkOnboard { .. } => None,
        }
    }

    /// How many of this route's units `bytes` amounts to, named.
    ///
    pub fn units(self, bytes: usize) -> String {
        match self {
            Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => {
                format!("{} words", bytes.div_ceil(4))
            }
            Programmer::Uf2Volume { .. } => format!("{} blocks", bytes / UF2_BLOCK),
            Programmer::Rp2350Probe { .. } => format!("{} words", bytes.div_ceil(4)),
            Programmer::StlinkOnboard { .. } => format!("{} words", bytes.div_ceil(4)),
        }
    }

    /// The USB vendor and product this mechanism's probes present, taken from the crate that owns
    /// the fact rather than restated here.
    ///
    /// **`Some` IS WHAT MAKES AN ON-BOARD DEBUGGER UNAMBIGUOUS, and that is the whole reason this
    /// exists.** A debugger soldered to a board is bound to that board by construction: it cannot
    /// be the one wired to something else. So a bench holding a micro:bit AND an external probe has
    /// no ambiguity to resolve -- the filter simply does not see the external one, and the board
    /// the reader named is written by its own debugger without a question being asked.
    ///
    /// `None` says the opposite and means it: every candidate is a separate piece of hardware that
    /// could be attached to any board, so several of them IS ambiguous and gets refused.
    pub fn usb_identity(self) -> Option<(u16, u16)> {
        match self {
            Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => {
                Some(lamella_cmsis_dap_nrf::MICROBIT_DAPLINK)
            }
            Programmer::Uf2Volume { .. } => None,
            Programmer::Rp2350Probe { .. } => None,
            Programmer::StlinkOnboard { probe_id, .. } => {
                Some((lamella_stlink::ST_VENDOR_ID, probe_id))
            }
        }
    }
}
/// The RP2350 in its Arm secure profile, as its bootloader checks it. `bin2uf2` states the same
/// number and this is the second place it appears; when a third wants it, it wants a home in
/// `lamella-wire` beside the other wire-visible identifiers rather than a third copy.
pub const RP2350_UF2_FAMILY: u32 = 0xe48b_ff59;
/// The RP2040's family id, for the same bootloader on the older part.
pub const RP2040_UF2_FAMILY: u32 = 0xe48b_ff56;
/// Where an RP2350 or RP2040 image belongs: the base of execute-in-place flash.
pub const RP2_XIP_BASE: u32 = 0x1000_0000;
/// Every board this build can write, and how.
pub const PROGRAMMING: &[Programming] = &[
    Programming {
        board: "micro-bit-v1",
        aot_target: Some("microbit"),
        programmer: Programmer::MicrobitV1Daplink,
        alternate: None,
    },
    Programming {
        board: "micro-bit-v2",
        aot_target: Some("nrf52833"),
        programmer: Programmer::MicrobitV2Daplink,
        alternate: None,
    },
    Programming {
        board: "rpi-pico2",
        aot_target: Some("rp2350"),
        programmer: Programmer::Uf2Volume { family: RP2350_UF2_FAMILY, base: RP2_XIP_BASE },
        alternate: Some(Programmer::Rp2350Probe { base: RP2_XIP_BASE }),
    },
    Programming {
        board: "rpi-pico2-w",
        aot_target: Some("rp2350"),
        programmer: Programmer::Uf2Volume { family: RP2350_UF2_FAMILY, base: RP2_XIP_BASE },
        alternate: Some(Programmer::Rp2350Probe { base: RP2_XIP_BASE }),
    },
    Programming {
        board: "rpi-pico",
        aot_target: Some("rp2040"),
        programmer: Programmer::Uf2Volume { family: RP2040_UF2_FAMILY, base: RP2_XIP_BASE },
        alternate: None,
    },
    Programming {
        board: "rpi-pico-w",
        aot_target: Some("rp2040"),
        programmer: Programmer::Uf2Volume { family: RP2040_UF2_FAMILY, base: RP2_XIP_BASE },
        alternate: None,
    },
    Programming {
        board: "nucleo-l053r8",
        aot_target: None,
        programmer: Programmer::StlinkOnboard {
            family: StFamily::L0,
            probe_id: lamella_stlink::product_id::V2_1,
        },
        alternate: None,
    },
];
/// Whether `lamella flash` can write `board`, for the `boards` listing's coverage column.
#[must_use]
pub fn can_flash(board: &str) -> bool {
    PROGRAMMING.iter().any(|row| row.board == board)
}
/// The first four bytes of a PICOBIN block, little-endian, as the RP2350 bootrom looks for them.
///
/// One place states it here and `lamella_aot::build`'s `rp2350_boot_image` states it where an image
/// is BUILT; the two are the same number for the same reason, and this one is a reader rather than
/// a writer.
pub const PICOBIN_BLOCK_START: u32 = 0xffff_ded3;
/// How far into flash the bootrom looks for the block.
pub const PICOBIN_SCAN_BYTES: usize = 4096;
/// Refuse an RP2350 image the bootrom would silently decline to boot.
///
/// **THE FAILURE THIS PREVENTS HAS NO SYMPTOM.** The RP2350 bootrom scans the first 4 KB of flash
/// for a PICOBIN `IMAGE_DEF` block and, finding none, simply does not boot -- no fault, no output,
/// nothing to read back. A board flashed with a correct-but-unstamped image is indistinguishable
/// from a blank one, and from a board whose program hung on its first instruction.
///
/// Images this toolchain builds carry the block already. The ones that do not are the ones this
/// verb exists for: an image somebody else's toolchain produced, where stamping is a separate step
/// in their build rather than a property of the linker output.
///
/// # Errors
/// When `bytes` is for an RP2350 board and carries no PICOBIN block in the scanned window.
pub fn check_rp2350_stamp(bytes: &[u8], aot_target: Option<&str>) -> Result<(), String> {
    if aot_target != Some("rp2350") {
        return Ok(());
    }
    let window = &bytes[..bytes.len().min(PICOBIN_SCAN_BYTES)];
    let found = window
        .chunks_exact(4)
        .any(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]) == PICOBIN_BLOCK_START);
    if found {
        return Ok(());
    }
    Err(format!(
        "this image carries no PICOBIN IMAGE_DEF block in its first {} bytes, and the RP2350 \
         bootrom will not boot it.\nThere is no symptom to read back: an unstamped image looks \
         exactly like a blank chip.\nStamp it in the build that linked it -- `lamella build \
         --board <id>` stamps what it produces.",
        window.len()
    ))
}
/// Which INSTANCE of the chosen route to write to, from the option that names it.
///
/// **A DRIVE AND A PROBE ARE TWO DIFFERENT QUESTIONS AND MUST NOT SHARE ONE WORD.** One `--probe`
/// naming a serial on a probe route and a drive letter on a volume route reasons that both answer
/// "which of several identical things". They are not identical things: they live in
/// different namespaces, they are discovered by different mechanisms, and a reader who typed the
/// one the route did not want got no complaint -- the value was simply passed to something that
/// interpreted it differently.
///
/// # Errors
/// Naming the selector the chosen route does not use. Refused rather than ignored: a `--probe`
/// silently dropped on a volume route reads as "I told it which board" while nothing was told.
pub fn selector_for(
    programmer: Programmer,
    probe: Option<&str>,
    volume: Option<&str>,
) -> Result<Option<String>, String> {
    let wants_volume = matches!(programmer, Programmer::Uf2Volume { .. });
    match (wants_volume, probe, volume) {
        (true, Some(_), _) => Err(
            "--probe names a debug probe, and this write goes to a bootloader DRIVE.\n\n\
Name the drive with --volume <name>, or ask for the probe route with --via probe."
                .to_owned(),
        ),
        (false, _, Some(_)) => Err(
            "--volume names a bootloader drive, and this write goes over a PROBE.\n\n\
Name the probe with --probe <serial>, or ask for the drive with --via volume."
                .to_owned(),
        ),
        (true, None, chosen) | (false, chosen, None) => Ok(chosen.map(str::to_owned)),
    }
}
/// Which route to write by: the board's default, or the one `--via` asks for.
///
/// **`--via probe` IS A MANUAL OPT-IN AND NOTHING SELECTS IT AUTOMATICALLY.** A board that is both
/// in its bootloader and wired to a probe may be TWO DIFFERENT BOARDS -- the probe on one, another
/// in BOOTSEL -- and there is nothing readable that says otherwise. Choosing for the reader in
/// that state is the same mistake as picking between two mounted volumes, and its wrong outcome is
/// the same: somebody else's board takes the program and every layer reports success.
///
/// # Errors
/// An unknown value, or `probe` on a board with no probe route -- worded apart, because "you typed
/// something I do not know" and "this board cannot do that" send a reader to different places.
pub fn route_for(row: &Programming, via: Option<&str>) -> Result<Programmer, String> {
    match via.map(str::trim) {
        None | Some("") => Ok(row.programmer),
        Some("volume") => Ok(row.programmer),
        Some("probe") => row.alternate.ok_or_else(|| {
            format!(
                "{} has no probe route in this build, so --via probe cannot be honored.\n\n\
It is written by {}. A route that does not exist is refused here rather than attempted,\n\
because a probe attached to this board would connect and then fail at the chip.",
                row.board,
                row.programmer.description()
            )
        }),
        Some(other) => {
            let mut message = format!("--via takes `probe` or `volume`, not `{other}`.

");
            message.push_str(
                "  volume   copy the image to the bootloader drive. Needs no probe, and
",
            );
            message.push_str("           CANNOT read the flash back to check it.
");
            message.push_str(
                "  probe    write over an attached SWD probe, which reads every byte back
",
            );
            message.push_str("           and compares it.

");
            message.push_str("Omitting --via takes the board's default route.");
            Err(message)
        }
    }
}
/// The mechanism for `board_id`, or the message explaining why there is none.
///
/// # Errors
/// An unknown board id, or one no mechanism covers. The two read differently on purpose.
pub fn programmer_for(board_id: &str) -> Result<&'static Programming, String> {
    catalog::resolve(board_id).map_err(|error| format!("lamella flash: {error}\n"))?;
    PROGRAMMING
        .iter()
        .find(|row| row.board == board_id)
        .ok_or_else(|| cannot_write(board_id))
}
/// The chip family a UF2 for `board_id` must declare, when its mechanism uses one.
///
/// **THE FAMILY BELONGS TO THE CHIP AND SO IT COMES FROM THE BOARD.** A `--format uf2` on the
/// command line cannot supply it, and a UF2 carrying the wrong one is refused by the bootloader --
/// which is the behavior worth preserving, so it is filled in from here rather than defaulted.
#[must_use]
pub fn uf2_family_for_board(board_id: &str) -> Option<u32> {
    let row = PROGRAMMING.iter().find(|row| row.board == board_id)?;
    match row.programmer {
        Programmer::Uf2Volume { family, .. } => Some(family),
        _ => None,
    }
}
/// Check that a prebuilt image belongs where this mechanism writes.
///
/// **A FILE THAT STATES AN ADDRESS IS BELIEVED ABOUT ITS OWN ADDRESS, NOT ABOUT OURS.** Intel HEX
/// carries a base, and every mechanism here writes from a fixed one; a file built for a different
/// part -- an STM32 image at `0x0800_0000`, say -- is well-formed, parses cleanly, and would be
/// written to the wrong place on a Nordic part where flash begins at zero. That is a silent bad
/// flash, so the disagreement is a refusal rather than a warning.
///
/// # Errors
/// When the artifact states a base this mechanism does not write to.
pub fn check_base(artifact: &crate::artifact::Artifact, programmer: Programmer) -> Result<(), String> {
    let expected = programmer.flash_base();
    let Some(stated) = artifact.base else {
        return Ok(());
    };
    if stated == expected {
        return Ok(());
    }
    Err(format!(
        "this image states it belongs at {stated:#010x}, and this board is written from\n\
{expected:#010x}. It was almost certainly built for a different part -- writing it here\n\
would put the right bytes in the wrong place, which a board reports as nothing at all."
    ))
}
/// Write `image` to the board through `programmer`.
/// Write `image` through `programmer`, with no restriction on which part.
///
/// The shape a front end uses when the human running it IS the permission -- somebody typing
/// `lamella flash` has already decided. A server acting for an agent wants
/// [`write_scoped`] instead.
///
/// # Errors
/// Anything that stops a write, already worded for a reader.
pub fn write(
    programmer: Programmer,
    image: &[u8],
    probe: Option<&str>,
) -> Result<lamella_flash_backend::Report, String> {
    write_with(programmer, image, probe, &lamella_flash_backend::Allow::Any)
}

/// Write `image` through `programmer`, permitting only the parts `allow` names.
///
/// **THE PERMISSION IS THE CONTRACT'S TO ENFORCE, NOT THIS FUNCTION'S**, which is why it is handed
/// straight through: it has to be checked after the part identifies itself and before anything is
/// erased, and only `lamella_flash_backend::flash` is in that position.
///
/// # Errors
/// Anything that stops a write, already worded for a reader.
pub fn write_scoped(
    programmer: Programmer,
    image: &[u8],
    selector: Option<&str>,
    allow: &lamella_flash_backend::Allow,
) -> Result<lamella_flash_backend::Report, String> {
    write_with(programmer, image, selector, allow)
}

fn write_with(
    programmer: Programmer,
    image: &[u8],
    probe: Option<&str>,
    allow: &lamella_flash_backend::Allow,
) -> Result<lamella_flash_backend::Report, String> {
    let image = lamella_flash_backend::Image { bytes: image, base: programmer.flash_base() };

    let (idcode, what) = match programmer {
        Programmer::MicrobitV1Daplink => (lamella_cmsis_dap_nrf::NRF51_IDCODE, NRF51_SETTLES),
        Programmer::MicrobitV2Daplink => (lamella_cmsis_dap_nrf::NRF52_IDCODE, NRF52_SETTLES),
        Programmer::Uf2Volume { family, base } => {
            let mut backend = crate::backends::Uf2Volume::new(probe, base, family);
            return lamella_flash_backend::flash(
                &mut backend,
                &image,
                lamella_flash_backend::VerifyPolicy::ReadBack,
                allow,
            )
            .map_err(|why| why.to_string());
        }
        Programmer::Rp2350Probe { base } => {
            let selector = match probe {
                Some(serial) if !serial.trim().is_empty() => {
                    lamella_probe::Selector::by_serial(serial.trim().to_owned())
                }
                _ => lamella_probe::Selector::from_environment(),
            };
            let session =
                lamella_probe::open(&selector).map_err(|why| describe_probe_choice(&why))?;
            let mut dap = lamella_probe_core::ArmDap::new(session.into_dap());
            let idcode = lamella_cmsis_dap_rp2350::connect(&mut dap)
                .map_err(|why| format!("connecting to the RP2350: {why:?}"))?;
            let mut backend = crate::backends::Rp2350Probe::new(dap, idcode, RP2350_SETTLES);
            return lamella_flash_backend::flash(
                &mut backend,
                &lamella_flash_backend::Image { bytes: image.bytes, base },
                lamella_flash_backend::VerifyPolicy::ReadBack,
                allow,
            )
            .map_err(|why| why.to_string());
        }
        Programmer::StlinkOnboard { family, probe_id } => {
            use lamella_probe_core::TargetAccess as _;

            let mut stlink =
                lamella_stlink::StLink::open(probe_id, probe).map_err(describe_stlink_choice)?;
            stlink
                .connect()
                .map_err(|why| format!("entering SWD through the board's ST-LINK: {why}"))?;
            stlink
                .init_mem()
                .map_err(|why| format!("opening memory access through the board's ST-LINK: {why}"))?;
            match family {
                StFamily::L0 => {
                    let mut backend = crate::backends::Stm32L0Probe::new(stlink);
                    return lamella_flash_backend::flash(
                        &mut backend,
                        &image,
                        lamella_flash_backend::VerifyPolicy::ReadBack,
                        allow,
                    )
                    .map_err(|why| why.to_string());
                }
            }
        }
    };

    let (vid, pid) = programmer
        .usb_identity()
        .ok_or_else(|| "this mechanism has no probe to open".to_owned())?;
    let serial = lamella_probe::resolve_serial(vid, pid, probe).map_err(|why| format!("{why}"))?;
    let session = lamella_probe::open(&lamella_probe::Selector::by_serial(&serial))
        .map_err(|why| format!("{why}"))?;
    let target = lamella_probe_core::ArmDap::new(session.into_dap());
    let mut backend = crate::backends::MicrobitDaplink::new(target, idcode, what);

    lamella_flash_backend::flash(
        &mut backend,
        &image,
        lamella_flash_backend::VerifyPolicy::ReadBack,
        allow,
    )
    .map_err(|why| why.to_string())
}
/// What an nRF51's debug-port id settles, and what it does not.
///
/// `0x0bb11477` is the GENERIC Cortex-M0 SW-DP id: an STM32F0 answers the same, so this separates a
/// micro:bit v1 from a v2 -- the confusion that erases a board -- and nothing finer.
pub const NRF51_SETTLES: &str = "a Cortex-M0 part, which separates a micro:bit v1 from a v2";
/// The same for the nRF52833's `0x2ba01477`, which it shares with STM32 M3/M4 parts.
pub const NRF52_SETTLES: &str = "a Cortex-M4 part, which separates a micro:bit v2 from a v1";
/// What the RP2350 backend's identify step settles.
///
/// **THE DEBUG-PORT ID WOULD NOT HAVE BEEN AN IDENTITY.** `0x4c013477` is family-wide -- a Pico 2,
/// a Pico 2 W and a Pimoroni Pico Plus 2 all answer it -- so the backend reads the 64-bit OTP chip
/// id instead, which is unique to the board and is the same value its bootloader publishes as a
/// USB serial.
pub const RP2350_SETTLES: &str = "this board's own OTP chip id, which no other board shares";
/// A probe-selection failure in terms that name the next move.
///
/// **THE AMBIGUOUS CASE IS THE ONE THAT MATTERS.** A route with no vendor/product filter sees every
/// attached probe, so a bench with two of them has to be told which -- and the cost of guessing is
/// that the write succeeds against the wrong board and says nothing.
/// The same guidance for the ST-LINK opener, whose error type is the core one.
///
/// **A SEPARATE FUNCTION AND NOT A CONVERSION**, because the ADVICE differs where it matters. On an
/// ambiguous bench the CMSIS-DAP message can offer the bootloader volume as a way round; a NUCLEO
/// has no volume this verb writes, so offering one would send a reader after something that is not
/// there. What is shared is the refusal itself: this will not guess, because writing the wrong
/// board succeeds and reports success.
fn describe_stlink_choice(error: lamella_probe_core::ProbeError) -> String {
    match error {
        lamella_probe_core::ProbeError::Ambiguous(names) => {
            let mut message =
                String::from("more than one ST-LINK of this generation is attached, and they are
");
            message.push_str("indistinguishable until one is named:
");
            for name in &names {
                message.push_str(&format!("  {name}
"));
            }
            message.push_str("
Name one with --probe <serial>, or set LAMELLA_PROBE_SERIAL.
");
            message.push_str("`lamella devices` lists what is attached.

");
            message.push_str("This will not guess -- writing the wrong board succeeds and ");
            message.push_str("reports success.");
            message
        }
        other => format!("opening the board's ST-LINK: {other}"),
    }
}

pub fn describe_probe_choice(error: &lamella_probe::ProbeError) -> String {
    match error {
        lamella_probe::ProbeError::Ambiguous(names) => {
            let mut message = String::from(
                "more than one probe is attached and this route has no way to tell which is on 
",
            );
            message.push_str("your board:
");
            for name in names {
                message.push_str(&format!("  {name}
"));
            }
            message.push_str("
Name one with --probe <serial>, or set LAMELLA_PROBE_SERIAL.
");
            message.push_str("`lamella devices` lists what is attached.

");
            message.push_str("This will not guess -- writing the wrong board succeeds and ");
            message.push_str("reports success.");
            message
        }
        lamella_probe::ProbeError::NotFound => {
            let mut message = String::from("no probe is attached, or the one that is reports no ");
            message.push_str("serial number and so cannot
be named. `lamella devices` lists what ");
            message.push_str("is attached.

");
            message.push_str("To flash this board without a probe, drop the image on its ");
            message.push_str("bootloader volume instead --
that is what this verb does by ");
            message.push_str("default.");
            message
        }
        other => format!("{other:?}"),
    }
}
/// Whether `image` is already a UF2, by the magic every block starts with.
///
/// **A UF2 WRAPPED TWICE IS A FILE THE BOOTLOADER IGNORES**, and it fails the way this whole route
/// fails: silently, with the drive still mounted and nothing said.
pub fn is_uf2(image: &[u8]) -> bool {
    image.len() >= 4
        && u32::from_le_bytes([image[0], image[1], image[2], image[3]])
            == lamella_flash_format::uf2::MAGIC_START0
}
/// A UF2 block, for reporting how many crossed.
pub const UF2_BLOCK: usize = 512;
/// What to print for a board this build cannot write.
///
/// **IT NAMES WHAT IS MISSING RATHER THAN REPORTING A CAPABILITY GAP.** The reader's question is
/// "can I use my board", and the honest answer distinguishes a board nobody has taught this tool
/// about from one that cannot work -- they are completely different waits.
pub fn cannot_write(board: &str) -> String {
    let mut text = format!("lamella flash: this build cannot write {board}.\n\n");
    text.push_str("it can write:\n");
    for row in PROGRAMMING {
        text.push_str(&format!("  {:<16} {}\n", row.board, row.programmer.description()));
    }
    text.push_str(
        "\nthat list is short because how a board is PROGRAMMED is not yet stated in any board \
         file --\nthe board files declare how a running board is TALKED TO, which is a different \
         fact. Every\nmechanism here has to be added by hand until it is.\n\n\
         `lamella build <file> --board ",
    );
    text.push_str(board);
    text.push_str("` still builds and measures an image for it.\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boards_own_debugger_is_found_without_asking_and_an_external_probe_is_not_a_rival() {
        for onboard in [Programmer::MicrobitV1Daplink, Programmer::MicrobitV2Daplink] {
            assert!(
                onboard.usb_identity().is_some(),
                "an on-board debugger resolves within its own vendor/product, so nothing else                  attached can compete with it"
            );
        }
        assert_eq!(
            Programmer::Rp2350Probe { base: RP2_XIP_BASE }.usb_identity(),
            None,
            "a Pico's probe is external by definition; refusing several is the same policy with              nothing to filter on"
        );
    }

    #[test]
    fn the_selector_that_does_not_belong_to_the_route_is_refused() {
        let volume = Programmer::Uf2Volume { family: RP2350_UF2_FAMILY, base: RP2_XIP_BASE };
        let probe = Programmer::Rp2350Probe { base: RP2_XIP_BASE };

        let error = selector_for(volume, Some("E6614103"), None).expect_err("a drive is not a probe");
        assert!(error.contains("--volume"), "and it names the option that IS right: {error}");

        let error = selector_for(probe, None, Some("D:")).expect_err("a probe is not a drive");
        assert!(error.contains("--probe"), "and it names the option that IS right: {error}");

        assert_eq!(
            selector_for(volume, None, Some("D:")).expect("a drive on a volume route"),
            Some("D:".to_owned())
        );
        assert_eq!(
            selector_for(probe, Some("E6614103"), None).expect("a serial on a probe route"),
            Some("E6614103".to_owned())
        );
        assert_eq!(selector_for(volume, None, None).expect("neither is fine"), None);
    }

    #[test]
    fn a_board_keeps_its_volume_route_unless_a_probe_is_asked_for() {
        let pico = PROGRAMMING.iter().find(|r| r.board == "rpi-pico2").expect("pico2 is listed");
        assert!(
            matches!(route_for(pico, None), Ok(Programmer::Uf2Volume { .. })),
            "no --via means the route needing no probe"
        );
        assert!(
            matches!(route_for(pico, Some("volume")), Ok(Programmer::Uf2Volume { .. })),
            "and asking for it by name is the same route"
        );
        assert!(
            matches!(route_for(pico, Some("probe")), Ok(Programmer::Rp2350Probe { .. })),
            "asking for a probe selects one"
        );
    }

    #[test]
    fn a_probe_route_that_does_not_exist_is_refused_rather_than_ignored() {
        for board in ["micro-bit-v1", "rpi-pico"] {
            let row = PROGRAMMING.iter().find(|r| r.board == board).expect("listed");
            let error = route_for(row, Some("probe")).expect_err("no probe route on this board");
            assert!(error.contains(board), "the message names the board: {error}");
            assert!(
                error.contains("--via probe cannot be honored"),
                "and says the request was refused: {error}"
            );
        }
    }

    #[test]
    fn an_unknown_route_explains_the_two_that_exist() {
        let row = PROGRAMMING.iter().find(|r| r.board == "rpi-pico2").expect("listed");
        let error = route_for(row, Some("swd")).expect_err("not a route");
        assert!(error.contains("probe") && error.contains("volume"), "both named: {error}");
        assert!(
            error.contains("CANNOT read the flash back"),
            "and the difference that matters is stated, unmangled: {error}"
        );
    }

    #[test]
    fn the_required_artifact_follows_the_route_not_the_board() {
        let row = PROGRAMMING.iter().find(|r| r.board == "rpi-pico2").expect("listed");
        let volume = route_for(row, Some("volume")).expect("volume route");
        let probe = route_for(row, Some("probe")).expect("probe route");
        assert!(
            matches!(volume.required_format(), Some(crate::artifact::Format::Uf2 { .. })),
            "a bootloader volume takes a UF2"
        );
        assert_eq!(probe.required_format(), None, "a probe takes raw bytes");
        assert_eq!(volume.flash_base(), probe.flash_base(), "the same chip, the same address");
    }

    #[test]
    fn a_uf2_is_recognized_so_it_is_never_wrapped_a_second_time() {
        let flat = [0xAAu8; 32];
        assert!(!is_uf2(&flat), "a flat image is not a UF2");
        let wrapped = crate::artifact::Format::Uf2 { family: RP2350_UF2_FAMILY }
            .render(&flat, RP2_XIP_BASE);
        assert!(is_uf2(&wrapped), "and the wrapper produces one this recognizes");
        assert!(!is_uf2(&[]), "an empty image is not a UF2 and must not panic");
        assert!(!is_uf2(&[0x55, 0x46]), "nor is anything shorter than the magic");
    }

    #[test]
    fn the_volume_mechanism_declares_that_it_cannot_read_back() {
        use lamella_flash_backend::FlashBackend as _;
        let mut backend = crate::backends::Uf2Volume::new(None, RP2_XIP_BASE, RP2350_UF2_FAMILY);
        let image = lamella_flash_backend::Image { bytes: &[0u8; 4], base: RP2_XIP_BASE };
        assert!(
            backend.read_back(&image).is_none(),
            "a bootloader volume unmounts; there is no path back to the programmed bytes"
        );
    }

    #[test]
    fn an_unstamped_rp2350_image_is_refused_by_name() {
        let plain = vec![0u8; 512];
        let error = check_rp2350_stamp(&plain, Some("rp2350")).expect_err("no PICOBIN block");
        assert!(error.contains("PICOBIN"), "it names what is missing: {error}");
        assert!(error.contains("no symptom"), "and why nothing will report it: {error}");
    }

    #[test]
    fn a_stamped_rp2350_image_passes_wherever_the_block_sits() {
        for offset in [0x40usize, 0x800] {
            let mut image = vec![0u8; PICOBIN_SCAN_BYTES];
            image[offset..offset + 4].copy_from_slice(&PICOBIN_BLOCK_START.to_le_bytes());
            assert!(
                check_rp2350_stamp(&image, Some("rp2350")).is_ok(),
                "a block at {offset:#x} is within the window the bootrom scans"
            );
        }
    }

    #[test]
    fn the_stamp_guard_does_not_fire_on_other_parts() {
        let plain = vec![0u8; 512];
        for target in ["microbit", "nrf52833", "rp2040"] {
            assert!(
                check_rp2350_stamp(&plain, Some(target)).is_ok(),
                "{target} has no PICOBIN block and does not need one"
            );
        }
    }

    #[test]
    fn the_stamp_guard_does_not_fire_on_a_board_that_names_no_target() {
        let plain = vec![0u8; 512];
        assert!(
            check_rp2350_stamp(&plain, None).is_ok(),
            "a board with no ahead-of-time target is not an RP2350"
        );
    }

    #[test]
    fn a_short_image_is_scanned_rather_than_panicking() {
        let stub = vec![0u8; 22];
        assert!(check_rp2350_stamp(&stub, Some("rp2350")).is_err(), "short and unstamped is still unstamped");
    }

    #[test]
    fn the_image_our_own_builder_emits_passes_the_guard() {
        let image = lamella_aot::build::rp2350_boot_image(0, &[0x00, 0xBF, 0x00, 0xBF]);
        assert!(
            check_rp2350_stamp(&image, Some("rp2350")).is_ok(),
            "our own RP2350 boot image must carry the block this guard looks for"
        );
        assert!(image.len() < PICOBIN_SCAN_BYTES, "and it is shorter than the scanned window");
    }

    #[test]
    fn every_programmable_board_resolves_and_names_a_target_the_backend_knows() {
        for row in PROGRAMMING {
            assert!(
                catalog::load_board(row.board).is_some(),
                "{}: not a board in the catalog -- `lamella boards` does not list it",
                row.board
            );
            if let Some(target) = row.aot_target {
                assert!(
                    lamella_aot::build::CORTEX_M_TARGETS.contains(&target),
                    "{}: the backend does not know a chip called {:?}; it knows {:?}",
                    row.board,
                    target,
                    lamella_aot::build::CORTEX_M_TARGETS
                );
            }
        }
        assert!(!PROGRAMMING.is_empty(), "an empty table would pass every assertion above");
    }

    #[test]
    fn a_uf2_board_names_its_chip_family_and_a_probe_board_names_none() {
        assert_eq!(uf2_family_for_board("rpi-pico2"), Some(RP2350_UF2_FAMILY));
        assert_eq!(uf2_family_for_board("rpi-pico2-w"), Some(RP2350_UF2_FAMILY));
        assert_eq!(uf2_family_for_board("rpi-pico"), Some(RP2040_UF2_FAMILY));
        assert_ne!(
            RP2350_UF2_FAMILY, RP2040_UF2_FAMILY,
            "the two generations must not share a family, or each would accept the other's image"
        );
        assert_eq!(uf2_family_for_board("micro-bit-v2"), None, "written over a probe, not a volume");
    }

    #[test]
    fn no_board_appears_twice() {
        let mut seen: Vec<&str> = PROGRAMMING.iter().map(|row| row.board).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "a board is listed twice: {seen:?}");
    }

}
