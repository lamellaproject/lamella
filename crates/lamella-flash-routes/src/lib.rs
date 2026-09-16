//! Which mechanism writes which board, and the backends that drive them.

pub mod artifact;
pub mod backends;
pub mod bootsel;
pub mod contracts;
pub mod dfu;
pub mod identity;
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
    /// An RP2040 over a general-purpose SWD probe, by the chip's own bootrom flash API.
    ///
    /// **A SEPARATE VARIANT FROM THE RP2350's, for the reason the two micro:bit variants are
    /// separate: the parts are not interchangeable and the write path checks which one answered
    /// before it erases.** The bootroms differ in the magic that identifies them, in the layout of
    /// the table their functions are found through, and -- the one that destroys a board -- in
    /// whether the erase and program calls take an address in the execute-in-place window or an
    /// offset from the start of flash. One variant covering both would have to pick.
    Rp2040Probe {
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
        /// **A BOARD FACT AND NOT A FAMILY ONE.** An STM32L0 NUCLEO may be fitted with a V2-1
        /// (`0x374b`) while a U5A5 carries a V3 (`0x374e`), so a route that assumed one generation
        /// per family would open the wrong device or none.
        probe_id: u16,
    },
    /// The EDBG soldered to an Xplained board, driving the part's own flash controller.
    ///
    /// **THE BOARD'S OWN MECHANISM, SO IT IS A DEFAULT AND NOT AN ALTERNATE** -- the same rule every
    /// other row follows. Somebody holding an Xplained kit already has the debugger, because it is
    /// on the board.
    EdbgOnboard {
        /// Which controller to drive. A key into routines, not a mechanism.
        family: SamFamily,
        /// Which EDBG product id this kit reports, from `bsp/<board>/board.toml`'s `usb_pid`.
        ///
        /// **A BOARD FACT AND NOT A FAMILY ONE**, exactly as `probe_id` is on
        /// [`Programmer::StlinkOnboard`]: two SAM D21 kits answer `0x2169` and three Xplained Pro
        /// kits answer `0x2111`, so the pair narrows to a KIT family and never to a board. The
        /// serial rung below it is what settles which board.
        probe_id: u16,
    },
    /// An external SWD probe, driving a SAM part's own flash controller.
    ///
    /// **THE DEFAULT ON A BOARD THAT HAS NO DEBUGGER AT ALL, WHICH IS NOT AN EXCEPTION TO THE
    /// DEFAULT RULE BUT THE RULE WITH NOTHING TO CHOOSE FROM.** Every other route here prefers
    /// hardware the owner already has; an Arduino Due's programming port is a serial bridge to the
    /// part's boot ROM and nothing on the board speaks SWD. So there is no cheaper route to prefer,
    /// and this one is the board's own -- reached through the debug header, with a probe the owner
    /// supplies.
    ///
    /// It answers `None` for [`Programmer::usb_identity`] for the same reason a Pico's probe route
    /// does: every candidate is a separate piece of hardware that could be wired to anything, so
    /// several of them IS ambiguous and gets refused rather than guessed at.
    SamExternalProbe {
        /// Which controller to drive. A key into routines, not a mechanism.
        family: SamFamily,
    },
    /// An external SWD probe, driving an STM32 part's own flash controller.
    ///
    /// The route for an STM32 board with no debugger soldered to it -- an Arduino GIGA R1 WiFi or a
    /// Portenta H7 -- reached through its debug header with a probe the owner supplies: a CMSIS-DAP
    /// probe, or an ST-LINK. It drives the same family plan as [`Programmer::StlinkOnboard`], so the
    /// flash sequence is the same and only the probe differs. A named serial selects the probe, and
    /// the USB identity of the device carrying it decides which kind it is -- see
    /// [`external_probe_for`]. With no serial named, every attached CMSIS-DAP probe is a candidate
    /// and several are refused rather than guessed between, which is why it answers `None` for
    /// [`Programmer::usb_identity`], as [`Programmer::SamExternalProbe`] does.
    StExternalProbe {
        /// Which family's flash plan to drive.
        family: StFamily,
    },
    /// An STM32 board's own system bootloader, over USB DFU, with no probe attached.
    ///
    /// **THE BOARD's OWN MECHANISM, SO A DEFAULT AND NOT AN ALTERNATE.** The bootloader is in the
    /// part's ROM, so reaching it takes no probe: a USB connection to the part's DFU pins and a reset
    /// with its boot pin set. `--via probe` still means an external probe, and on such a board that
    /// is the alternate.
    ///
    /// It answers `None` for [`Programmer::usb_identity`], as the volume route does: no probe is
    /// involved, and the bootloader is chosen among the attached ones by
    /// [`crate::dfu::choose_device`].
    StDfu {
        /// Which family's plan the addresses come from. Its parts' bootloader is
        /// [`StFamily::system_bootloader`].
        family: StFamily,
    },
}

/// A Microchip SAM family, as far as flashing is concerned.
///
/// **IT GROWS WITH ROUTES, NEVER WITH DRIVERS**, on the same rule [`StFamily`] follows:
/// `lamella-cmsis-dap-sam` carries EEFC and FLASHCALW routines for families that have no variant
/// here, and that gap is the design. A variant added ahead of a route contract would let a caller
/// name a path nothing implements.
///
/// **THE TWO HERE SHARE AN IDENTITY MECHANISM AND THE OTHERS DO NOT**, which is why they arrived
/// together: both answer the DSU's `DID` and report their geometry through `NVMCTRL_PARAM`. An EEFC
/// part is identified through `CHIPID` instead, and a SAM4L through its own parameter block, so
/// those need an identify of their own rather than another row in a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamFamily {
    /// SAM D10, D11, D21, and the SAM W25 module's D21. NVMCTRL, erased by ROW of four pages.
    Samd21,
    /// SAM D5x and E5x. NVMCTRL, erased by 8 KiB block, with its command register where the D21
    /// puts its configuration register.
    ///
    /// **THIS DOES NOT COVER THE SAM E51**, whose `SERIES` is `0x1` where this line's guard requires
    /// `0x4`, and that is not a gap in the guard.
    Same54,
    /// SAM4E and SAM4N: one EEFC controller fronting ONE plane, 512-byte pages, erased eight pages
    /// at a time.
    ///
    /// **SINGLE-PLANE PARTS ONLY, AND THAT IS THE WHOLE REASON THIS IS NOT CALLED `Sam4s`.** The
    /// same `Sam4sFlash` routines drive the SAM4S, but an ATSAM4SD32 is TWO planes with two
    /// controllers, and which controller fronts which address window is decided by a `GPNVM2` swap
    /// bit rather than by the address. Getting that wrong fills one plane's write latch and programs
    /// the other -- so a variant that covered both would have to choose a controller it cannot
    /// choose correctly from an address alone.
    ///
    /// Driven on a SAM4E and a SAM4N: erase, write, verify, restore, dumps hashed.
    Sam4Eefc,
    /// SAM4L: FLASHCALW, a controller from the AVR32 UC3 line with a PicoCache in front of it.
    ///
    /// **IT SHARES THE SAM4 NAME AND NOT ONE CONSTANT WITH THE EEFC**, which is why it is a variant
    /// rather than another part on [`SamFamily::Sam4Eefc`]: the key is `0xA5` where an EEFC's is
    /// `0x5A`, the command error sits at the bit an EEFC uses for a programming failure, and the
    /// array is mapped at zero rather than at `0x00400000`.
    ///
    /// Driven on an ATSAM4LC8C: a page erased, programmed and verified word for word in two rounds
    /// -- the second is the one that carries the evidence -- and a 512 KB dump hashed identical
    /// before and after.
    Sam4l,
    /// SAM3X and SAM3A: an EEFC of the SAM4S's shape with 256-byte pages, TWO 256 KB planes behind
    /// two controllers -- and **no page-erase command at all**.
    ///
    /// **THE COMMAND SET JUMPS `EA` 0x05 STRAIGHT TO `SLB` 0x08**, so the only erase below a whole
    /// plane is the one folded into `EWP`, which erases the page it is about to write. That makes
    /// "always pre-erase" wrong here in a way no data descriptor could carry: a pre-erase pass
    /// would erase every page twice, and the only bulk erase available takes a WHOLE PLANE and so
    /// destroys flash the image does not cover.
    ///
    /// Driven on an ATSAM3X8E.
    Sam3x,
    /// The dual-plane SAM4S: an ATSAM4SD16 or ATSAM4SD32, whose flash is TWO planes behind TWO
    /// EEFCs.
    ///
    /// **THE SIBLING OF [`SamFamily::Sam4Eefc`] AND NOT A SUPERSET OF IT.** Every register, command
    /// and granule is the same; what differs is that an address alone does not name a controller,
    /// because `GPNVM2` swaps which plane sits in which window. That is not a constant a data
    /// descriptor could carry -- it is a fuse, read from the part -- so it is a variant, and the
    /// single-plane one stays single-plane by name.
    ///
    /// Driven on an ATSAM4SD32C, dual-plane and `GPNVM2` paths included.
    Sam4sDual,
}

/// Which register a SAM route reads to find out what it is pointed at.
///
/// **IT IS A PROPERTY OF THE FAMILY AND NOT OF THE BOARD**, and stating it once is what stops the
/// wrong reader being pointed at a part that has no such register: a SAM4 has no DSU, a SAM3X's
/// `CHIPID` is not a SAM4's, and each wrong address decodes to SOMETHING.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamIdentity {
    /// The DSU's `DID`, which the NVMCTRL families answer.
    Dsu,
    /// The SAM4 `CHIPID` pair, accepted for these CSP family names.
    ///
    /// **A LIST, BECAUSE THAT TABLE SPANS FAMILIES THAT DO NOT SHARE A CONTROLLER.**
    /// `lamella_cmsis_dap_sam::sam4_identify` will name a SAM4L to a caller driving an EEFC, and
    /// its own table carries a warning saying so. Naming the families a route drives is what turns
    /// that warning into a refusal.
    Sam4Chipid(&'static [&'static str]),
    /// The SAM3X / SAM3A `CHIPID`, which is at a different address and carries its member in the
    /// `CIDR` alone.
    Sam3xChipid,
}

impl SamFamily {
    /// The controller this family's routines drive, named for a refusal.
    pub fn controller(self) -> &'static str {
        match self {
            SamFamily::Samd21 => "SAM D10/D11/D21 NVMCTRL",
            SamFamily::Same54 => "SAM D5x/E5x NVMCTRL",
            SamFamily::Sam4Eefc => "single-plane SAM4 EEFC",
            SamFamily::Sam4l => "SAM4L FLASHCALW",
            SamFamily::Sam3x => "SAM3X/A EEFC",
            SamFamily::Sam4sDual => "dual-plane SAM4S EEFC",
        }
    }

    /// Where this family maps its flash array, which is the address an image for it is written
    /// from.
    ///
    /// **ONE STATEMENT OF IT.** [`Programmer::flash_base`] answers this question for a route and
    /// [`crate::backends::SamProbe`] answers it for a backend, and `lamella_flash_backend::flash`
    /// compares the two before it erases anything -- so if they disagree the write refuses as
    /// `WrongBase` and nothing is erased. Both sides call this, which is what keeps them from
    /// disagreeing.
    ///
    /// **THE ANSWER IS NOT "SAM", IT IS PER FAMILY**: the NVMCTRL parts and the SAM4L boot from
    /// zero and the EEFC parts do not.
    pub fn flash_base(self) -> u32 {
        match self {
            SamFamily::Samd21 | SamFamily::Same54 => lamella_cmsis_dap_sam::SAM_NVMCTRL_FLASH_BASE,
            SamFamily::Sam4Eefc => lamella_cmsis_dap_sam::SAM4E_FLASH_BASE,
            SamFamily::Sam4l => lamella_cmsis_dap_sam::SAM4L_FLASH_BASE,
            SamFamily::Sam3x => lamella_cmsis_dap_sam::SAM3X_FLASH0_BASE,
            SamFamily::Sam4sDual => lamella_cmsis_dap_sam::SAM4S_FLASH0_BASE,
        }
    }

    /// Which register names the part in front of this route.
    ///
    /// **THREE MECHANISMS, AND A ROUTE HAS TO SAY WHICH ONE IT SPEAKS.** A SAM D21 or E5x answers a
    /// DSU; a SAM4 has no DSU at all and answers `CHIPID` at `0x400E_0740`; a SAM3X answers
    /// `CHIPID` too, 0x200 further up. Reading the wrong one decodes whatever happens to sit at
    /// that address on that part and then decides whether to erase flash on what it found.
    pub fn identity_register(self) -> SamIdentity {
        match self {
            SamFamily::Samd21 | SamFamily::Same54 => SamIdentity::Dsu,
            SamFamily::Sam4Eefc => SamIdentity::Sam4Chipid(&["sam4e", "sam4n", "sam4s"]),
            SamFamily::Sam4l => SamIdentity::Sam4Chipid(&["sam4l"]),
            SamFamily::Sam3x => SamIdentity::Sam3xChipid,
            SamFamily::Sam4sDual => SamIdentity::Sam4Chipid(&["sam4s"]),
        }
    }

    /// What a DSU `DID` reading settles, said plainly enough that a caller cannot mistake it for
    /// more.
    ///
    /// **IT NAMES A CONTROLLER, NOT A BOARD.** Every SAM D21 on a bench answers the same processor,
    /// family and series fields, so this settles which flash routines apply and settles nothing
    /// about which of them is on the wire.
    pub fn what(self) -> &'static str {
        match self {
            SamFamily::Samd21 => {
                "a SAM D10/D11/D21 -- the controller, which every part in that line answers, not this board"
            }
            SamFamily::Same54 => {
                "a SAM D5x/E5x -- the controller, which every part in that line answers, not this board"
            }
            SamFamily::Sam4Eefc => {
                "a single-plane SAM4 behind one EEFC -- the controller, not this board"
            }
            SamFamily::Sam4l => "a SAM4L behind FLASHCALW -- the controller, not this board",
            SamFamily::Sam3x => "a SAM3X/A behind two EEFCs -- the controller, not this board",
            SamFamily::Sam4sDual => {
                "a dual-plane SAM4S behind two EEFCs -- the controller, not this board"
            }
        }
    }
}

/// An STM32 family, as far as flashing is concerned.
///
/// **IT GROWS WITH BACKENDS, NEVER WITH PRIMITIVES.** `lamella-cmsis-dap-stm32` carries flash-size
/// registers and erase/program primitives for families that have no variant here, and the gap is
/// the design rather than a backlog: a family belongs here only once something implements the
/// route contract for it and has driven a part. A variant added ahead of that would let a caller
/// NAME a route that has never been run, which is the thing `alternate: None` refuses to do one
/// level up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StFamily {
    /// STM32L0. Erases to ZERO, unlike every other part this tree programs.
    ///
    /// Driven on a NUCLEO-L073RZ, a NUCLEO-L011K4 and a NUCLEO-L053R8.
    L0,
    /// STM32C0 (RM0490). 2 KB pages, a 64-bit double word, one lock.
    ///
    /// **REACHABLE FROM AN EXAMPLE AND NOT FROM A BOARD**, because no STM32C0 board has a `bsp/`
    /// entry for a row to name. It belongs here on this enum's own terms -- the family is driven
    /// and the route contract is implemented for it -- and what a caller cannot do is select it by
    /// board id until a board file exists.
    C0,
    /// STM32L4 (RM0351: L47x/L48x/L49x/L4Ax). 2 KB pages, a 64-bit double word, two banks behind
    /// ONE lock.
    ///
    /// **ITS IDENTITY REGISTER IS AT THE F4/F7 DEBUG-REGION ADDRESS, NOT ITS L0 SIBLING'S**, which
    /// is the number a family-by-name guess gets wrong.
    L4,
    /// STM32H7 (RM0399: H745/H747/H755/H757). 128 KB sectors, a 32-byte flash word, and **two
    /// independently locked banks** -- a bank-2 address driven through bank 1's registers does
    /// nothing and reports success.
    ///
    /// Driven on a NUCLEO-H755ZI-Q: 2 MB backed up, written, restored, whole-flash hash identical.
    H7,
    /// STM32U5 (RM0456). 8 KB pages, a 128-bit quad-word, two banks behind ONE lock.
    ///
    /// Driven on a NUCLEO-U5A5ZJ-Q: 4 MB restored and hash-verified against the backup taken first.
    U5,
    /// STM32F7 (RM0385: F74x/F75x; RM0410: F76x/F77x). Sectors of 32, 128 and 256 KB in one array, a
    /// 32-bit word, one lock. The sector map is read from the part, and an F76x in dual-bank mode is
    /// refused.
    F7,
}

impl StFamily {
    /// What this family is called, for a refusal that has to name it.
    pub fn name(self) -> &'static str {
        match self {
            StFamily::L0 => "STM32L0",
            StFamily::C0 => "STM32C0",
            StFamily::L4 => "STM32L4",
            StFamily::H7 => "STM32H7",
            StFamily::U5 => "STM32U5",
            StFamily::F7 => "STM32F7",
        }
    }
}
impl Programmer {
    /// What this mechanism is, for a person reading the output.
    pub fn description(self) -> &'static str {
        match self {
            Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => {
                "the board's on-board DAPLink probe, over SWD"
            }
            Programmer::Uf2Volume { .. } => "the board's bootloader volume, by copying the image",
            Programmer::Rp2350Probe { .. } | Programmer::Rp2040Probe { .. } => {
                "an SWD probe, by the chip's own bootrom flash API"
            }
            Programmer::StlinkOnboard { .. } => {
                "the ST-LINK on the board, by the part's own flash controller"
            }
            Programmer::EdbgOnboard { .. } => {
                "the EDBG on the board, by the part's own flash controller"
            }
            Programmer::SamExternalProbe { .. } | Programmer::StExternalProbe { .. } => {
                "an external SWD probe, by the part's own flash controller"
            }
            Programmer::StDfu { .. } => "the part's system bootloader, over USB DFU",
        }
    }

    /// Whether this mechanism reaches the part through a debug probe.
    ///
    /// **IT IS WHAT MAKES `--via probe` ANSWERABLE ON A BOARD WITH ONE ROUTE.** A board whose only
    /// route is ALREADY a probe must not be reported as having no probe route: that is true of the
    /// `alternate` field and false of the board, on an Arduino Due, a micro:bit and every NUCLEO.
    /// `--via` chooses BETWEEN routes; a caller asking for a probe wants to know whether it is
    /// already getting one.
    pub fn writes_over_a_probe(self) -> bool {
        match self {
            Programmer::Uf2Volume { .. } => false,
            Programmer::StDfu { .. } => false,
            Programmer::MicrobitV1Daplink
            | Programmer::MicrobitV2Daplink
            | Programmer::Rp2350Probe { .. }
            | Programmer::Rp2040Probe { .. }
            | Programmer::StlinkOnboard { .. }
            | Programmer::EdbgOnboard { .. }
            | Programmer::SamExternalProbe { .. }
            | Programmer::StExternalProbe { .. } => true,
        }
    }

    /// Whether a board reached this way can also be written by copying the image onto a bootloader
    /// volume -- the drive a part's boot ROM presents over USB.
    ///
    /// True of the RP2040 and the RP2350, whose boot ROM presents one, and of that volume route
    /// itself. False everywhere else: a board may carry a debugger whose firmware presents a drive,
    /// but that drive belongs to the debugger, and this verb does not write it.
    fn has_bootloader_volume_route(self) -> bool {
        match self {
            Programmer::Uf2Volume { .. }
            | Programmer::Rp2350Probe { .. }
            | Programmer::Rp2040Probe { .. } => true,
            Programmer::MicrobitV1Daplink
            | Programmer::MicrobitV2Daplink
            | Programmer::StlinkOnboard { .. }
            | Programmer::EdbgOnboard { .. }
            | Programmer::SamExternalProbe { .. }
            | Programmer::StExternalProbe { .. }
            | Programmer::StDfu { .. } => false,
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
            Programmer::Rp2350Probe { base } | Programmer::Rp2040Probe { base } => base,
            Programmer::StlinkOnboard { family, .. }
            | Programmer::StExternalProbe { family }
            | Programmer::StDfu { family } => family.plan().flash_base,
            Programmer::EdbgOnboard { family, .. } => family.flash_base(),
            Programmer::SamExternalProbe { family } => family.flash_base(),
        }
    }

    /// The format an image must be written in to reach this mechanism, where the mechanism decides
    /// it. A probe takes raw bytes; a bootloader volume takes a file, and which file matters.
    pub fn required_format(self) -> Option<crate::artifact::Format> {
        match self {
            Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => None,
            Programmer::Uf2Volume { family, .. } => Some(crate::artifact::Format::Uf2 { family }),
            Programmer::Rp2350Probe { .. } | Programmer::Rp2040Probe { .. } => None,
            Programmer::StlinkOnboard { .. } | Programmer::StExternalProbe { .. } => None,
            Programmer::StDfu { .. } => None,
            Programmer::EdbgOnboard { .. } | Programmer::SamExternalProbe { .. } => None,
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
            Programmer::Rp2350Probe { .. } | Programmer::Rp2040Probe { .. } => {
                format!("{} words", bytes.div_ceil(4))
            }
            Programmer::StlinkOnboard { family, .. }
            | Programmer::StExternalProbe { family }
            | Programmer::StDfu { family } => {
                let plan = family.plan();
                format!(
                    "{} {}",
                    bytes.div_ceil(plan.program_align as usize),
                    plan.unit
                )
            }
            Programmer::EdbgOnboard { .. } | Programmer::SamExternalProbe { .. } => {
                format!("{} words", bytes.div_ceil(4))
            }
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
            Programmer::Uf2Volume { .. } | Programmer::StDfu { .. } => None,
            Programmer::Rp2350Probe { .. } | Programmer::Rp2040Probe { .. } => None,
            Programmer::StlinkOnboard { probe_id, .. } => {
                Some((lamella_stlink::ST_VENDOR_ID, probe_id))
            }
            Programmer::EdbgOnboard { probe_id, .. } => {
                Some((lamella_cmsis_dap_sam::EDBG_VENDOR_ID, probe_id))
            }
            Programmer::SamExternalProbe { .. } | Programmer::StExternalProbe { .. } => None,
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
        board: "bbc-micro-bit-v1",
        aot_target: Some("microbit"),
        programmer: Programmer::MicrobitV1Daplink,
        alternate: None,
    },
    Programming {
        board: "bbc-micro-bit-v2",
        aot_target: Some("nrf52833"),
        programmer: Programmer::MicrobitV2Daplink,
        alternate: None,
    },
    Programming {
        board: "rpi-pico2",
        aot_target: Some("rp2350"),
        programmer: Programmer::Uf2Volume {
            family: RP2350_UF2_FAMILY,
            base: RP2_XIP_BASE,
        },
        alternate: Some(Programmer::Rp2350Probe { base: RP2_XIP_BASE }),
    },
    Programming {
        board: "rpi-pico2-w",
        aot_target: Some("rp2350"),
        programmer: Programmer::Uf2Volume {
            family: RP2350_UF2_FAMILY,
            base: RP2_XIP_BASE,
        },
        alternate: Some(Programmer::Rp2350Probe { base: RP2_XIP_BASE }),
    },
    Programming {
        board: "rpi-pico",
        aot_target: Some("rp2040"),
        programmer: Programmer::Uf2Volume {
            family: RP2040_UF2_FAMILY,
            base: RP2_XIP_BASE,
        },
        alternate: Some(Programmer::Rp2040Probe { base: RP2_XIP_BASE }),
    },
    Programming {
        board: "rpi-pico-w",
        aot_target: Some("rp2040"),
        programmer: Programmer::Uf2Volume {
            family: RP2040_UF2_FAMILY,
            base: RP2_XIP_BASE,
        },
        alternate: Some(Programmer::Rp2040Probe { base: RP2_XIP_BASE }),
    },
    Programming {
        board: "st-nucleo-l053r8",
        aot_target: None,
        programmer: Programmer::StlinkOnboard {
            family: StFamily::L0,
            probe_id: lamella_stlink::product_id::V2_1,
        },
        alternate: None,
    },
    Programming {
        board: "st-nucleo-l011k4",
        aot_target: None,
        programmer: Programmer::StlinkOnboard {
            family: StFamily::L0,
            probe_id: lamella_stlink::product_id::V2_1,
        },
        alternate: None,
    },
    Programming {
        board: "st-nucleo-h755zi-q",
        aot_target: None,
        programmer: Programmer::StlinkOnboard {
            family: StFamily::H7,
            probe_id: lamella_stlink::product_id::V3E,
        },
        alternate: None,
    },
    Programming {
        board: "st-nucleo-u5a5zj-q",
        aot_target: None,
        programmer: Programmer::StlinkOnboard {
            family: StFamily::U5,
            probe_id: lamella_stlink::product_id::V3E,
        },
        alternate: None,
    },
    Programming {
        board: "st-nucleo-l476rg",
        aot_target: None,
        programmer: Programmer::StlinkOnboard {
            family: StFamily::L4,
            probe_id: lamella_stlink::product_id::V2_1,
        },
        alternate: None,
    },
    Programming {
        board: "st-stm32f746g-disco",
        aot_target: None,
        programmer: Programmer::StlinkOnboard {
            family: StFamily::F7,
            probe_id: lamella_stlink::product_id::V2_1,
        },
        alternate: None,
    },
    Programming {
        board: "st-stm32f769i-disco",
        aot_target: None,
        programmer: Programmer::StlinkOnboard {
            family: StFamily::F7,
            probe_id: lamella_stlink::product_id::V2_1,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-samd21-xpro",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Samd21,
            probe_id: 0x2169,
        },
        alternate: None,
    },
    Programming {
        board: "arduino-zero",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Samd21,
            probe_id: 0x2157,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-samd21-cnano",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Samd21,
            probe_id: 0x2175,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-samr21-xpro",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Samd21,
            probe_id: 0x2111,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-atsamd11-xpro",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Samd21,
            probe_id: 0x2111,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-atsamd10-xmini",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Samd21,
            probe_id: 0x2145,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-samw25-xpro",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Samd21,
            probe_id: 0x2111,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-same51-cnano",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Same54,
            probe_id: 0x2175,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-same54-xpro",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Same54,
            probe_id: 0x2111,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-sam4e-xpro",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Sam4Eefc,
            probe_id: 0x2111,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-sam4n-xpro",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Sam4Eefc,
            probe_id: 0x2111,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-sam4l8-xpro",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Sam4l,
            probe_id: 0x2111,
        },
        alternate: None,
    },
    Programming {
        board: "arduino-due",
        aot_target: None,
        programmer: Programmer::SamExternalProbe {
            family: SamFamily::Sam3x,
        },
        alternate: None,
    },
    Programming {
        board: "arduino-giga-r1-wifi",
        aot_target: None,
        programmer: Programmer::StExternalProbe {
            family: StFamily::H7,
        },
        alternate: None,
    },
    Programming {
        board: "arduino-portenta-h7",
        aot_target: None,
        programmer: Programmer::StExternalProbe {
            family: StFamily::H7,
        },
        alternate: None,
    },
    Programming {
        board: "microchip-sam4s-xpro",
        aot_target: None,
        programmer: Programmer::EdbgOnboard {
            family: SamFamily::Sam4sDual,
            probe_id: 0x2111,
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
    let found = window.chunks_exact(4).any(|word| {
        u32::from_le_bytes([word[0], word[1], word[2], word[3]]) == PICOBIN_BLOCK_START
    });
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
/// **A DRIVE, A PROBE AND A BOOTLOADER ARE DIFFERENT QUESTIONS AND MUST NOT SHARE ONE WORD.** One
/// `--probe` naming a serial on a probe route and a drive letter on a volume route reasons that both
/// answer "which of several identical things". They are not identical things: they live in
/// different namespaces, they are discovered by different mechanisms, and a reader who typed the
/// one the route did not want got no complaint -- the value was simply passed to something that
/// interpreted it differently. A system bootloader over USB DFU is the third: `--device` names it,
/// and since it is not a probe, `LAMELLA_PROBE_SERIAL` does not name it either.
///
/// # Errors
/// Naming a selector the chosen route does not use. Refused rather than ignored: a `--probe`
/// silently dropped on a volume route reads as "I told it which board" while nothing was told.
pub fn selector_for(
    programmer: Programmer,
    probe: Option<&str>,
    volume: Option<&str>,
    device: Option<&str>,
) -> Result<Option<String>, String> {
    match programmer {
        Programmer::Uf2Volume { .. } => match (probe, device) {
            (Some(_), _) => Err(
                "--probe names a debug probe, and this write goes to a bootloader DRIVE.\n\n\
Name the drive with --volume <name>, or ask for the probe route with --via probe."
                    .to_owned(),
            ),
            (None, Some(_)) => Err(
                "--device names a system bootloader over USB DFU, and this write goes to a bootloader \
DRIVE.\n\n\
Name the drive with --volume <name>."
                    .to_owned(),
            ),
            (None, None) => Ok(volume.map(str::to_owned)),
        },
        Programmer::StDfu { .. } => match (probe, volume) {
            (Some(_), _) => Err(
                "--probe names a debug probe, and this write goes to the part's system bootloader \
over USB DFU.\n\n\
Name the bootloader with --device <serial> when several are attached, or ask for the probe route \
with --via probe."
                    .to_owned(),
            ),
            (None, Some(_)) => Err(
                "--volume names a bootloader drive, and this write goes to the part's system bootloader \
over USB DFU, which presents none.\n\n\
Name the bootloader with --device <serial> when several are attached, or ask for the probe route \
with --via probe."
                    .to_owned(),
            ),
            (None, None) => Ok(device.map(str::to_owned)),
        },
        _ => match (volume, device) {
            (Some(_), _) => Err(
                "--volume names a bootloader drive, and this write goes over a PROBE.\n\n\
Name the probe with --probe <serial>, or ask for the drive with --via volume."
                    .to_owned(),
            ),
            (None, Some(_)) => Err(
                "--device names a system bootloader over USB DFU, and this write goes over a PROBE.\n\n\
Name the probe with --probe <serial>."
                    .to_owned(),
            ),
            (None, None) => Ok(probe.map(str::to_owned)),
        },
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
            if row.programmer.writes_over_a_probe() {
                return format!(
                    "{} has exactly one route and it is ALREADY a probe:\n{}.\n\n\
--via chooses between two routes and this board has one, so there is nothing to select.\n\
Omit it and the write goes over a probe either way.",
                    row.board,
                    row.programmer.description()
                );
            }
            format!(
                "{} has no probe route in this build, so --via probe cannot be honored.\n\n\
It is written by {}. A route that does not exist is refused here rather than attempted,\n\
because a probe attached to this board would connect and then fail at the chip.",
                row.board,
                row.programmer.description()
            )
        }),
        Some(other) => {
            let mut message = format!(
                "--via takes `probe` or `volume`, not `{other}`.

"
            );
            message.push_str(
                "  volume   copy the image to the bootloader drive. Needs no probe, and
",
            );
            message.push_str(
                "           CANNOT read the flash back to check it.
",
            );
            message.push_str(
                "  probe    write over an attached SWD probe, which reads every byte back
",
            );
            message.push_str(
                "           and compares it.

",
            );
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
pub fn check_base(
    artifact: &crate::artifact::Artifact,
    programmer: Programmer,
) -> Result<(), String> {
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

/// An image file, read into the bytes its route takes.
#[derive(Debug)]
pub struct Prepared {
    /// What the route's backend is handed: a UF2 for a bootloader volume, flat bytes for a probe.
    pub bytes: Vec<u8>,
    /// What the file held, for the line that reports the read: `998096 B of ELF`.
    pub read: String,
    /// The line that reports a wrap into a UF2, when the route needed one.
    pub wrapped: Option<String>,
}

/// Read the image file at `path` into the bytes a write through `route` puts on the board `row`
/// describes. Nothing is opened: no probe, and no drive.
///
/// A bootloader volume is handed a `.uf2` exactly as the file holds it. Every other image, on
/// every route, is read to flat bytes at an address -- a `.bin`, Intel HEX, S-records, or a linked
/// ELF flattened by physical address -- and for a bootloader volume those bytes are then wrapped
/// into a UF2 by [`wrap_for_route`].
///
/// # Errors
/// A file that cannot be read or parsed; an image that states an address this route does not
/// write from ([`check_base`]); an RP2350 image with no boot block ([`check_rp2350_stamp`]); or a
/// flat image larger than the flash the board's facts state. Each is worded for a reader.
///
pub fn prepare_image(
    path: &std::path::Path,
    row: &Programming,
    route: Programmer,
) -> Result<Prepared, String> {
    let extension = crate::artifact::classify_format(path);
    if let Some(required) = route.required_format() {
        if extension.as_deref() == Some(required.extension()) {
            let bytes =
                std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
            let read = format!("{} B of {}", bytes.len(), required.description());
            return Ok(Prepared { bytes, read, wrapped: None });
        }
    }
    let artifact = crate::artifact::read(path)?;
    check_base(&artifact, route)?;
    check_rp2350_stamp(&artifact.bytes, row.aot_target)?;
    refuse_unless_it_fits(
        row.board,
        &artifact,
        route.flash_base(),
        extension.as_deref() == Some("elf"),
    )?;
    let read = format!("{} B of {}", artifact.bytes.len(), artifact.format);
    Ok(match wrap_for_route(route, &artifact.bytes) {
        Some((bytes, line)) => Prepared { bytes, read, wrapped: Some(line) },
        None => Prepared { bytes: artifact.bytes, read, wrapped: None },
    })
}

/// `image` wrapped into a UF2 for `route`, with the line that reports the wrap -- or `None` when
/// the route takes the bytes as they stand.
///
/// A bootloader volume takes a UF2, so a flat image is wrapped at the route's flash base with the
/// family id its bootloader checks. An image that is already a UF2 is not wrapped again, and every
/// other route takes flat bytes.
///
#[must_use]
pub fn wrap_for_route(route: Programmer, image: &[u8]) -> Option<(Vec<u8>, String)> {
    let Some(crate::artifact::Format::Uf2 { family }) = route.required_format() else {
        return None;
    };
    if is_uf2(image) {
        return None;
    }
    let base = route.flash_base();
    let wrapped = crate::artifact::Format::Uf2 { family }.render(image, base);
    let line = format!("wrapped {} B as UF2 at {base:#010x} (family {family:#010x})", image.len());
    Some((wrapped, line))
}

/// Refuse a flat image larger than the flash `board`'s facts state. A board that states no flash
/// size is not refused, because there is nothing to compare against.
///
fn refuse_unless_it_fits(
    board: &str,
    artifact: &crate::artifact::Artifact,
    base: u32,
    from_elf: bool,
) -> Result<(), String> {
    let (table, part) = catalog::resolve(board)?;
    let length = i64::try_from(artifact.bytes.len()).unwrap_or(i64::MAX);
    let verdict = lamella_bsp_gen::fit::fit(&table, &part, length);
    let lamella_bsp_gen::fit::Fit::Exceeds { over } = verdict.flash_fit else {
        return Ok(());
    };
    let source = match &verdict.flash.source {
        lamella_bsp_gen::fit::BudgetSource::Board { region } => format!("board region '{region}'"),
        lamella_bsp_gen::fit::BudgetSource::Part => format!("part {}", verdict.part),
    };
    let end = u64::from(base).saturating_add(u64::try_from(artifact.bytes.len()).unwrap_or(u64::MAX));
    let mut message = format!(
        "this image is {} B, from {base:#010x} to {end:#010x}, and {board} has {} B of flash \
         ({source}) -- {over} B too many.",
        artifact.bytes.len(),
        verdict.flash.bytes
    );
    if from_elf {
        message.push_str(
            "\nA linked ELF is written from its lowest loadable address to its highest, so a segment \
             placed outside\nflash -- initialized data given a load address in RAM, most often -- \
             stretches the image across the gap.",
        );
    }
    Err(message)
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

/// What a refusal to find a system bootloader tells its reader.
const ENTER_THE_SYSTEM_BOOTLOADER: &str = "An STM32 answers over USB DFU only while its system \
bootloader runs, which it does after a reset with the part's boot pin held high; AN2606 gives the \
pattern each series follows.";

fn write_with(
    programmer: Programmer,
    image: &[u8],
    probe: Option<&str>,
    allow: &lamella_flash_backend::Allow,
) -> Result<lamella_flash_backend::Report, String> {
    let image = lamella_flash_backend::Image {
        bytes: image,
        base: programmer.flash_base(),
    };

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
            let selector = lamella_probe::Selector::named_or_environment(probe);
            let session = lamella_probe::open(&selector)
                .map_err(|why| describe_probe_choice(&why, &selector, probe, programmer))?;
            let mut dap = lamella_probe_core::ArmDap::new(session.into_dap());
            let idcode = lamella_cmsis_dap_rp2350::connect(&mut dap)
                .map_err(|why| format!("connecting to the RP2350: {why:?}"))?;
            let mut backend = crate::backends::Rp2350Probe::new(dap, idcode, RP2350_SETTLES);
            return lamella_flash_backend::flash(
                &mut backend,
                &lamella_flash_backend::Image {
                    bytes: image.bytes,
                    base,
                },
                lamella_flash_backend::VerifyPolicy::ReadBack,
                allow,
            )
            .map_err(|why| why.to_string());
        }
        Programmer::EdbgOnboard { family, .. } => {
            let (vid, pid) = programmer
                .usb_identity()
                .ok_or_else(|| "this mechanism has no probe to open".to_owned())?;
            let serial =
                lamella_probe::resolve_serial(vid, pid, probe).map_err(|why| format!("{why}"))?;
            let session = lamella_probe::open(&lamella_probe::Selector::by_serial(&serial))
                .map_err(|why| format!("{why}"))?;
            let mut dap = lamella_probe_core::ArmDap::new(session.into_dap());
            {
                use lamella_probe_core::TargetAccess as _;
                dap.connect()
                    .map_err(|why| format!("entering SWD through the board's EDBG: {why}"))?;
                dap.init_mem().map_err(|why| {
                    format!("opening memory access through the board's EDBG: {why}")
                })?;
            }
            let mut backend =
                crate::backends::SamProbe::new(dap, family, programmer.description());
            return lamella_flash_backend::flash(
                &mut backend,
                &image,
                lamella_flash_backend::VerifyPolicy::ReadBack,
                allow,
            )
            .map_err(|why| why.to_string());
        }
        Programmer::SamExternalProbe { family } => {
            use lamella_probe_core::TargetAccess as _;

            let selector = lamella_probe::Selector::named_or_environment(probe);
            let session = lamella_probe::open(&selector)
                .map_err(|why| describe_probe_choice(&why, &selector, probe, programmer))?;
            let mut dap = lamella_probe_core::ArmDap::new(session.into_dap());
            dap.connect()
                .map_err(|why| format!("entering SWD through the attached probe: {why}"))?;
            dap.init_mem()
                .map_err(|why| format!("opening memory access through the attached probe: {why}"))?;
            let mut backend =
                crate::backends::SamProbe::new(dap, family, programmer.description());
            return lamella_flash_backend::flash(
                &mut backend,
                &image,
                lamella_flash_backend::VerifyPolicy::ReadBack,
                allow,
            )
            .map_err(|why| why.to_string());
        }
        Programmer::StExternalProbe { family } => {
            use lamella_probe_core::TargetAccess as _;

            let selector = lamella_probe::Selector::named_or_environment(probe);
            if let Some(serial) = selector.serial.as_deref() {
                if let ExternalProbe::StLink { product_id } =
                    external_probe_for(serial, &attached_usb_devices())?
                {
                    let stlink = lamella_stlink::StLink::open(product_id, Some(serial))
                        .map_err(describe_stlink_choice)?;
                    return write_through_stlink(
                        stlink,
                        family,
                        &image,
                        allow,
                        "the attached ST-LINK",
                    );
                }
            }
            let session = lamella_probe::open(&selector)
                .map_err(|why| describe_probe_choice(&why, &selector, probe, programmer))?;
            let mut dap = lamella_probe_core::ArmDap::new(session.into_dap());
            if family.plan().attach_under_reset {
                attach_under_reset(&mut dap, family.plan().low_power_debug).map_err(|why| {
                    format!("attaching to the board under reset through the attached probe: {why}")
                })?;
            } else {
                dap.connect()
                    .map_err(|why| format!("entering SWD through the attached probe: {why}"))?;
            }
            dap.init_mem()
                .map_err(|why| format!("opening memory access through the attached probe: {why}"))?;
            let mut backend = crate::backends::StProbe::new(dap, family.plan());
            return lamella_flash_backend::flash(
                &mut backend,
                &image,
                lamella_flash_backend::VerifyPolicy::ReadBack,
                allow,
            )
            .map_err(|why| why.to_string());
        }
        Programmer::StDfu { family } => {
            let bootloader = family.system_bootloader().ok_or_else(|| {
                format!("{} has no system bootloader this build writes through", family.name())
            })?;
            let attached = lamella_usbbulk::enumerate_class(crate::dfu::DFU_MODE)
                .map_err(|why| format!("listing the attached USB DFU interfaces: {why}"))?;
            let device = crate::dfu::choose_device(&attached, lamella_stlink::ST_VENDOR_ID, probe)
                .map_err(|why| format!("{why}\n\n{ENTER_THE_SYSTEM_BOOTLOADER}"))?;
            let interface = lamella_usbbulk::ControlInterface::open(
                device.vendor_id,
                device.product_id,
                device.serial_number.as_deref(),
                crate::dfu::DFU_MODE,
            )
            .map_err(|why| format!("opening the bootloader's DFU interface: {why}"))?;
            let mut pipe = crate::dfu::UsbPipe::new(interface);
            let configuration = pipe
                .configuration()
                .map_err(|why| format!("reading the bootloader's configuration: {why}"))?;
            let functional = crate::dfu::FunctionalDescriptor::find(&configuration)
                .map_err(|why| why.to_string())?;
            if !functional.can_download || !functional.can_upload {
                return Err(format!(
                    "the bootloader's functional descriptor states download {} and upload {}, and a \
                     write here needs both",
                    functional.can_download, functional.can_upload
                ));
            }
            let dfu = crate::dfu::DfuSe::new(pipe, functional.transfer_size);
            let mut backend = crate::backends::StDfu::new(dfu, family.plan(), bootloader);
            return lamella_flash_backend::flash(
                &mut backend,
                &image,
                lamella_flash_backend::VerifyPolicy::ReadBack,
                allow,
            )
            .map_err(|why| why.to_string());
        }
        Programmer::Rp2040Probe { base } => {
            let selector = lamella_probe::Selector::named_or_environment(probe);
            let session = lamella_probe::open(&selector)
                .map_err(|why| describe_probe_choice(&why, &selector, probe, programmer))?;
            let mut dap = lamella_probe_core::ArmDap::new(session.into_dap());
            let idcode = lamella_cmsis_dap_rp2040::connect(&mut dap)
                .map_err(|why| format!("connecting to the RP2040: {why}"))?;
            let mut backend = crate::backends::Rp2040Probe::new(dap, idcode, RP2040_SETTLES);
            return lamella_flash_backend::flash(
                &mut backend,
                &lamella_flash_backend::Image {
                    bytes: image.bytes,
                    base,
                },
                lamella_flash_backend::VerifyPolicy::ReadBack,
                allow,
            )
            .map_err(|why| why.to_string());
        }
        Programmer::StlinkOnboard { family, probe_id } => {
            let stlink =
                lamella_stlink::StLink::open(probe_id, probe).map_err(describe_stlink_choice)?;
            return write_through_stlink(stlink, family, &image, allow, "the board's ST-LINK");
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
/// Attach to a target held in reset and leave its core halted at the reset vector, through any probe
/// that speaks [`TargetAccess`](lamella_probe_core::TargetAccess): hold reset, bring up SWD and memory
/// access, then [`lamella_stlink::attach_held_in_reset`] -- the known answer, `low_power_debug`'s
/// bits, the reset vector catch, the release and the halt.
///
/// # Errors
/// Any step the probe or the target refuses. A failure while SWD or memory access is brought up
/// leaves the core held in reset; a later failure before the release releases it.
pub fn attach_under_reset<T>(
    target: &mut T,
    low_power_debug: Option<lamella_stlink::LowPowerDebug>,
) -> Result<(), lamella_probe_core::ProbeError>
where
    T: lamella_probe_core::TargetAccess + lamella_probe_core::CoreMemory,
{
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(50);
    lamella_probe_core::TargetAccess::set_reset(target, true)?;
    std::thread::sleep(SETTLE);
    lamella_probe_core::TargetAccess::connect(target)?;
    lamella_probe_core::TargetAccess::init_mem(target)?;
    lamella_stlink::attach_held_in_reset(target, low_power_debug)
}

/// Writes `image` through an opened ST-LINK on `family`'s plan: the family's attach, memory access,
/// then the plan's erase, program and read-back. `through` names the probe in a refusal.
fn write_through_stlink(
    mut stlink: lamella_stlink::StLink,
    family: StFamily,
    image: &lamella_flash_backend::Image<'_>,
    allow: &lamella_flash_backend::Allow,
    through: &str,
) -> Result<lamella_flash_backend::Report, String> {
    use lamella_probe_core::TargetAccess as _;

    if family.plan().attach_under_reset {
        stlink
            .attach_under_reset(family.plan().low_power_debug)
            .map_err(|why| {
                format!("attaching to the board under reset through {through}: {why}")
            })?;
    } else {
        stlink
            .connect()
            .map_err(|why| format!("entering SWD through {through}: {why}"))?;
    }
    stlink
        .init_mem()
        .map_err(|why| format!("opening memory access through {through}: {why}"))?;
    let mut backend = crate::backends::StProbe::new(stlink, family.plan());
    lamella_flash_backend::flash(
        &mut backend,
        image,
        lamella_flash_backend::VerifyPolicy::ReadBack,
        allow,
    )
    .map_err(|why| why.to_string())
}

/// A USB device as an external-probe route sees it when a serial names the probe: its identity, and
/// whether it presents a CMSIS-DAP interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbDevice {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// Serial number, if the operating system reported one.
    pub serial: Option<String>,
    /// Product string, if the operating system reported one.
    pub product: Option<String>,
    /// Whether the device presents a CMSIS-DAP interface.
    pub speaks_cmsis_dap: bool,
}

/// The kind of probe that carries the serial an external-probe route was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalProbe {
    /// An ST-LINK, opened through its own protocol under this USB product id.
    StLink {
        /// The ST-LINK's USB product id.
        product_id: u16,
    },
    /// A CMSIS-DAP probe.
    CmsisDap,
    /// No attached device reports the serial.
    NotAttached,
}

/// Every attached USB device that could carry a probe serial: the CMSIS-DAP probes and the
/// vendor-class devices that are not, as `lamella devices --all-devices` lists them. Listing opens
/// nothing.
pub fn attached_usb_devices() -> Vec<UsbDevice> {
    let mut others = Vec::new();
    let probes = lamella_probe::list_reporting(&mut others);
    let listed = probes.iter().map(|probe| UsbDevice {
        vendor_id: probe.vendor_id,
        product_id: probe.product_id,
        serial: probe.serial.clone(),
        product: probe.product.clone(),
        speaks_cmsis_dap: true,
    });
    let passed_over = others.iter().map(|other| UsbDevice {
        vendor_id: other.vendor_id,
        product_id: other.product_id,
        serial: other.serial.clone(),
        product: other.product.clone(),
        speaks_cmsis_dap: false,
    });
    listed.chain(passed_over).collect()
}

/// Which kind of probe carries `serial`, decided by the USB identity of the device that reports it
/// and never by the serial's text.
///
/// An ST-LINK is recognized by ST's vendor id and an ST-LINK product id; any other device must
/// present a CMSIS-DAP interface. The serial matches whole and without regard to case, as the probe
/// ladder matches it, and a device listed through several interfaces counts once.
///
/// # Errors
/// When more than one device reports the serial, or the one that does is neither kind -- naming
/// every device it found.
pub fn external_probe_for(serial: &str, devices: &[UsbDevice]) -> Result<ExternalProbe, String> {
    let serial = serial.trim();
    let mut carrying: Vec<UsbDevice> = Vec::new();
    for device in devices {
        let reports_it = device
            .serial
            .as_deref()
            .is_some_and(|reported| reported.eq_ignore_ascii_case(serial));
        if !reports_it {
            continue;
        }
        match carrying
            .iter_mut()
            .find(|held| held.vendor_id == device.vendor_id && held.product_id == device.product_id)
        {
            Some(held) => held.speaks_cmsis_dap |= device.speaks_cmsis_dap,
            None => carrying.push(device.clone()),
        }
    }
    match carrying.as_slice() {
        [] => Ok(ExternalProbe::NotAttached),
        [device] if is_st_link(device.vendor_id, device.product_id) => {
            Ok(ExternalProbe::StLink { product_id: device.product_id })
        }
        [device] if device.speaks_cmsis_dap => Ok(ExternalProbe::CmsisDap),
        [device] => {
            let mut message = format!("the device reporting the serial `{serial}` is ");
            message.push_str(&describe_usb_device(device));
            message.push_str(", which is neither an ST-LINK nor a CMSIS-DAP probe, so this route ");
            message.push_str("cannot drive it.");
            Err(message)
        }
        several => {
            let mut message = format!(
                "the serial `{serial}` is reported by {} devices, so it names none of them:\n",
                several.len()
            );
            for device in several {
                message.push_str(&format!("  {}\n", describe_usb_device(device)));
            }
            message.push_str("Unplug the ones you do not mean, then name the probe again.");
            Err(message)
        }
    }
}

/// Whether a USB identity is an ST-LINK's: ST's vendor id and a product id of either ST-LINK
/// generation.
fn is_st_link(vendor_id: u16, product_id: u16) -> bool {
    use lamella_stlink::product_id::{V2, V2_1, V2EC, V3_FAMILY};
    vendor_id == lamella_stlink::ST_VENDOR_ID
        && ([V2, V2_1, V2EC].contains(&product_id) || V3_FAMILY.contains(&product_id))
}

/// A device in the words a refusal names it by: its vendor and product id, then its product string.
fn describe_usb_device(device: &UsbDevice) -> String {
    format!(
        "{:04x}:{:04x} {}",
        device.vendor_id,
        device.product_id,
        device.product.as_deref().unwrap_or("(no product string)")
    )
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
/// What the RP2040 backend's identify step settles, and what it cannot.
///
/// **THIS PART HAS NO OTP CHIP ID, so the RP2350's answer is not available here and saying
/// otherwise would be the more dangerous kind of wrong.** `0x0bc12477` is answered by every RP2040:
/// it separates a Pico from a Pico 2 -- the confusion that erases a board, because the two take the
/// same probe and the same connector -- and it does not separate a Pico from a Pico W, nor one Pico
/// from another. The RP2040's unique id lives in the QSPI flash device rather than the die, so
/// reading it means going through the flash chip; naming a board on a bench holding several is
/// still `--probe` and the wiring, not anything the part will tell you.
pub const RP2040_SETTLES: &str =
    "an RP2040 part, which separates a Pico from a Pico 2 and not one Pico from another";
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
            let mut message = String::from(
                "more than one ST-LINK of this generation is attached, and they are
",
            );
            message.push_str(
                "indistinguishable until one is named:
",
            );
            for name in &names {
                message.push_str(&format!(
                    "  {name}
"
                ));
            }
            message.push_str(
                "
Name one with --probe <serial>, or set LAMELLA_PROBE_SERIAL.
",
            );
            message.push_str(
                "`lamella devices` lists what is attached.

",
            );
            message.push_str("This will not guess -- writing the wrong board succeeds and ");
            message.push_str("reports success.");
            message
        }
        other => format!("opening the board's ST-LINK: {other}"),
    }
}

/// A probe-selection failure, worded for the person holding the board.
///
/// The interesting case is the refusal: several probes are attached, the route has no way to tell
/// which is wired to the board that was named, and it stops. So the message lists every candidate
/// and says how to name one, because the remedy is to pick and the reader should not have to go
/// and look them up.
///
/// `selector` is what the route asked for and `named` is the `--probe` value, if one was given, so a
/// serial that matched nothing is refused by that serial and by where it came from. `programmer`
/// decides whether a bootloader volume is offered as the way round: only a board that has one is
/// sent to look for it.
#[must_use]
pub fn describe_probe_choice(
    error: &lamella_probe::ProbeError,
    selector: &lamella_probe::Selector,
    named: Option<&str>,
    programmer: Programmer,
) -> String {
    match error {
        lamella_probe::ProbeError::Ambiguous(names) => {
            let mut message = String::from(
                "more than one probe is attached and this route has no way to tell which is on 
",
            );
            message.push_str(
                "your board:
",
            );
            for name in names {
                message.push_str(&format!(
                    "  {name}
"
                ));
            }
            message.push_str(
                "
Name one with --probe <serial>, or set LAMELLA_PROBE_SERIAL.
",
            );
            message.push_str(
                "`lamella devices` lists what is attached.

",
            );
            message.push_str("This will not guess -- writing the wrong board succeeds and ");
            message.push_str("reports success.");
            message
        }
        lamella_probe::ProbeError::NotFound => {
            let mut message = match selector.serial.as_deref() {
                Some(serial) if named.is_some_and(|given| given.trim() == serial) => {
                    format!("no attached probe has the serial `{serial}`, which --probe named.
")
                }
                Some(serial) => {
                    let mut message = format!("no attached probe has the serial `{serial}`, which ");
                    message.push_str(lamella_probe::PROBE_SERIAL_ENV);
                    message.push_str(" names.
Correct or unset it, or name the probe with --probe.
");
                    message
                }
                None => {
                    let mut message = String::from("no probe is attached, or the one that is reports ");
                    message.push_str("no serial number and so cannot be named.
");
                    message
                }
            };
            message.push_str("`lamella devices` lists every attached probe with its serial.");
            let st_route = matches!(programmer, Programmer::StExternalProbe { .. });
            if st_route && selector.serial.is_none() {
                message.push_str("\nAn ST-LINK is used only when its serial is named.");
            }
            if programmer.has_bootloader_volume_route() {
                message.push_str("

To flash this board without a probe, drop the image on its ");
                message.push_str("bootloader volume instead --
that is what this verb does by default.");
            }
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
///
/// **NO BUILD OF THIS TOOL WRITES A BOARD THAT ANOTHER BUILD REFUSES**, so the wording never
/// implies one does. A route is a row in [`PROGRAMMING`], not a cargo feature.
pub fn cannot_write(board: &str) -> String {
    let mut text = format!("lamella flash: nothing states how to write {board}.\n\n");
    text.push_str("it can write:\n");
    for row in PROGRAMMING {
        text.push_str(&format!(
            "  {:<16} {}\n",
            row.board,
            row.programmer.description()
        ));
    }
    text.push_str(
        "\nthat list is short because how a board is PROGRAMMED is not yet stated in any board \
         file --\nthe board files declare how a running board is TALKED TO, which is a different \
         fact. Every\nmechanism here has to be added by hand until it is.\n\n\
         `lamella build <file> --board ",
    );
    text.push_str(board);
    text.push_str("`\ncompiles it and measures the result against this board's budget. That\n");
    text.push_str("measurement is of the ASSEMBLY, not of a flash image -- the image is what\n");
    text.push_str("`--format` produces, and producing one needs the missing fact above.\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A linked ELF32 executable with one loadable segment per `(physical address, bytes)`.
    fn linked_elf(segments: &[(u32, &[u8])]) -> Vec<u8> {
        const HEADER: usize = 52;
        const PROGRAM_HEADER: usize = 32;
        let word = |value: usize| u32::try_from(value).expect("a 32-bit field");
        let half = |value: usize| u16::try_from(value).expect("a 16-bit field");
        let mut elf = vec![0u8; HEADER + PROGRAM_HEADER * segments.len()];
        // ELFCLASS32, ELFDATA2LSB, EV_CURRENT; then ET_EXEC for EM_ARM.
        elf[..7].copy_from_slice(&[0x7F, b'E', b'L', b'F', 1, 1, 1]);
        elf[16..18].copy_from_slice(&2u16.to_le_bytes());
        elf[18..20].copy_from_slice(&40u16.to_le_bytes());
        elf[20..24].copy_from_slice(&1u32.to_le_bytes());
        elf[28..32].copy_from_slice(&word(HEADER).to_le_bytes());
        elf[40..42].copy_from_slice(&half(HEADER).to_le_bytes());
        elf[42..44].copy_from_slice(&half(PROGRAM_HEADER).to_le_bytes());
        elf[44..46].copy_from_slice(&half(segments.len()).to_le_bytes());
        for (index, (address, bytes)) in segments.iter().enumerate() {
            let at = HEADER + PROGRAM_HEADER * index;
            let (offset, length) = (word(elf.len()), word(bytes.len()));
            // PT_LOAD, p_offset, p_vaddr, p_paddr, p_filesz, p_memsz.
            for (field, value) in
                [(0, 1), (4, offset), (8, *address), (12, *address), (16, length), (20, length)]
            {
                elf[at + field..at + field + 4].copy_from_slice(&value.to_le_bytes());
            }
            elf.extend_from_slice(bytes);
        }
        elf
    }

    /// `bytes` in a file named `name`, in a scratch directory of the calling test's own.
    fn image_file(test: &str, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let directory = std::env::temp_dir()
            .join(format!("lamella-flash-routes-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a scratch directory");
        let path = directory.join(name);
        std::fs::write(&path, bytes).expect("the image file");
        path
    }

    /// A body that carries the RP2350 boot block, so an image made of it is not refused as
    /// unstamped.
    fn stamped(length: usize) -> Vec<u8> {
        let mut body = vec![0u8; length];
        body[0x40..0x44].copy_from_slice(&PICOBIN_BLOCK_START.to_le_bytes());
        body
    }

    fn pico2() -> &'static Programming {
        programmer_for("rpi-pico2").expect("rpi-pico2 is routed")
    }

    /// A serial named with `--probe` that no attached probe carries is refused by that serial, and a
    /// board with no bootloader volume is not sent looking for one.
    #[test]
    fn a_named_probe_nothing_carries_is_refused_by_its_serial_and_offers_no_volume_it_lacks() {
        let serial = "NOSUCHPROBE0000";
        let selector = lamella_probe::Selector::by_serial(serial);
        for programmer in [
            Programmer::StExternalProbe { family: StFamily::H7 },
            Programmer::SamExternalProbe { family: SamFamily::Samd21 },
        ] {
            let text = describe_probe_choice(
                &lamella_probe::ProbeError::NotFound,
                &selector,
                Some(serial),
                programmer,
            );
            let route = programmer.description();
            assert!(text.contains(serial), "{route}: names the serial nothing carries: {text}");
            assert!(text.contains("--probe"), "{route}: says where that serial came from: {text}");
            assert!(!text.contains("no probe is attached"), "{route}: probes may be attached: {text}");
            assert!(!text.contains("volume"), "{route}: has no bootloader volume: {text}");
        }
    }

    /// A serial that came from the environment is reported as the environment's, so a stale export is
    /// what the reader goes to correct.
    #[test]
    fn a_serial_from_the_environment_is_refused_as_the_environments() {
        let selector = lamella_probe::Selector::by_serial("STALEEXPORT0000");
        let text = describe_probe_choice(
            &lamella_probe::ProbeError::NotFound,
            &selector,
            None,
            Programmer::StExternalProbe { family: StFamily::H7 },
        );
        assert!(text.contains("STALEEXPORT0000"), "names the serial: {text}");
        assert!(text.contains("LAMELLA_PROBE_SERIAL"), "names where it came from: {text}");
    }

    /// A Pico's probe route that found nothing still points at the bootloader volume, which a Pico
    /// has and which this verb writes by default.
    #[test]
    fn a_pico_probe_route_that_found_nothing_still_offers_its_volume() {
        let text = describe_probe_choice(
            &lamella_probe::ProbeError::NotFound,
            &lamella_probe::Selector::any(),
            None,
            Programmer::Rp2350Probe { base: RP2_XIP_BASE },
        );
        assert!(text.contains("bootloader volume"), "a Pico has one: {text}");
    }

    /// A device as the probe listing reports it.
    fn usb(vendor_id: u16, product_id: u16, serial: &str, speaks_cmsis_dap: bool) -> UsbDevice {
        UsbDevice {
            vendor_id,
            product_id,
            serial: Some(serial.to_owned()),
            product: Some(format!("device {vendor_id:04x}:{product_id:04x}")),
            speaks_cmsis_dap,
        }
    }

    /// The probe kind comes from the USB identity of the device carrying the serial, never from the
    /// serial's text: an ST-LINK whose serial is short is an ST-LINK, a CMSIS-DAP probe whose serial
    /// has an ST-LINK's length is a CMSIS-DAP probe, and the serial matches whole, without regard
    /// to case.
    #[test]
    fn a_named_serial_chooses_the_probe_kind_by_the_identity_of_the_device_carrying_it() {
        let v3s = lamella_stlink::product_id::V3S;
        let devices = [
            usb(lamella_stlink::ST_VENDOR_ID, v3s, "PROBE00000001", false),
            usb(0x1fc9, 0x0143, "0123456789ABCDEF01234567", true),
        ];
        let st_link = Ok(ExternalProbe::StLink { product_id: v3s });
        assert_eq!(external_probe_for("PROBE00000001", &devices), st_link);
        assert_eq!(external_probe_for("probe00000001", &devices), st_link);
        assert_eq!(
            external_probe_for("0123456789ABCDEF01234567", &devices),
            Ok(ExternalProbe::CmsisDap)
        );
        assert_eq!(external_probe_for("PROBE0000000", &devices), Ok(ExternalProbe::NotAttached));
    }

    /// Every ST-LINK generation's product id is an ST-LINK, and a device under ST's vendor id that is
    /// not one -- its USB DFU bootloader, `0xdf11` -- is refused by name rather than driven.
    #[test]
    fn every_st_link_product_id_is_an_st_link_and_no_other_st_device_is() {
        use lamella_stlink::product_id::{V2, V2_1, V2EC, V3_FAMILY};
        for product_id in [V2, V2_1, V2EC].into_iter().chain(V3_FAMILY) {
            let devices = [usb(lamella_stlink::ST_VENDOR_ID, product_id, "SERIAL1", false)];
            assert_eq!(
                external_probe_for("SERIAL1", &devices),
                Ok(ExternalProbe::StLink { product_id })
            );
        }
        let dfu = [usb(lamella_stlink::ST_VENDOR_ID, 0xdf11, "SERIAL1", false)];
        let refusal = external_probe_for("SERIAL1", &dfu).expect_err("a DFU bootloader is no probe");
        assert!(refusal.contains("0483:df11"), "names what it found: {refusal}");
    }

    /// A probe listed through its CMSIS-DAP interface and a sibling vendor interface is one device,
    /// so its serial is not refused as ambiguous.
    #[test]
    fn one_probe_listed_through_two_interfaces_is_one_device() {
        let devices = [usb(0x1fc9, 0x0143, "SERIAL2", true), usb(0x1fc9, 0x0143, "SERIAL2", false)];
        assert_eq!(external_probe_for("SERIAL2", &devices), Ok(ExternalProbe::CmsisDap));
    }

    /// A serial that two different devices report names neither, and the refusal names both.
    #[test]
    fn a_serial_two_devices_report_is_refused_naming_both() {
        let devices = [
            usb(lamella_stlink::ST_VENDOR_ID, lamella_stlink::product_id::V2, "000000000001", false),
            usb(0x2e8a, 0x000c, "000000000001", true),
        ];
        let refusal =
            external_probe_for("000000000001", &devices).expect_err("two devices, one serial");
        assert!(refusal.contains("0483:3748"), "names the first: {refusal}");
        assert!(refusal.contains("2e8a:000c"), "names the second: {refusal}");
    }

    /// A device carrying the serial that is neither an ST-LINK nor a CMSIS-DAP probe is refused by
    /// name rather than driven.
    #[test]
    fn a_serial_on_a_device_that_is_no_probe_is_refused_naming_it() {
        let devices = [usb(0x0403, 0x6001, "SERIAL3", false)];
        let refusal =
            external_probe_for("SERIAL3", &devices).expect_err("a serial bridge is no probe");
        assert!(refusal.contains("0403:6001"), "names what it found: {refusal}");
    }

    /// An STM32 external-probe route with nothing named says that an ST-LINK is taken only by its
    /// serial, since the candidates it counted were CMSIS-DAP probes.
    #[test]
    fn an_st_external_route_with_nothing_named_says_an_st_link_needs_its_serial() {
        let text = describe_probe_choice(
            &lamella_probe::ProbeError::NotFound,
            &lamella_probe::Selector::any(),
            None,
            Programmer::StExternalProbe { family: StFamily::H7 },
        );
        assert!(text.contains("ST-LINK"), "says how an ST-LINK is chosen: {text}");
    }

    /// An external-probe STM32 route is a probe route that takes the family plan's facts and filters
    /// no USB identity, because no debugger is soldered to its board.
    #[test]
    fn an_external_probe_st_route_takes_the_plans_facts_and_no_usb_filter() {
        let route = Programmer::StExternalProbe { family: StFamily::H7 };
        let onboard = Programmer::StlinkOnboard { family: StFamily::H7, probe_id: 0x374e };
        assert!(route.writes_over_a_probe(), "a probe route, so --via probe is already answered");
        assert_eq!(route.usb_identity(), None, "nothing is soldered on, so there is nothing to filter");
        assert_eq!(route.flash_base(), StFamily::H7.plan().flash_base);
        assert_eq!(route.required_format(), None, "a probe takes raw bytes");
        assert_eq!(route.units(64), onboard.units(64), "the same plan counts the same granule");
        assert!(route.description().contains("external"), "{}", route.description());
    }

    /// **EVERY FLAT FORMAT REACHES A BOOTLOADER VOLUME AS THE SAME UF2.** A `.bin`, Intel HEX,
    /// S-records and a linked ELF holding one image are wrapped once, at the flash base, with the
    /// family the bootloader checks.
    #[test]
    fn every_flat_format_reaches_a_bootloader_volume_as_the_same_uf2() {
        let code = stamped(700);
        let row = pico2();
        let volume = route_for(row, None).expect("the default route");
        let expected = crate::artifact::Format::Uf2 { family: RP2350_UF2_FAMILY }
            .render(&code, RP2_XIP_BASE);
        assert_eq!(expected.len(), 3 * UF2_BLOCK, "700 B is three blocks");
        let mut payload = Vec::new();
        for (index, block) in expected.chunks(UF2_BLOCK).enumerate() {
            let field =
                |at: usize| u32::from_le_bytes(block[at..at + 4].try_into().expect("a word"));
            let number = u32::try_from(index).expect("a small index");
            assert_eq!(
                [field(0), field(4), field(8), field(12), field(16)],
                [0x0A32_4655, 0x9E5D_5157, 0x2000, RP2_XIP_BASE + 256 * number, 256],
                "block {index}: magic, family flag, its address, and a FULL 256 B payload"
            );
            assert_eq!(
                [field(20), field(24), field(28), field(508)],
                [number, 3, RP2350_UF2_FAMILY, 0x0AB1_6F30],
                "block {index}: its index, the count, the family, the end magic"
            );
            payload.extend_from_slice(&block[32..32 + 256]);
        }
        assert_eq!(&payload[..code.len()], &code[..], "the payload is the image");
        assert!(payload[code.len()..].iter().all(|&byte| byte == 0), "then zero-filled to the page");
        let files = [
            ("image.bin", code.clone()),
            ("image.hex", crate::artifact::Format::IntelHex.render(&code, RP2_XIP_BASE)),
            ("image.s19", crate::artifact::Format::SRecord.render(&code, RP2_XIP_BASE)),
            ("image.elf", linked_elf(&[(RP2_XIP_BASE, &code)])),
        ];
        for (name, contents) in files {
            let path = image_file("flat-formats", name, &contents);
            let prepared =
                prepare_image(&path, row, volume).unwrap_or_else(|why| panic!("{name}: {why}"));
            assert_eq!(prepared.bytes, expected, "{name} becomes the UF2 of its flat image");
            assert!(prepared.read.starts_with("700 B of "), "{name}: {}", prepared.read);
            assert!(prepared.wrapped.is_some(), "{name}: the wrap is reported");
            assert!(
                wrap_for_route(volume, &prepared.bytes).is_none(),
                "{name}: and a UF2 is never wrapped a second time"
            );
        }
    }

    /// Over the probe route an ELF is its flat image, because a probe takes raw bytes.
    #[test]
    fn over_a_probe_an_elf_is_its_flat_image() {
        let code = stamped(700);
        let path = image_file("elf-probe", "image.elf", &linked_elf(&[(RP2_XIP_BASE, &code)]));
        let row = pico2();
        let probe = route_for(row, Some("probe")).expect("the probe route");
        let prepared = prepare_image(&path, row, probe).expect("an ELF is taken");
        assert_eq!(prepared.bytes, code);
        assert_eq!(prepared.wrapped, None);
    }

    /// An ELF linked for another part's address is refused, and the refusal names both addresses.
    #[test]
    fn an_elf_linked_for_another_address_is_refused_naming_both() {
        let elf = linked_elf(&[(0x0800_0000, &stamped(700))]);
        let path = image_file("elf-base", "image.elf", &elf);
        let row = pico2();
        let volume = route_for(row, None).expect("the default route");
        let error = prepare_image(&path, row, volume).expect_err("not this part's address");
        assert!(error.contains("0x08000000") && error.contains("0x10000000"), "{error}");
    }

    /// An RP2350 ELF with no boot block is refused on the volume route exactly as on the probe
    /// route.
    #[test]
    fn an_unstamped_elf_is_refused_on_both_routes() {
        let elf = linked_elf(&[(RP2_XIP_BASE, &[0u8; 700])]);
        let path = image_file("elf-stamp", "image.elf", &elf);
        let row = pico2();
        for via in [None, Some("probe")] {
            let route = route_for(row, via).expect("a route");
            let error = prepare_image(&path, row, route).expect_err("no boot block");
            assert!(error.contains("PICOBIN"), "{via:?}: {error}");
        }
    }

    /// **ONE SEGMENT PAST THE FLASH STRETCHES AN ELF ACROSS THE GAP**, so the image is refused by
    /// the board's flash size on both routes, naming where it ends and what the board holds.
    #[test]
    fn an_elf_with_a_segment_past_the_flash_is_refused_by_the_boards_flash_size() {
        let code = stamped(700);
        let data = [0xAA_u8; 16];
        let elf = linked_elf(&[(RP2_XIP_BASE, &code), (RP2_XIP_BASE + 0x40_0000, &data)]);
        let path = image_file("elf-fit", "image.elf", &elf);
        let row = pico2();
        for via in [None, Some("probe")] {
            let route = route_for(row, via).expect("a route");
            let error = prepare_image(&path, row, route).expect_err("larger than the flash");
            assert!(error.contains("0x10400010"), "{via:?}: where it ends: {error}");
            assert!(error.contains("4194304 B of flash"), "{via:?}: what the board holds: {error}");
            assert!(error.contains("load address in RAM"), "{via:?}: the likely cause: {error}");
        }
    }

    /// A `.uf2` is copied to a volume byte for byte, and refused over a probe.
    #[test]
    fn a_uf2_is_copied_to_a_volume_as_it_stands_and_refused_over_a_probe() {
        let uf2 = crate::artifact::Format::Uf2 { family: RP2350_UF2_FAMILY }
            .render(&stamped(300), RP2_XIP_BASE);
        let path = image_file("uf2", "image.uf2", &uf2);
        let row = pico2();
        let volume = route_for(row, None).expect("the default route");
        let prepared = prepare_image(&path, row, volume).expect("what a volume takes");
        assert_eq!(prepared.bytes, uf2);
        assert_eq!(prepared.wrapped, None);
        let probe = route_for(row, Some("probe")).expect("the probe route");
        let error = prepare_image(&path, row, probe).expect_err("a probe takes raw bytes");
        assert!(error.contains("bootloader volume"), "{error}");
    }

    /// **THE SIZE REFUSAL BINDS ONLY WHERE A BOARD STATES ITS FLASH**, so every routed board must.
    #[test]
    fn every_routed_board_states_the_flash_an_image_must_fit() {
        let unstated: Vec<&str> = PROGRAMMING
            .iter()
            .filter(|row| {
                let (table, part) =
                    catalog::resolve(row.board).expect("every routed board resolves");
                matches!(
                    lamella_bsp_gen::fit::fit(&table, &part, 1).flash_fit,
                    lamella_bsp_gen::fit::Fit::Unknown { .. }
                )
            })
            .map(|row| row.board)
            .collect();
        assert!(unstated.is_empty(), "these routed boards state no flash size: {unstated:?}");
    }

    #[test]
    fn a_boards_own_debugger_is_found_without_asking_and_an_external_probe_is_not_a_rival() {
        for onboard in [Programmer::MicrobitV1Daplink, Programmer::MicrobitV2Daplink] {
            assert!(
                onboard.usb_identity().is_some(),
                "an on-board debugger resolves within its own vendor/product, so nothing else \
                 attached can compete with it"
            );
        }
        assert_eq!(
            Programmer::Rp2350Probe { base: RP2_XIP_BASE }.usb_identity(),
            None,
            "a Pico's probe is external by definition; refusing several is the same policy with \
             nothing to filter on"
        );
    }

    #[test]
    fn the_selector_that_does_not_belong_to_the_route_is_refused() {
        let volume = Programmer::Uf2Volume {
            family: RP2350_UF2_FAMILY,
            base: RP2_XIP_BASE,
        };
        let probe = Programmer::Rp2350Probe { base: RP2_XIP_BASE };

        let error =
            selector_for(volume, Some("SERIAL00"), None, None).expect_err("a drive is not a probe");
        assert!(
            error.contains("--volume"),
            "and it names the option that IS right: {error}"
        );

        let error = selector_for(probe, None, Some("D:"), None).expect_err("a probe is not a drive");
        assert!(
            error.contains("--probe"),
            "and it names the option that IS right: {error}"
        );

        assert_eq!(
            selector_for(volume, None, Some("D:"), None).expect("a drive on a volume route"),
            Some("D:".to_owned())
        );
        assert_eq!(
            selector_for(probe, Some("SERIAL00"), None, None).expect("a serial on a probe route"),
            Some("SERIAL00".to_owned())
        );
        assert_eq!(
            selector_for(volume, None, None, None).expect("neither is fine"),
            None
        );
    }

    /// A system bootloader over DFU is neither a probe nor a drive, so neither of their options can
    /// name one, and each refusal names the option that can; `--device` names it, and names nothing
    /// on any other route.
    #[test]
    fn a_dfu_bootloader_is_named_by_its_own_option_and_by_no_other() {
        let dfu = Programmer::StDfu { family: StFamily::H7 };

        let error =
            selector_for(dfu, Some("SERIAL00"), None, None).expect_err("a bootloader is not a probe");
        assert!(error.contains("--device <serial>"), "and it names the option that IS right: {error}");

        let error = selector_for(dfu, None, Some("D:"), None).expect_err("a bootloader is not a drive");
        assert!(error.contains("--device <serial>"), "and it names the option that IS right: {error}");
        assert!(!error.contains("--probe"), "a probe's option names no bootloader: {error}");

        assert_eq!(
            selector_for(dfu, None, None, Some("SERIAL00")).expect("a serial on the DFU route"),
            Some("SERIAL00".to_owned())
        );
        assert_eq!(selector_for(dfu, None, None, None).expect("none named is fine"), None);

        let probe = Programmer::Rp2350Probe { base: RP2_XIP_BASE };
        let error =
            selector_for(probe, None, None, Some("SERIAL00")).expect_err("a probe is not a bootloader");
        assert!(error.contains("--probe <serial>"), "and it names the option that IS right: {error}");

        let volume = Programmer::Uf2Volume {
            family: RP2350_UF2_FAMILY,
            base: RP2_XIP_BASE,
        };
        let error =
            selector_for(volume, None, None, Some("SERIAL00")).expect_err("a drive is not a bootloader");
        assert!(error.contains("--volume <name>"), "and it names the option that IS right: {error}");
    }

    #[test]
    fn a_board_keeps_its_volume_route_unless_a_probe_is_asked_for() {
        let pico = PROGRAMMING
            .iter()
            .find(|r| r.board == "rpi-pico2")
            .expect("rpi-pico2 is listed");
        assert!(
            matches!(route_for(pico, None), Ok(Programmer::Uf2Volume { .. })),
            "no --via means the route needing no probe"
        );
        assert!(
            matches!(
                route_for(pico, Some("volume")),
                Ok(Programmer::Uf2Volume { .. })
            ),
            "and asking for it by name is the same route"
        );
        assert!(
            matches!(
                route_for(pico, Some("probe")),
                Ok(Programmer::Rp2350Probe { .. })
            ),
            "asking for a probe selects one"
        );
    }

    #[test]
    fn a_probe_route_that_does_not_exist_is_refused_rather_than_ignored() {
        let without: Vec<&Programming> = PROGRAMMING
            .iter()
            .filter(|row| row.alternate.is_none())
            .collect();
        assert!(
            !without.is_empty(),
            "some board in the table has one route only"
        );
        let mut already = 0;
        let mut genuinely = 0;
        for row in without {
            let error = route_for(row, Some("probe")).expect_err("no probe route on this board");
            assert!(
                error.contains(row.board),
                "the message names the board: {error}"
            );
            if row.programmer.writes_over_a_probe() {
                assert!(
                    error.contains("ALREADY a probe"),
                    "{} is written over a probe and the refusal must say so: {error}",
                    row.board
                );
                already += 1;
            } else {
                assert!(
                    error.contains("--via probe cannot be honored"),
                    "and says the request was refused: {error}"
                );
                genuinely += 1;
            }
        }
        assert!(already > 0, "no single-route board is written over a probe: {already}");
        assert_eq!(genuinely, 0, "a volume-only board joined the table; the other arm is live now");
    }

    #[test]
    fn both_rp2040_picos_offer_the_probe_route_that_can_verify() {
        for board in ["rpi-pico", "rpi-pico-w"] {
            let row = PROGRAMMING
                .iter()
                .find(|r| r.board == board)
                .expect("listed");
            assert!(
                matches!(
                    route_for(row, Some("probe")),
                    Ok(Programmer::Rp2040Probe { .. })
                ),
                "{board} has a probe route"
            );
            assert!(
                matches!(route_for(row, None), Ok(Programmer::Uf2Volume { .. })),
                "{board} still defaults to the bootloader volume"
            );
        }
    }

    #[test]
    fn an_unknown_route_explains_the_two_that_exist() {
        let row = PROGRAMMING
            .iter()
            .find(|r| r.board == "rpi-pico2")
            .expect("listed");
        let error = route_for(row, Some("swd")).expect_err("not a route");
        assert!(
            error.contains("probe") && error.contains("volume"),
            "both named: {error}"
        );
        assert!(
            error.contains("CANNOT read the flash back"),
            "and the difference that matters is stated, unmangled: {error}"
        );
    }

    #[test]
    fn the_required_artifact_follows_the_route_not_the_board() {
        let row = PROGRAMMING
            .iter()
            .find(|r| r.board == "rpi-pico2")
            .expect("listed");
        let volume = route_for(row, Some("volume")).expect("volume route");
        let probe = route_for(row, Some("probe")).expect("probe route");
        assert!(
            matches!(
                volume.required_format(),
                Some(crate::artifact::Format::Uf2 { .. })
            ),
            "a bootloader volume takes a UF2"
        );
        assert_eq!(probe.required_format(), None, "a probe takes raw bytes");
        assert_eq!(
            volume.flash_base(),
            probe.flash_base(),
            "the same chip, the same address"
        );
    }

    #[test]
    fn a_uf2_is_recognized_so_it_is_never_wrapped_a_second_time() {
        let flat = [0xAAu8; 32];
        assert!(!is_uf2(&flat), "a flat image is not a UF2");
        let wrapped = crate::artifact::Format::Uf2 {
            family: RP2350_UF2_FAMILY,
        }
        .render(&flat, RP2_XIP_BASE);
        assert!(
            is_uf2(&wrapped),
            "and the wrapper produces one this recognizes"
        );
        assert!(
            !is_uf2(&[]),
            "an empty image is not a UF2 and must not panic"
        );
        assert!(
            !is_uf2(&[0x55, 0x46]),
            "nor is anything shorter than the magic"
        );
    }

    #[test]
    fn the_volume_mechanism_declares_that_it_cannot_read_back() {
        use lamella_flash_backend::FlashBackend as _;
        let mut backend = crate::backends::Uf2Volume::new(None, RP2_XIP_BASE, RP2350_UF2_FAMILY);
        let image = lamella_flash_backend::Image {
            bytes: &[0u8; 4],
            base: RP2_XIP_BASE,
        };
        assert!(
            backend.read_back(&image).is_none(),
            "a bootloader volume unmounts; there is no path back to the programmed bytes"
        );
    }

    #[test]
    fn an_unstamped_rp2350_image_is_refused_by_name() {
        let plain = vec![0u8; 512];
        let error = check_rp2350_stamp(&plain, Some("rp2350")).expect_err("no PICOBIN block");
        assert!(
            error.contains("PICOBIN"),
            "it names what is missing: {error}"
        );
        assert!(
            error.contains("no symptom"),
            "and why nothing will report it: {error}"
        );
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
        assert!(
            check_rp2350_stamp(&stub, Some("rp2350")).is_err(),
            "short and unstamped is still unstamped"
        );
    }

    #[test]
    fn the_image_our_own_builder_emits_passes_the_guard() {
        let image = lamella_aot::build::rp2350_boot_image(0, &[0x00, 0xBF, 0x00, 0xBF]);
        assert!(
            check_rp2350_stamp(&image, Some("rp2350")).is_ok(),
            "our own RP2350 boot image must carry the block this guard looks for"
        );
        assert!(
            image.len() < PICOBIN_SCAN_BYTES,
            "and it is shorter than the scanned window"
        );
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
        assert!(
            !PROGRAMMING.is_empty(),
            "an empty table would pass every assertion above"
        );
    }

    #[test]
    fn every_route_agrees_with_the_debugger_its_board_carries() {
        let mut checked = 0;
        for row in PROGRAMMING {
            let board = catalog::load_board(row.board)
                .expect("the census above already requires every board to resolve");
            let onboard = board.debug.as_ref().map(|debug| debug.onboard.as_str());
            let wants = match row.programmer {
                Programmer::StlinkOnboard { .. } => "st-link",
                Programmer::EdbgOnboard { .. } => "edbg",
                Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => "cmsis-dap",
                Programmer::Rp2350Probe { .. }
                | Programmer::Rp2040Probe { .. }
                | Programmer::SamExternalProbe { .. }
                | Programmer::StExternalProbe { .. } => "none",
                Programmer::StDfu { .. } => "none",
                Programmer::Uf2Volume { .. } => continue,
            };
            assert_eq!(
                onboard,
                Some(wants),
                "{}: its default route needs an on-board debugger of kind {wants:?}, and the board's \
                 `[debug] onboard` is {onboard:?}",
                row.board
            );
            checked += 1;
        }
        assert!(checked > 0, "no row was checked, so this proved nothing");
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
        assert_eq!(
            uf2_family_for_board("bbc-micro-bit-v2"),
            None,
            "written over a probe, not a volume"
        );
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
