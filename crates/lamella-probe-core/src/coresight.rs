//! The CoreSight ROM-table walk: asking a target's own debug architecture what is attached to it.

use super::{CoreMemory, ProbeError};
use std::fmt;


/// Peripheral Identification Register 4, at this offset from a component's base address.
///
/// CoreSight v3.0 (IHI 0029G), B2.1.5 and B2.2.2.2. `PIDR4`-`PIDR7` run `0xFD0`-`0xFDC` and
/// `PIDR0`-`PIDR3` run `0xFE0`-`0xFEC`, so the eight are NOT in numeric order in the address map.
pub const PIDR4: u32 = 0xfd0;
/// Peripheral Identification Register 0. See [`PIDR4`] for why this is above it in memory.
pub const PIDR0: u32 = 0xfe0;
/// Component Identification Register 0. CoreSight v3.0 (IHI 0029G), B2.2.1.2: `CIDR0`-`CIDR3` are
/// at `0xFF0`, `0xFF4`, `0xFF8`, `0xFFC`.
pub const CIDR0: u32 = 0xff0;

/// Device Architecture Register, for a Class `0x9` component only.
///
/// CoreSight v3.0 (IHI 0029G), B2.3.4.2.
pub const DEVARCH: u32 = 0xfbc;

/// Device Configuration Register, for a Class `0x9` component only.
///
/// ADIv6.0 (IHI 0074E), D3.5.10. Read only for a Class `0x9` ROM table, where its `FORMAT` field
/// decides how wide that table's entries are.
pub const DEVID: u32 = 0xfc8;

/// `0xFCC` -- and it is TWO registers, chosen by the component's class.
///
/// For a Class `0x9` CoreSight component this is `DEVTYPE` (IHI 0029G, B2.1.5). For a Class `0x1`
/// ROM table it is `MEMTYPE` (ADIv6.0 IHI 0074E, Table D2-1; and the Armv7-M ARM DDI 0403E.d states
/// the same offset in its Table C1-3). **One offset, two unrelated meanings, and nothing at the
/// offset says which** -- so it is read only after `CIDR1.CLASS` has been decoded.
pub const DEVTYPE_OR_MEMTYPE: u32 = 0xfcc;

/// Which of the two ROM table encodings a table uses.
///
/// **The two are not interchangeable, and nothing inside an entry says which one it is.** The
/// answer comes from the TABLE's own `CIDR1.CLASS`, and for a Class `0x9` table also from its
/// `DEVID`. Reading a Class `0x9` table with Class `0x1` rules gets the terminator, the present
/// marker and the last-entry offset all wrong at once, and every one of those failures is silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableFormat {
    /// A Class `0x1` ROM table: 32-bit entries, a ONE-bit `PRESENT` beside a `FORMAT` bit, ended by
    /// an all-zero word (ADIv6.0 IHI 0074E, D2.2.1 and D2.4.4).
    ClassOne,
    /// A Class `0x9` ROM table with `DEVID.FORMAT == 0x0`: 32-bit entries and a TWO-bit `PRESENT`
    /// whose four values include a "skip me" distinct from "table ends here" (IHI 0074E, D3.5.17).
    ClassNine32,
}

impl TableFormat {
    /// The offset of the last entry slot. IHI 0074E, D2.2.1 and D3.5.17 agree in wording: an entry
    /// at this offset is the final one whatever it holds, and the reserved area begins after it.
    ///
    /// WARNING: the two differ by 1,792 bytes. Reading a Class `0x9` table with the Class `0x1`
    /// value runs on through its reserved area and into its power-control registers, decoding each
    /// word as an entry -- and `SYSPCR` is READ-WRITE, so what it decodes is whatever software left
    /// there.
    pub fn last_entry_offset(self) -> u32 {
        match self {
            TableFormat::ClassOne => 0xefc,
            TableFormat::ClassNine32 => 0x7fc,
        }
    }

    /// A ROM table occupies exactly 4KB either way. ADIv5.2 (IHI 0031G), D1.1: *"A ROM Table always
    /// occupies 4KB of memory."*
    pub fn entry_stride(self) -> u32 {
        4
    }
}

/// How many ROM tables deep the walk will follow a hierarchy before refusing.
///
/// ADIv6.0 (IHI 0074E), D1 prohibits circular references, but a debugger that trusts a target to be
/// conforming hangs on the one that is not. The cycle guard below is the real defence; this is the
/// cheap backstop for a chain that is merely absurd rather than circular.
const MAX_DEPTH: u8 = 8;

/// How many components the walk will report before refusing to continue.
///
/// A single table can name 960, and a hierarchy multiplies that. A bench probe that has started
/// reading nonsense should stop and say so rather than issue a hundred thousand round trips.
const MAX_COMPONENTS: usize = 256;

/// The component class from `CIDR1.CLASS`.
///
/// CoreSight v3.0 (IHI 0029G), B2.2.1.1, Table B2-8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// `0x0` -- generic verification component.
    GenericVerification,
    /// `0x1` -- a ROM table. The walk recurses into these.
    RomTable,
    /// `0x9` -- a CoreSight component, which carries `DEVARCH` and `DEVTYPE` as well.
    CoreSight,
    /// `0xB` -- peripheral test block.
    PeripheralTestBlock,
    /// `0xE` -- generic IP component.
    GenericIp,
    /// `0xF` -- a CoreLink, PrimeCell or system component with no standardized register layout.
    PrimeCell,
    /// A value the specification reserves: `0x2`-`0x8`, `0xA`, or `0xC`-`0xD`.
    Reserved(u8),
}

impl Class {
    /// Decodes `CIDR1.CLASS`, bits[7:4] of `CIDR1`.
    fn from_bits(bits: u8) -> Class {
        match bits {
            0x0 => Class::GenericVerification,
            0x1 => Class::RomTable,
            0x9 => Class::CoreSight,
            0xb => Class::PeripheralTestBlock,
            0xe => Class::GenericIp,
            0xf => Class::PrimeCell,
            other => Class::Reserved(other),
        }
    }

    /// The raw four-bit encoding.
    pub fn bits(self) -> u8 {
        match self {
            Class::GenericVerification => 0x0,
            Class::RomTable => 0x1,
            Class::CoreSight => 0x9,
            Class::PeripheralTestBlock => 0xb,
            Class::GenericIp => 0xe,
            Class::PrimeCell => 0xf,
            Class::Reserved(bits) => bits,
        }
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Class::GenericVerification => f.write_str("generic verification component"),
            Class::RomTable => f.write_str("ROM table"),
            Class::CoreSight => f.write_str("CoreSight component"),
            Class::PeripheralTestBlock => f.write_str("peripheral test block"),
            Class::GenericIp => f.write_str("generic IP component"),
            Class::PrimeCell => f.write_str("PrimeCell or system component"),
            Class::Reserved(bits) => write!(f, "reserved class {bits:#x}"),
        }
    }
}

/// Who designed a component, as a JEP106 code read out of the peripheral ID registers.
///
/// CoreSight v3.0 (IHI 0029G), A1.5. **The code is what this carries, not a name.** A JEP106 number
/// means nothing without the JEDEC registry, which this decode does not consult and does not
/// reproduce -- so a name is attached at the point of use, from parts that identify themselves, and
/// the decode below stops at the number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Designer {
    /// The 11-bit compressed form: a 4-bit continuation code and a 7-bit identity code.
    ///
    /// The continuation code is *"the count of `0x7F` continuation codes in the designer's JEP106
    /// code"* -- a bank number, one less than the bank as JEDEC numbers them. Limited to the first
    /// 16 banks.
    Compressed {
        /// The 4-bit continuation code, from `PIDR4.DES_2`.
        continuation: u8,
        /// The 7-bit identity code, from `PIDR2.DES_1`:`PIDR1.DES_0`.
        identity: u8,
    },
    /// The 16-bit extended form: a 9-bit continuation code and a 7-bit identity code, both in
    /// `PIDR4.DES_3`. Reached only when a designer's bank is past the compressed form's 16.
    Extended {
        /// The 9-bit continuation code, `DES_3` bits[15:7].
        continuation: u16,
        /// The 7-bit identity code, `DES_3` bits[6:0].
        identity: u8,
    },
    /// The designer is deliberately not identified: identity `0x7F` with continuation `0xF`.
    /// IHI 0029G's own Example A1-1 calls this *"not recommended"*, and it does occur.
    Unspecified,
    /// Identity `0x7F` with a continuation code the specification reserves -- `0x1` through `0xE`.
    /// Neither a designer nor a defined escape, so it is carried rather than guessed at.
    ReservedEscape {
        /// The reserved 4-bit continuation code.
        continuation: u8,
    },
    /// `PIDR2.JEDEC` is `0`, where IHI 0029G says it *"Must be 0b1 to indicate that a JEDEC-assigned
    /// value is used"*. The designer fields hold something that is not a JEP106 code, so nothing
    /// here decodes them.
    NotJedec,
}

impl Designer {
    /// The `(continuation, identity)` pair for the two forms that identify a designer.
    ///
    /// The two forms differ only in how many banks they can reach, so a caller matching a designer
    /// against a table wants them unified -- and a designer reachable by the compressed form may
    /// legally be encoded in the extended one (IHI 0029G, Example A1-1 gives Arm both ways).
    /// **A lookup table keyed on only one form would miss the same vendor spelled the other way.**
    pub fn jep106(self) -> Option<(u16, u8)> {
        match self {
            Designer::Compressed { continuation, identity } => {
                Some((u16::from(continuation), identity))
            }
            Designer::Extended { continuation, identity } => Some((continuation, identity)),
            Designer::Unspecified | Designer::ReservedEscape { .. } | Designer::NotJedec => None,
        }
    }

    /// The 11-bit value that names this designer where a designer field is 11 bits wide -- an ADIv5
    /// `DPIDR`'s `DESIGNER`, or a `DEVARCH.ARCHITECT`. `None` for a designer outside the first 16
    /// banks, which no 11-bit field can express.
    ///
    /// **This is what makes the walk comparable with what the DP already reports.** `DPIDR` names a
    /// designer in exactly this encoding, so a target whose DP and whose components disagree is
    /// saying something worth hearing.
    pub fn as_eleven_bit(self) -> Option<u16> {
        match self.jep106() {
            Some((continuation, identity)) if continuation < 16 => {
                Some((continuation << 7) | u16::from(identity))
            }
            _ => None,
        }
    }
}

impl fmt::Display for Designer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Designer::Compressed { continuation, identity } => {
                write!(f, "JEP106 bank {continuation}, id {identity:#04x}")
            }
            Designer::Extended { continuation, identity } => {
                write!(f, "JEP106 bank {continuation} (extended), id {identity:#04x}")
            }
            Designer::Unspecified => f.write_str("designer not specified"),
            Designer::ReservedEscape { continuation } => {
                write!(f, "reserved designer escape (continuation {continuation:#x})")
            }
            Designer::NotJedec => f.write_str("designer field is not a JEDEC code"),
        }
    }
}

/// A component's part number, in BOTH readings, because the registers do not say which is meant.
///
/// CoreSight v3.0 (IHI 0029G), B2.2.2.1: a 12-bit part number puts bits[11:8] in `PIDR1.PART_1` and
/// bits[7:0] in `PIDR0.PART_0`, leaving `PIDR2.REVISION` as a revision; a 16-bit part number shifts
/// everything up a nibble and takes `PIDR2.REVISION` as part number bits[3:0].
///
/// **The choice is "specific to the designer of the component", in the specification's words, and
/// no bit records it.** So a walk cannot decide, and picking one silently is how a part number gets
/// misreported by a factor of sixteen. Both readings are carried; the designer's own convention
/// picks between them, which is knowledge that belongs in the per-vendor table and not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartNumber {
    /// `PIDR1.PART_1`:`PIDR0.PART_0`. The conventional reading, and the one Arm's own components use.
    pub twelve_bit: u16,
    /// `PIDR1.PART_1`:`PIDR0.PART_0`:`PIDR2.REVISION`.
    pub sixteen_bit: u16,
}

/// `DEVARCH`, present only on a Class `0x9` component.
///
/// CoreSight v3.0 (IHI 0029G), B2.3.4. **This is the register that answers "which architecture",
/// as opposed to "whose silicon"** -- `ARCHID` names the thing the component implements, and the
/// architect may differ from the designer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceArchitecture {
    /// `ARCHITECT`, bits[31:21] -- the 11-bit compressed JEP106 code of the ARCHITECT, which
    /// IHI 0029G notes is `0x23B` where Arm is the architect.
    pub architect: u16,
    /// `REVISION`, bits[19:16] -- the revision of the architecture `archid` names.
    pub revision: u8,
    /// `ARCHID`, bits[15:0].
    pub archid: u16,
}

impl DeviceArchitecture {
    /// Decodes `DEVARCH`, or `None` when `PRESENT` (bit[20]) is `0`.
    ///
    /// IHI 0029G, B2.3.4.1: with `PRESENT` clear, *"Bits[31:0] must be RAZ"* -- so a zero word is
    /// the register saying it is not implemented, which is the ordinary case on a component that
    /// predates the register.
    fn decode(word: u32) -> Option<DeviceArchitecture> {
        (word & (1 << 20) != 0).then(|| DeviceArchitecture {
            architect: (word >> 21) as u16 & 0x7ff,
            revision: (word >> 16) as u8 & 0xf,
            archid: word as u16,
        })
    }
}

/// The designer a JEP106 code names, for the codes this project can source.
///
/// **A JEP106 number means nothing without JEDEC's assignment list**, which is not held here and
/// is not transcribed. A row is added when the code and the name are tied to each other -- by the
/// CoreSight specification stating the code, or by a part that names itself through a vendor
/// identity register a datasheet tabulates. **`None` is the honest answer for every other code**,
/// and it is what a new row starts as: it says a designer was read and this table cannot name it,
/// which is different from the read having failed.
///
/// The pair is the one [`Designer::jep106`] returns, so a designer spelled in the extended form
/// matches the same row as one spelled in the compressed form. **One table, because a second one
/// gains a row in neither**: the walk, the debug-port decode and any tool built on either all name
/// a designer through this.
#[must_use]
pub fn designer_name(continuation: u16, identity: u8) -> Option<&'static str> {
    match (continuation, identity) {
        (0x4, 0x3b) => Some("Arm"),
        (0x0, 0x1f) => Some("Atmel (Microchip)"),
        (0x0, 0x20) => Some("STMicroelectronics"),
        _ => None,
    }
}

/// [`designer_name`] for a field that carries the code in its 11-bit form: an ADIv5
/// `DPIDR.DESIGNER`, or a `DEVARCH.ARCHITECT`.
///
/// # A DEBUG PORT'S DESIGNER FIELD NAMES THE IP VENDOR, NOT THE SILICON VENDOR
///
/// The debug port is licensed Arm IP, so `DPIDR.DESIGNER` reports Arm on parts from several
/// different silicon vendors: every debug-port id recorded in this project's chip facts decodes to
/// Arm's `0x23B`, across four vendors. **A caller that wants the silicon vendor has to read the
/// ROM table instead**, where a vendor's own system table carries that vendor's code -- which is
/// what [`walk`] and [`Walk::designers`] are for.
///
/// The 11-bit form reaches the first 16 JEP106 banks only, which is the same limit
/// [`Designer::as_eleven_bit`] states from the other direction.
#[must_use]
pub fn designer_name_of_eleven_bit(value: u16) -> Option<&'static str> {
    designer_name((value >> 7) & 0xf, value as u8 & 0x7f)
}

/// The name Arm's own specification gives an `ARCHID`, where it gives one.
///
/// CoreSight v3.0 (IHI 0029G), B2.3.4.1, Table B2-19. **The table is headed "Example ARCHID
/// values" and is explicitly not exhaustive**, so an unlisted id is an ordinary outcome and not a
/// sign that anything is wrong. The values are only meaningful when `DEVARCH.ARCHITECT` says Arm
/// is the architect, which is why this takes the architect too rather than the id alone.
pub fn architecture_name(architect: u16, archid: u16) -> Option<&'static str> {
    if architect != ARM_ELEVEN_BIT {
        return None;
    }
    Some(match archid {
        0x0a00 => "RAS",
        0x1a01 => "ITM",
        0x1a02 => "DWT",
        0x1a03 => "FPB",
        0x2a04 => "processor debug (Armv8-M)",
        0x6a05 => "processor debug (Armv8-R)",
        0x0a10 => "PC sample-based profiling",
        0x4a13 => "ETM",
        0x1a14 => "CTI",
        0x6a15 => "processor debug (v8.0-A)",
        0x7a15 => "processor debug (v8.1-A)",
        0x8a15 => "processor debug (v8.2-A)",
        0x2a16 => "PMU",
        0x0a17 => "Memory Access Port v2",
        0x0a27 => "JTAG Access Port v2",
        0x0a31 => "basic trace router",
        0x0a34 => "power requester",
        0x0a47 => "Unknown Access Port v2",
        0x0a50 => "HSSTP",
        0x0a63 => "STM",
        0x0a75 => "CoreSight ELA",
        0x0af7 => "CoreSight ROM",
        _ => return None,
    })
}

/// `DEVARCH.ARCHID` for a Class `0x9` ROM table -- "ROM Table v0".
///
/// ADIv6.0 (IHI 0074E), D3.5.9 states it as the register's only permitted value for such a table,
/// and CoreSight v3.0 (IHI 0029G), Table B2-19 lists the same id as "CoreSight ROM architecture".
pub const CORESIGHT_ROM_ARCHID: u16 = 0x0af7;

/// The 11-bit compressed JEP106 code for Arm.
///
/// Stated twice by IHI 0029G rather than recalled: Example A1-1 gives Arm continuation `0x4` and
/// identity `0x3B`, and B2.3.4.1 says of `DEVARCH.ARCHITECT` that *"For components where Arm is the
/// architect, this field is 0x23B"*. `(0x4 << 7) | 0x3B == 0x23B`, so the two agree.
pub const ARM_ELEVEN_BIT: u16 = 0x23b;

/// One component the walk found, and everything its identification registers said about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// The base address of the component's 4KB identification block.
    pub address: u32,
    /// How many ROM tables were traversed to reach it. The table `BASE` names is depth `0`.
    pub depth: u8,
    /// `CIDR1.CLASS`.
    pub class: Class,
    /// Who designed it, from `PIDR1.DES_0` / `PIDR2.DES_1` / `PIDR4.DES_2` / `PIDR4.DES_3`.
    pub designer: Designer,
    /// Its part number, in both readings. See [`PartNumber`].
    pub part: PartNumber,
    /// `PIDR2.REVISION`, which is ALSO part number bits[3:0] under a 16-bit part numbering
    /// scheme -- see [`PartNumber`] for why nothing here can tell which.
    pub revision: u8,
    /// `PIDR3.REVAND` -- a minor revision, a modification marker, or the revision itself, depending
    /// on the designer's scheme (IHI 0029G, B2.2.2.1).
    pub revand: u8,
    /// `PIDR3.CMOD` -- `0x0` means unmodified from the original design.
    pub cmod: u8,
    /// `PIDR4.SIZE`, the log2 of the number of 4KB blocks the component occupies.
    ///
    /// **Deprecated by the specification and not to be trusted**: IHI 0029G says it "might not
    /// correctly indicate the size", and ADIv6.0 (IHI 0074E), D1.4 records that the field's use for
    /// this is deprecated. Carried because it is part of the Unique Component Identifier.
    pub size_log2: u8,
    /// `DEVARCH`, for a Class `0x9` component that implements it.
    pub architecture: Option<DeviceArchitecture>,
    /// The word at `0xFCC` -- `DEVTYPE` for a Class `0x9` component, `MEMTYPE` for a ROM table.
    ///
    /// Undecoded on purpose. `DEVTYPE`'s major/sub encoding is a large nested table whose value
    /// here would be a generic label ("trace sink"), and the question this walk exists to answer is
    /// whose identity register to trust. The raw word is still a fingerprint, and it is part of the
    /// Unique Component Identifier, so it is carried rather than dropped.
    pub type_word: Option<u32>,
}

impl Component {
    /// Whether this component is a table of further components.
    ///
    /// **Both classes can be one.** ADIv5.2 (IHI 0031G), D1.2 permits a ROM table to be Class
    /// `0x1` OR Class `0x9`, and says both may appear in one system. A walk that recursed only into
    /// Class `0x1` would silently stop at the first Class `0x9` table and report a subset -- which
    /// looks exactly like a target with fewer components.
    ///
    /// The Class `0x9` test is `DEVARCH`, as ADIv6.0 (IHI 0074E), D3.1 says outright: *"The Device
    /// Architecture Register, DEVARCH, identifies the component as a Class 0x9 ROM Table."* D3.5.9
    /// then fixes the values -- `ARCHITECT` `0x23B`, `ARCHID` `0x0AF7`, "ROM Table v0".
    ///
    /// It says only that this component IS a table, not that its entries can be read: that needs
    /// `DEVID`, which [`walk`] reads once it gets here.
    pub fn is_rom_table(&self) -> bool {
        match self.class {
            Class::RomTable => true,
            Class::CoreSight => self.architecture.is_some_and(|arch| {
                arch.architect == ARM_ELEVEN_BIT && arch.archid == CORESIGHT_ROM_ARCHID
            }),
            _ => false,
        }
    }
}

/// One thing the walk could not do, recorded rather than raised.
///
/// **The specification requires this.** ADIv5.2 (IHI 0031G), C2.6.1 lists a faulting base address,
/// an invalid set of Component ID registers, an entry pointing at a faulting location and an entry
/// pointing at a block with no valid Component IDs, and says *"A debugger must handle the following
/// situations as non-fatal errors"* and that Arm *"recommends that it continues operating"*. A
/// subsystem disabled by a tie-off or a fuse produces exactly these, on a working part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The address being read when it happened.
    pub address: u32,
    /// How deep in the hierarchy, matching [`Component::depth`].
    pub depth: u8,
    /// What went wrong.
    pub cause: Cause,
}

/// Why a [`Problem`] was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// A read of the address faulted or the transfer failed.
    Unreadable(ProbeError),
    /// The four Component ID registers do not carry the architectural preamble, so whatever is at
    /// this address is not a CoreSight component. Carries them so the reader can see what was there.
    NotAComponent([u32; 4]),
    /// A Class `0x1` ROM table entry sets `FORMAT` to `0`.
    ///
    /// The Armv6-M ARM (DDI 0419E), Table C1-3 calls `FORMAT` `0` the *"8-bit format"*; ADIv6.0
    /// (IHI 0074E), D2.4.4 makes the field RAO. Either way the entries are not the 32-bit words
    /// this reads, so the rest of the table is not parseable and the walk stops on it rather than
    /// reporting whatever the misparse produced.
    EightBitFormat(u32),
    /// A Class `0x9` ROM table entry has `PRESENT == 0b01`, which ADIv6.0 (IHI 0074E), D3.5.17
    /// reserves. Neither present, nor a gap, nor the end -- so what follows it is not readable.
    ReservedEntry(u32),
    /// A Class `0x9` ROM table's `DEVID.FORMAT` is not `0x0`, so its entries are not 32-bit words.
    ///
    /// ADIv6.0 (IHI 0074E), D3.5.10: `0x1` is the 64-bit format and `0x2`-`0xF` are reserved. The
    /// 64-bit form is refused rather than half-read -- its `OFFSET` is bits[63:12], so a component
    /// address does not fit the `u32` this walk carries, and ADIv5.2 (IHI 0031G), D1.2 requires
    /// `FORMAT` to be `0` on an ADIv5 implementation, which is every part this drives. Carries the
    /// `DEVID` word so a target that does it anyway is reported rather than guessed at.
    UnsupportedTableFormat(u32),
    /// A ROM table entry points at a table already visited on this path.
    ///
    /// ADIv6.0 (IHI 0074E), D1 prohibits circular references. A target that has one would otherwise
    /// walk forever.
    CircularReference(u32),
    /// The hierarchy is deeper than [`MAX_DEPTH`], or names more than [`MAX_COMPONENTS`] components.
    TooLarge,
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cause::Unreadable(error) => write!(f, "could not be read: {error}"),
            Cause::NotAComponent(cidr) => write!(
                f,
                "no component ID preamble (CIDR0-3 read {:#010x} {:#010x} {:#010x} {:#010x})",
                cidr[0], cidr[1], cidr[2], cidr[3]
            ),
            Cause::EightBitFormat(entry) => {
                write!(f, "ROM table entry {entry:#010x} is not the 32-bit format")
            }
            Cause::ReservedEntry(entry) => {
                write!(f, "ROM table entry {entry:#010x} uses a reserved PRESENT value")
            }
            Cause::UnsupportedTableFormat(devid) => write!(
                f,
                "this ROM table's DEVID {devid:#010x} asks for entries this walk does not read"
            ),
            Cause::CircularReference(to) => write!(f, "points back at the table at {to:#010x}"),
            Cause::TooLarge => f.write_str("the component hierarchy is larger than this walk allows"),
        }
    }
}

/// Everything one walk found, including what it could not reach.
///
/// **`problems` being non-empty does not make `components` wrong**, and an empty `components` with
/// a populated `problems` is the interesting case rather than a failure -- it is a target that
/// answered reads but has nothing recognizable where the ROM table should be.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Walk {
    /// Every component reached, in the order the tables listed them, depth-first.
    pub components: Vec<Component>,
    /// Everything the walk could not do. See [`Problem`].
    pub problems: Vec<Problem>,
}

/// What [`Walk::vendor_designer`] concluded about whose silicon a walk describes.
///
/// **Three of the four variants are answers rather than failures**, which is why this is an enum
/// and not an `Option<Designer>`: a caller that has to choose a vendor identity register needs to
/// know WHICH of the ways the question ended, because they call for different next steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorDesigner {
    /// Exactly one designer other than Arm. **This is the code that selects a vendor identity
    /// register.**
    One(Designer),
    /// Every component that identifies a designer identifies Arm.
    ///
    /// A true statement about the silicon: the vendor licensed Arm's debug components and designed
    /// none of its own, so the ROM table has nothing further to say and the part must be named by
    /// some other route.
    ArmThroughout,
    /// More than one non-Arm designer. Nothing here chooses between them, because a walk that saw
    /// two vendors' components is describing something this rule was not written for -- and
    /// picking the first would be a guess wearing an answer's clothes.
    Several,
    /// No component identified a designer at all: every one answered a reserved escape, an
    /// unspecified code, or a `PIDR2.JEDEC` that says the field is not a JEP106 code.
    Nothing,
}

impl Walk {
    /// The designer every component agrees on, or `None` when they do not agree or none identifies
    /// one.
    ///
    /// **This is the answer the walk exists to produce.** A Cortex-M system's components are mostly
    /// Arm's, so the designer that is NOT Arm is usually the vendor -- but a system where the
    /// vendor designed nothing reports Arm throughout, and that is a true answer about the silicon
    /// rather than a failure to look harder. Use [`Walk::designers`] when the distribution matters.
    pub fn sole_designer(&self) -> Option<Designer> {
        let mut found: Option<Designer> = None;
        for component in &self.components {
            if component.designer.jep106().is_none() {
                continue;
            }
            match found {
                Some(seen) if seen != component.designer => return None,
                Some(_) => {}
                None => found = Some(component.designer),
            }
        }
        found
    }

    /// Which designer in this walk belongs to the SILICON vendor rather than to Arm.
    ///
    /// **This is the step between reading a table and reading a vendor register**, and it is a
    /// rule about the architecture rather than about any part: Arm designed the SCS, the DWT, the
    /// FPB and the ITM, and they carry Arm's code on every vendor's silicon. So a designer that is
    /// not Arm's is the vendor's own, and it is the code that selects which vendor identity
    /// register to read next.
    ///
    /// Every outcome other than [`VendorDesigner::One`] is a real answer about the part and not a
    /// failure to look harder -- see each variant.
    pub fn vendor_designer(&self) -> VendorDesigner {
        let mut identified = 0usize;
        let mut vendor: Option<Designer> = None;
        for component in &self.components {
            if component.designer.jep106().is_none() {
                continue;
            }
            identified += 1;
            if component.designer.as_eleven_bit() == Some(ARM_ELEVEN_BIT) {
                continue;
            }
            match vendor {
                Some(seen) if seen.jep106() != component.designer.jep106() => {
                    return VendorDesigner::Several;
                }
                _ => vendor = Some(component.designer),
            }
        }
        match (vendor, identified) {
            (Some(designer), _) => VendorDesigner::One(designer),
            (None, 0) => VendorDesigner::Nothing,
            (None, _) => VendorDesigner::ArmThroughout,
        }
    }

    /// Every distinct designer the walk saw, in first-seen order, with how many components each
    /// designed.
    ///
    /// **The vendor is usually the minority entry.** On a Cortex-M part Arm designed the SCS, DWT,
    /// FPB and ITM; anything else is the vendor's own, and a system-level ROM table is the one most
    /// likely to carry the vendor's code.
    pub fn designers(&self) -> Vec<(Designer, usize)> {
        let mut tally: Vec<(Designer, usize)> = Vec::new();
        for component in &self.components {
            match tally.iter_mut().find(|(seen, _)| *seen == component.designer) {
                Some((_, count)) => *count += 1,
                None => tally.push((component.designer, 1)),
            }
        }
        tally
    }
}

/// What a MEM-AP's `BASE` register said about where its debug components are described.
///
/// ADIv5.2 (IHI 0031G), C2.6.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugBase {
    /// The AP names a ROM table or a single debug component at this address.
    ///
    /// **Which of the two it is, `BASE` does not say.** IHI 0031G, D1.4 lists both, and says of
    /// the whole register that "The ADIv5 architecture specification does not specify requirements
    /// for the type of component pointed to by BASE". Reading the component's own `CIDR1.CLASS`
    /// is what settles it, which is what [`walk`] does.
    At(u32),
    /// No debug entry is present on this AP: `BASE.P` is clear, or the legacy `0xFFFFFFFF`.
    Absent,
    /// The register read as all zeroes.
    ///
    /// IHI 0031G, B2.2.9 says of `SELECT.APSEL` that *"If there is no AP with the ID APSEL, all AP
    /// transactions return zero on reads"* -- so an all-zero `BASE` is most likely an AP index that
    /// is not populated, and only secondarily a legacy register naming address zero. **Reported as
    /// its own answer** because sending a walk to address `0x00000000` would turn "you selected an
    /// AP that does not exist" into "there is nothing recognizable at the base address", which
    /// points a reader at the target instead of at their own AP selection.
    Zero,
}

/// Decodes a `BASE` register word.
///
/// ADIv5.2 (IHI 0031G), C2.6.1: `BASEADDR` bits[31:12], bits[11:2] RES0, `Format` bit[1], `P`
/// bit[0], plus a legacy format that predates the `Format` bit.
///
/// WARNING: **`P` IS ONLY A PRESENCE BIT IN THE ADIv5 FORMAT.** In the legacy format (`Format`
/// clear) the specification makes bit[0] "Reserved, RAZ" -- so a decoder that tests `P`
/// unconditionally reports "no debug components" for a legacy DAP, out of the very word that holds
/// their address.
pub fn decode_base(word: u32) -> DebugBase {
    if word == 0 {
        return DebugBase::Zero;
    }
    if word == 0xffff_ffff {
        return DebugBase::Absent;
    }
    let adiv5_format = word & 0b10 != 0;
    if adiv5_format && word & 1 == 0 {
        return DebugBase::Absent;
    }
    DebugBase::At(word & 0xffff_f000)
}

/// What a debug port's own `BASEPTR0`/`BASEPTR1` say about where its debug components are
/// described.
///
/// **This is the ADIv6 answer to the question `BASE` answers on an ADIv5 MEM-AP**, and it is asked
/// one level further out: `BASE` is an AP register, so reading it means having already selected an
/// AP -- and an ADIv6 DP selects an AP by ADDRESS rather than by index, which is exactly what a
/// caller with no prior knowledge does not have. `BASEPTR` is a DP register, so it can be read
/// before any AP is chosen, and ADIv6.0 (IHI 0074E) D1.5 says it typically points at *"a top-level
/// ROM Table which indicates where APv2 APs are located"*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugBasePointer {
    /// The debug port is not a DPv3, so these registers do not exist on it.
    ///
    /// **THIS IS NOT A FAILED READ, AND THE READ MUST NOT BE ATTEMPTED ANYWAY**: on an earlier DP,
    /// register offset `0x0` reads `DPIDR` whatever `SELECT.DPBANKSEL` holds, so asking regardless
    /// returns a debug-port id that decodes as a plausible address. IHI 0074E, B2.2.6 gives
    /// `DPIDR.VERSION` the value `0x3` for DPv3, and A1 states that ADIv6 permits DPv3 only.
    NotDpv3 {
        /// `DPIDR.VERSION`, so a report can name the architecture that answered.
        version: u8,
    },
    /// `BASEPTR0.VALID` is clear: *"No valid base address is specified"* (IHI 0074E, B2.2.2), which
    /// D1.5 lists as one of the three things a DP's base pointer can say -- no debug components are
    /// accessible from this DP at all.
    Absent,
    /// The system address of the first component reachable from this DP.
    At {
        /// The address, 4KB aligned. `BASEPTR1[31:0]` supplies bits[63:32], and `BASEPTR0`
        /// bits[31:12] supply bits[31:12]; bits[11:0] are always zero.
        address: u64,
        /// The address width `DPIDR1.ASIZE` declares, in bits, so a report can say whether the
        /// upper word was meaningful. IHI 0074E, B2.2.7 permits 12, 20, 32, 40, 48 and 52.
        address_bits: u8,
    },
}

/// `DPIDR.VERSION` for a DPv3, the only version ADIv6 permits (IHI 0074E, B2.2.6 and A1).
pub const DPV3: u8 = 0x3;

/// Decodes a debug port's base pointer from the four registers that carry it.
///
/// A pure function over words read elsewhere, for the same reason [`decode_base`] is: **the reads
/// are four DP register accesses across three banks, and the arithmetic that turns them into an
/// address is the part worth checking without a target on the wire.**
///
/// `dpidr` gates the rest -- see [`DebugBasePointer::NotDpv3`].
#[must_use]
pub fn decode_base_pointer(dpidr: u32, dpidr1: u32, low: u32, high: u32) -> DebugBasePointer {
    let version = (dpidr >> 12) as u8 & 0xf;
    if version != DPV3 {
        return DebugBasePointer::NotDpv3 { version };
    }
    if low & 1 == 0 {
        return DebugBasePointer::Absent;
    }
    let address = (u64::from(high) << 32) | u64::from(low & 0xffff_f000);
    DebugBasePointer::At { address, address_bits: dpidr1 as u8 & 0x7f }
}

/// One ROM table entry, decoded against the format its table declared.
///
/// `OFFSET` bits[31:12], `POWERID` bits[8:4] and `POWERIDVALID` bit[2] are the same in both formats
/// (ADIv6.0 IHI 0074E, D2.4.4 and D3.5.17). **The low two bits are not**, which is the whole reason
/// [`decode`](Self::decode) takes a [`TableFormat`] rather than reading a word on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RomEntry {
    /// The raw word.
    pub word: u32,
    /// A component is present at [`component_address`](Self::component_address).
    pub present: bool,
    /// This entry ends the table, whatever else it holds.
    pub ends_table: bool,
    /// The entry is in an encoding this reads. Clear for a Class `0x1` entry with `FORMAT == 0`
    /// (the legacy 8-bit table) or a Class `0x9` entry with the reserved `PRESENT` value `0b01`.
    pub understood: bool,
    /// `POWERID` bits[8:4], when `POWERIDVALID` bit[2] is set.
    pub power_id: Option<u8>,
}

impl RomEntry {
    /// Decodes a table entry word under its table's format.
    ///
    /// **The two formats disagree about what "not present" means, and only one spelling of it ends
    /// the table.** ADIv6.0 (IHI 0074E), D2.2.2 for Class `0x1`: an entry marked not present *"must
    /// be skipped"*, and *"unless the entry has the value 0x00000000 ... it must not be assumed that
    /// an entry that is marked not present represents the end of the ROM Table"*. D3.5.17 for Class
    /// `0x9` splits the same distinction into two encodings outright -- `0b00` ends the table,
    /// `0b10` is a gap to skip -- so there it is stated in the field rather than left to the word.
    ///
    /// A walk that stopped at the first not-present entry would truncate the table on a part whose
    /// tie-offs disable a component: silently, and identically every run.
    pub fn decode(word: u32, format: TableFormat) -> RomEntry {
        let (present, ends_table, understood) = match format {
            TableFormat::ClassOne => (word & 1 != 0, word == 0, word & 0b10 != 0 || word == 0),
            TableFormat::ClassNine32 => match word & 0b11 {
                0b00 => (false, true, true),
                0b10 => (false, false, true),
                0b11 => (true, false, true),
                _ => (false, false, false),
            },
        };
        RomEntry {
            word,
            present,
            ends_table,
            understood,
            power_id: (word & 0b100 != 0).then(|| (word >> 4) as u8 & 0x1f),
        }
    }

    /// The address of the component this entry names, given the table's own base address.
    ///
    /// Shared by both formats: `OFFSET` is bits[31:12] of the 32-bit word either way.
    ///
    /// ADIv6.0 (IHI 0074E), D1.4: `Component_n_Address = ROM_Base_Address + (OFFSET << 12)`, with
    /// `OFFSET` a two's-complement signed value -- the Armv6-M ARM (DDI 0419E), Table C1-3 calls it
    /// a *"Signed base address offset"* outright, and every Cortex-M ROM table entry is negative
    /// because the table sits at `0xE00FF000`, above the components it points at.
    ///
    /// `OFFSET << 12` is exactly `word & 0xFFFF_F000`, so a wrapping 32-bit add IS the sign-extended
    /// arithmetic here; the sign only becomes visible in an address space wider than 32 bits, which
    /// IHI 0031G, C2.6.1 rules out for every Cortex-M profile.
    pub fn component_address(self, table_base: u32) -> u32 {
        table_base.wrapping_add(self.word & 0xffff_f000)
    }
}

/// Reads and decodes the identification registers of the component at `address`.
///
/// Reads `CIDR0`-`CIDR3` first and refuses on the preamble, because everything after that is only
/// meaningful if this is a component at all. ADIv5.2 (IHI 0031G), C2.6.1 requires a debugger to
/// treat *"the four words starting at (base address + 0xFF0) are not valid Component ID registers"*
/// as non-fatal, which is why this returns the words it saw rather than a bare refusal.
///
/// # Round trips
///
/// Up to eleven word reads per component, one probe transaction each, because [`CoreMemory`] has no
/// block read -- and it has none deliberately, since it is the seam a high-level probe implements
/// with three methods. On a Cortex-M that is a few dozen transactions for the whole walk, which is
/// under a second and is spent once. If a caller ever needs it faster, the three runs it reads are
/// each contiguous (`CIDR0`-`CIDR3`, `PIDR0`-`PIDR3`, `PIDR4`) and
/// [`TargetAccess::read_words_into`](super::TargetAccess::read_words_into) collapses each into one
/// transfer -- at the cost of a walk that only a `TargetAccess` implementor can drive.
pub fn read_component<M: CoreMemory + ?Sized>(
    memory: &mut M,
    address: u32,
    depth: u8,
) -> Result<Component, Problem> {
    let problem = |at: u32, cause: Cause| Problem { address: at, depth, cause };
    let mut read = |offset: u32| {
        let at = address.wrapping_add(offset);
        memory.read_word(at).map_err(|error| problem(at, Cause::Unreadable(error)))
    };

    let cidr = [read(CIDR0)?, read(CIDR0 + 4)?, read(CIDR0 + 8)?, read(CIDR0 + 12)?];
    let preamble_holds = cidr[0] & 0xff == 0x0d
        && cidr[1] & 0x0f == 0x00
        && cidr[2] & 0xff == 0x05
        && cidr[3] & 0xff == 0xb1;
    if !preamble_holds {
        return Err(problem(address, Cause::NotAComponent(cidr)));
    }
    let class = Class::from_bits((cidr[1] >> 4) as u8 & 0xf);

    let pidr0 = read(PIDR0)?;
    let pidr1 = read(PIDR0 + 4)?;
    let pidr2 = read(PIDR0 + 8)?;
    let pidr3 = read(PIDR0 + 12)?;
    let pidr4 = read(PIDR4)?;

    let part_0 = pidr0 as u16 & 0xff;
    let part_1 = pidr1 as u16 & 0xf;
    let revision = (pidr2 >> 4) as u8 & 0xf;
    let part = PartNumber {
        twelve_bit: (part_1 << 8) | part_0,
        sixteen_bit: (part_1 << 12) | (part_0 << 4) | u16::from(revision),
    };

    let architecture = match class {
        Class::CoreSight => DeviceArchitecture::decode(read(DEVARCH)?),
        _ => None,
    };
    let type_word = match class {
        Class::CoreSight | Class::RomTable => Some(read(DEVTYPE_OR_MEMTYPE)?),
        _ => None,
    };

    Ok(Component {
        address,
        depth,
        class,
        designer: decode_designer(pidr1, pidr2, pidr4),
        part,
        revision,
        revand: (pidr3 >> 4) as u8 & 0xf,
        cmod: pidr3 as u8 & 0xf,
        size_log2: (pidr4 >> 4) as u8 & 0xf,
        architecture,
        type_word,
    })
}

/// Decodes the JEP106 designer out of `PIDR1`, `PIDR2` and `PIDR4`.
///
/// CoreSight v3.0 (IHI 0029G), A1.5 states the discriminator, and it is not the field-by-field test
/// the register descriptions suggest: *"When the 7-bit identity code in the 11-bit compressed form
/// is not 0x7F, then the 11-bit compressed form is used"*; when it IS `0x7F`, continuation `0x0`
/// means the extended form is present, continuation `0xF` means no designer, and `0x1`-`0xE` are
/// reserved. `0x7F` can never be a real identity because *"The values 0x00 and 0x7F are never
/// allocated to companies by JEDEC"*, which is what makes it usable as an escape.
fn decode_designer(pidr1: u32, pidr2: u32, pidr4: u32) -> Designer {
    if pidr2 & 0b1000 == 0 {
        return Designer::NotJedec;
    }
    let des_0 = (pidr1 >> 4) as u8 & 0xf;
    let des_1 = pidr2 as u8 & 0b111;
    let des_2 = pidr4 as u8 & 0xf;
    let identity = (des_1 << 4) | des_0;
    if identity != 0x7f {
        return Designer::Compressed { continuation: des_2, identity };
    }
    match des_2 {
        0x0 => {
            let des_3 = (pidr4 >> 8) as u16 & 0xffff;
            Designer::Extended { continuation: des_3 >> 7, identity: des_3 as u8 & 0x7f }
        }
        0xf => Designer::Unspecified,
        continuation => Designer::ReservedEscape { continuation },
    }
}

/// Walks the ROM table at `base` and everything it points at.
///
/// `base` comes from the MEM-AP's `BASE` register -- see
/// [`ArmDap::rom_table_base`](super::ArmDap::rom_table_base). ADIv5.2 (IHI 0031G), D1.4 says `BASE`
/// may name a ROM table OR a single debug component directly, and this handles both: the component
/// at `base` is read first and only descended into when it turns out to be a table.
///
/// **Nothing here fails.** Every reachable component is reported and every unreachable one is a
/// [`Problem`], per IHI 0031G, C2.6.1's requirement that these be non-fatal. A caller wanting a
/// hard failure should check `components.is_empty()`, which says something the errors do not: that
/// there was nothing to find rather than that finding it went wrong.
///
/// The cycle guard is a PATH, not a set of everything seen, so a hierarchy in which two tables both
/// point at a third reports that third twice -- once per route to it, which is what the tables say.
/// A cycle is what is refused; reaching one component by two paths is not one.
pub fn walk<M: CoreMemory + ?Sized>(memory: &mut M, base: u32) -> Walk {
    let mut walk = Walk::default();
    let mut visited = Vec::new();
    let _ = descend(memory, base, 0, &mut walk, &mut visited);
    walk
}

/// Reads the component at `address` and, if it is a table, every entry in it.
///
/// Returns `false` when a LIMIT stopped it, which the caller propagates by ending its own entry
/// loop. A per-entry failure -- unreadable, not a component, circular -- returns `true` instead:
/// those say nothing about the sibling entries, and ADIv5.2 (IHI 0031G), C2.6.1 requires the walk
/// to continue past them. **Without the distinction, hitting the cap inside a 960-entry table
/// records one `TooLarge` per remaining entry**, burying the answer in hundreds of copies of the
/// same sentence.
fn descend<M: CoreMemory + ?Sized>(
    memory: &mut M,
    address: u32,
    depth: u8,
    walk: &mut Walk,
    visited: &mut Vec<u32>,
) -> bool {
    if walk.components.len() >= MAX_COMPONENTS || depth > MAX_DEPTH {
        walk.problems.push(Problem { address, depth, cause: Cause::TooLarge });
        return false;
    }
    if visited.contains(&address) {
        walk.problems.push(Problem { address, depth, cause: Cause::CircularReference(address) });
        return true;
    }
    let component = match read_component(memory, address, depth) {
        Ok(component) => component,
        Err(problem) => {
            walk.problems.push(problem);
            return true;
        }
    };
    let is_table = component.is_rom_table();
    let class = component.class;
    walk.components.push(component);
    if !is_table {
        return true;
    }
    let format = match table_format(memory, address, class) {
        Ok(format) => format,
        Err(cause) => {
            walk.problems.push(Problem { address, depth, cause });
            return true;
        }
    };
    visited.push(address);

    let mut offset = 0;
    let mut keep_going = true;
    loop {
        let at = address.wrapping_add(offset);
        let word = match memory.read_word(at) {
            Ok(word) => word,
            Err(error) => {
                walk.problems.push(Problem { address: at, depth, cause: Cause::Unreadable(error) });
                break;
            }
        };
        let entry = RomEntry::decode(word, format);
        if entry.ends_table {
            break;
        }
        if !entry.understood {
            let cause = match format {
                TableFormat::ClassOne => Cause::EightBitFormat(word),
                TableFormat::ClassNine32 => Cause::ReservedEntry(word),
            };
            walk.problems.push(Problem { address: at, depth, cause });
            break;
        }
        if entry.present
            && !descend(memory, entry.component_address(address), depth + 1, walk, visited)
        {
            keep_going = false;
            break;
        }
        if offset >= format.last_entry_offset() {
            break;
        }
        offset += format.entry_stride();
    }
    visited.pop();
    keep_going
}

/// Which entry encoding the ROM table at `address` uses.
///
/// Called only for a component [`Component::is_rom_table`] accepted, so `class` is `RomTable` or
/// `CoreSight` and nothing else reaches the arms below.
///
/// A Class `0x1` table has one encoding and needs no further reads. A Class `0x9` table's entry
/// width is in its own `DEVID`, so this costs one extra transaction -- taken here rather than in
/// [`read_component`] because it is owed only by a table, and most components are not one.
fn table_format<M: CoreMemory + ?Sized>(
    memory: &mut M,
    address: u32,
    class: Class,
) -> Result<TableFormat, Cause> {
    match class {
        Class::CoreSight => {
            let devid =
                memory.read_word(address.wrapping_add(DEVID)).map_err(Cause::Unreadable)?;
            match devid & 0xf {
                0x0 => Ok(TableFormat::ClassNine32),
                _ => Err(Cause::UnsupportedTableFormat(devid)),
            }
        }
        _ => Ok(TableFormat::ClassOne),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A target's memory, as a map, so a walk can be driven over a table nobody has to solder.
    ///
    /// `write_word` and `set_reset` panic rather than returning an error: the walk must never do
    /// either, and a panic makes that an assertion the test suite can fail on rather than a
    /// sentence in a doc comment that nothing checks.
    struct FakeTarget {
        words: HashMap<u32, u32>,
    }

    impl FakeTarget {
        fn new() -> FakeTarget {
            FakeTarget { words: HashMap::new() }
        }

        fn at(mut self, address: u32, word: u32) -> FakeTarget {
            self.words.insert(address, word);
            self
        }

        /// Lays down the four Component ID registers and the five Peripheral ID registers for a
        /// component at `base`, with `class` and a compressed-form designer.
        fn component(self, base: u32, class: u8, continuation: u8, identity: u8, part: u16) -> FakeTarget {
            let des_0 = u32::from(identity & 0xf);
            let des_1 = u32::from(identity >> 4) & 0b111;
            self.at(base + CIDR0, 0x0d)
                .at(base + CIDR0 + 4, u32::from(class) << 4)
                .at(base + CIDR0 + 8, 0x05)
                .at(base + CIDR0 + 12, 0xb1)
                .at(base + PIDR0, u32::from(part & 0xff))
                .at(base + PIDR0 + 4, (des_0 << 4) | u32::from(part >> 8) & 0xf)
                .at(base + PIDR0 + 8, 0b1000 | des_1)
                .at(base + PIDR0 + 12, 0)
                .at(base + PIDR4, u32::from(continuation))
                .at(base + DEVARCH, 0)
                .at(base + DEVID, 0)
                .at(base + DEVTYPE_OR_MEMTYPE, 0)
        }
    }

    /// A `DEVARCH` word with Arm as the architect, `PRESENT` set and revision 0.
    fn arm_devarch(archid: u16) -> u32 {
        (u32::from(ARM_ELEVEN_BIT) << 21) | (1 << 20) | u32::from(archid)
    }

    impl CoreMemory for FakeTarget {
        fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
            self.words.get(&address).copied().ok_or(ProbeError::Ack(crate::Ack::Fault))
        }

        fn write_word(&mut self, _address: u32, _value: u32) -> Result<(), ProbeError> {
            panic!("the ROM table walk must never write to the target");
        }

        fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
            panic!("the ROM table walk must never touch the reset line");
        }
    }

    /// The Armv7-M ROM table, entry for entry, from the Armv7-M ARM (DDI 0403E.d), Table C1-3.
    ///
    /// **The specification states BOTH SIDES of these**: the entry word and the address of the
    /// component it points at. So this is not a test of the decoder against itself -- it is the
    /// document's own arithmetic, and a sign error or an off-by-a-nibble in the shift fails it.
    /// The Armv8-M ARM (DDI 0553B.y), B13.2.1 repeats the same table and adds the PMU row.
    #[test]
    fn armv7m_rom_table_entries_resolve_to_the_addresses_the_arm_names() {
        const ROM: u32 = 0xe00f_f000;
        let stated = [
            (0xfff0_f003u32, 0xe000_e000u32),
            (0xfff0_2003, 0xe000_1000),
            (0xfff0_3003, 0xe000_2000),
            (0xfff0_1003, 0xe000_0000),
            (0xfff0_4003, 0xe000_3000),
        ];
        for (word, expected) in stated {
            let entry = RomEntry::decode(word, TableFormat::ClassOne);
            assert!(entry.present, "{word:#010x} is a fitted component");
            assert!(entry.understood, "{word:#010x} is the 32-bit format");
            assert_eq!(
                entry.component_address(ROM),
                expected,
                "{word:#010x} at {ROM:#010x} must resolve to {expected:#010x}"
            );
        }
    }

    /// The same entries with bit[0] clear -- what the ARM shows for a unit that is not fitted.
    #[test]
    fn an_unfitted_component_is_not_present_but_still_a_valid_entry() {
        let entry = RomEntry::decode(0xfff0_2002, TableFormat::ClassOne);
        assert!(!entry.present);
        assert!(entry.understood);
        assert!(!entry.ends_table, "not-present is not end-of-table");
    }

    /// IHI 0074E, D3.5.17: a Class `0x9` table's `PRESENT` is TWO bits with three defined values,
    /// and the same word means different things under the two formats.
    ///
    /// **`0xFFF02002` is the case that decides it**: Class `0x1` reads bit[1] as `FORMAT` and calls
    /// the entry a valid not-present one; Class `0x9` reads bits[1:0] as `PRESENT == 0b10`, a gap
    /// that is explicitly NOT the end of the table. Both skip it -- and `0x...0003` is the one that
    /// diverges, being present under both but for different reasons.
    #[test]
    fn the_two_formats_read_the_low_bits_differently() {
        let gap = RomEntry::decode(0xfff0_2002, TableFormat::ClassNine32);
        assert!(!gap.present && !gap.ends_table && gap.understood, "PRESENT 0b10 is a gap");

        let present = RomEntry::decode(0xfff0_2003, TableFormat::ClassNine32);
        assert!(present.present && !present.ends_table, "PRESENT 0b11 is present");

        let end = RomEntry::decode(0xfff0_2000, TableFormat::ClassNine32);
        assert!(end.ends_table, "PRESENT 0b00 ends the table whatever the OFFSET holds");
        let same = RomEntry::decode(0xfff0_2000, TableFormat::ClassOne);
        assert!(!same.ends_table && !same.understood);

        let reserved = RomEntry::decode(0xfff0_2001, TableFormat::ClassNine32);
        assert!(!reserved.understood, "PRESENT 0b01 is reserved");
    }

    /// The all-zero word ends a Class `0x1` table, and must not also be reported as a legacy 8-bit
    /// entry -- its `FORMAT` bit is clear because every bit is.
    #[test]
    fn the_class_one_terminator_is_not_an_eight_bit_entry() {
        let entry = RomEntry::decode(0, TableFormat::ClassOne);
        assert!(entry.ends_table);
        assert!(entry.understood, "the terminator is not a format complaint");
    }

    /// The last entry slot is 1,792 bytes apart in the two formats (IHI 0074E, D2.2.1 and D3.5.17).
    #[test]
    fn the_two_formats_end_at_different_offsets() {
        assert_eq!(TableFormat::ClassOne.last_entry_offset(), 0xefc);
        assert_eq!(TableFormat::ClassNine32.last_entry_offset(), 0x7fc);
    }

    /// IHI 0029G, Example A1-1: Arm is continuation `0x4`, identity `0x3B`, which is `0x23B` in the
    /// 11-bit field the DP also uses.
    #[test]
    fn arm_decodes_from_the_compressed_form_the_specification_gives() {
        let designer = decode_designer(0xb0, 0b1011, 0x4);
        assert_eq!(designer, Designer::Compressed { continuation: 0x4, identity: 0x3b });
        assert_eq!(designer.jep106(), Some((4, 0x3b)));
        assert_eq!(designer.as_eleven_bit(), Some(ARM_ELEVEN_BIT));
    }

    /// IHI 0029G, Example A1-1 again, the other way round: Arm encoded in the 16-bit extended form
    /// must decode to the SAME bank and identity as the compressed one.
    ///
    /// **This is the case a lookup table keyed on one form gets wrong**, and the specification puts
    /// both spellings of one vendor side by side precisely because both are legal.
    #[test]
    fn the_extended_form_of_arm_decodes_to_the_same_designer() {
        let des_3 = (0x004u32 << 7) | 0x3b;
        let designer = decode_designer(0xf0, 0b1111, (des_3 << 8) | 0x0);
        assert_eq!(
            designer,
            Designer::Extended { continuation: 0x004, identity: 0x3b },
            "the escape names the extended form and DES_3 carries it"
        );
        assert_eq!(designer.jep106(), Some((4, 0x3b)), "same bank and id as the compressed form");
        assert_eq!(designer.as_eleven_bit(), Some(ARM_ELEVEN_BIT));
    }

    /// IHI 0029G, Example A1-2: bank 17 is past the compressed form's reach, so only the extended
    /// form can express it -- and `as_eleven_bit` must refuse rather than truncate.
    #[test]
    fn a_designer_past_bank_sixteen_has_no_eleven_bit_spelling() {
        let des_3 = (0x010u32 << 7) | 0x2a;
        let designer = decode_designer(0xf0, 0b1111, (des_3 << 8) | 0x0);
        assert_eq!(designer, Designer::Extended { continuation: 0x010, identity: 0x2a });
        assert_eq!(designer.as_eleven_bit(), None);
    }

    /// IHI 0029G, A1.5: identity `0x7F` with continuation `0xF` is "the designer is not specified",
    /// and `0x1`-`0xE` are reserved. Neither is a designer, and neither may be reported as one.
    #[test]
    fn the_two_non_designer_escapes_are_distinguished() {
        assert_eq!(decode_designer(0xf0, 0b1111, 0xf), Designer::Unspecified);
        assert_eq!(
            decode_designer(0xf0, 0b1111, 0x5),
            Designer::ReservedEscape { continuation: 0x5 }
        );
        assert_eq!(decode_designer(0xf0, 0b1111, 0xf).jep106(), None);
        assert_eq!(decode_designer(0xf0, 0b1111, 0x5).jep106(), None);
    }

    /// IHI 0029G, B2.2.2.1: `PIDR2.JEDEC` clear means the designer fields are not a JEP106 code.
    /// Decoding them anyway would name a vendor out of bits that do not mean one.
    #[test]
    fn a_cleared_jedec_bit_stops_the_designer_decode() {
        assert_eq!(decode_designer(0xb0, 0b0011, 0x4), Designer::NotJedec);
    }

    /// A designer whose identity happens to end in `0xF` is not the escape. The escape is the WHOLE
    /// 7-bit identity being `0x7F`, and testing `DES_0 == 0xF` alone would swallow 7 real vendors.
    #[test]
    fn only_the_full_seven_bit_identity_is_the_escape() {
        let designer = decode_designer(0xf0, 0b1010, 0x0);
        assert_eq!(designer, Designer::Compressed { continuation: 0x0, identity: 0x2f });
    }

    /// IHI 0029G, B2.2.2.1: the 12-bit and 16-bit readings of a part number differ by a nibble, and
    /// nothing in the registers says which the designer meant.
    #[test]
    fn both_part_number_readings_are_carried() {
        let mut target = FakeTarget::new().component(0x1000, 0x9, 0x4, 0x3b, 0x4c4);
        target.words.insert(0x1000 + PIDR0 + 8, 0b1011 | (0x2 << 4));
        let component = read_component(&mut target, 0x1000, 0).expect("a valid component");
        assert_eq!(component.part.twelve_bit, 0x4c4);
        assert_eq!(component.part.sixteen_bit, 0x4c42);
        assert_eq!(component.revision, 0x2);
    }

    /// The preamble check must reject, because a faulting or unmapped region reads as something and
    /// that something must not become a component.
    #[test]
    fn a_wrong_preamble_is_not_a_component() {
        let mut target = FakeTarget::new()
            .at(0x2000 + CIDR0, 0x0d)
            .at(0x2000 + CIDR0 + 4, 0x10)
            .at(0x2000 + CIDR0 + 8, 0x05)
            .at(0x2000 + CIDR0 + 12, 0x00);
        let problem = read_component(&mut target, 0x2000, 0).expect_err("preamble does not hold");
        assert!(matches!(problem.cause, Cause::NotAComponent(_)));
    }

    /// Only `CIDR1`'s LOW nibble is preamble. A check over the whole byte would reject every
    /// component whose class is not zero, which is every component that exists.
    #[test]
    fn the_class_nibble_is_not_part_of_the_preamble() {
        for class in [0x1u8, 0x9, 0xe, 0xf] {
            let mut target = FakeTarget::new().component(0x3000, class, 0x4, 0x3b, 0x000);
            let component = read_component(&mut target, 0x3000, 0).expect("a valid component");
            assert_eq!(component.class.bits(), class);
        }
    }

    /// A whole Armv7-M-shaped table: a Class `0x1` ROM table at `0xE00FF000` listing four Arm
    /// components at the addresses the ARM states, terminated by `0x00000000`.
    #[test]
    fn a_cortex_m_shaped_table_walks_to_its_components() {
        const ROM: u32 = 0xe00f_f000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x1, 0x4, 0x3b, 0x4c4)
            .at(ROM + 0x000, 0xfff0_f003)
            .at(ROM + 0x004, 0xfff0_2003)
            .at(ROM + 0x008, 0xfff0_3003)
            .at(ROM + 0x00c, 0xfff0_1003)
            .at(ROM + 0x010, 0x0000_0000)
            .component(0xe000_e000, 0x9, 0x4, 0x3b, 0x008)
            .component(0xe000_1000, 0x9, 0x4, 0x3b, 0x002)
            .component(0xe000_2000, 0x9, 0x4, 0x3b, 0x003)
            .component(0xe000_0000, 0x9, 0x4, 0x3b, 0x001);

        let walk = walk(&mut target, ROM);
        assert_eq!(walk.problems, vec![], "nothing in this table is unreachable");
        let addresses: Vec<u32> = walk.components.iter().map(|c| c.address).collect();
        assert_eq!(addresses, vec![ROM, 0xe000_e000, 0xe000_1000, 0xe000_2000, 0xe000_0000]);
        assert_eq!(walk.components[0].depth, 0, "the table BASE names is depth 0");
        assert_eq!(walk.components[1].depth, 1);
        assert_eq!(walk.sole_designer(), Some(Designer::Compressed { continuation: 4, identity: 0x3b }));
    }

    /// IHI 0074E, D2.2.2: a not-present entry is SKIPPED, and the table continues past it. A walk
    /// that stopped there would report a shorter table on exactly the parts where a component is
    /// disabled by a tie-off -- silently, and identically every run.
    #[test]
    fn a_not_present_entry_does_not_end_the_table() {
        const ROM: u32 = 0xe00f_f000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x1, 0x4, 0x3b, 0x4c4)
            .at(ROM + 0x000, 0xfff0_2002)
            .at(ROM + 0x004, 0xfff0_f003)
            .at(ROM + 0x008, 0x0000_0000)
            .component(0xe000_e000, 0x9, 0x4, 0x3b, 0x008);

        let walk = walk(&mut target, ROM);
        let addresses: Vec<u32> = walk.components.iter().map(|c| c.address).collect();
        assert_eq!(addresses, vec![ROM, 0xe000_e000], "the entry after the gap is still read");
        assert_eq!(walk.problems, vec![], "a not-present entry is not a problem");
    }

    /// An entry pointing at a faulting location is non-fatal, per IHI 0031G, C2.6.1 -- the rest of
    /// the table is still read and reported.
    #[test]
    fn an_entry_pointing_at_nothing_does_not_stop_the_walk() {
        const ROM: u32 = 0xe00f_f000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x1, 0x4, 0x3b, 0x4c4)
            .at(ROM + 0x000, 0xfff0_2003)
            .at(ROM + 0x004, 0xfff0_f003)
            .at(ROM + 0x008, 0x0000_0000)
            .component(0xe000_e000, 0x9, 0x4, 0x3b, 0x008);

        let walk = walk(&mut target, ROM);
        assert_eq!(walk.components.len(), 2, "the table and the component that IS there");
        assert_eq!(walk.problems.len(), 1);
        assert_eq!(walk.problems[0].address, 0xe000_1000 + CIDR0);
        assert!(matches!(walk.problems[0].cause, Cause::Unreadable(_)));
    }

    /// IHI 0074E, D2.2.1: an entry at `0xEFC` is the last one whatever it holds. Running past it
    /// reads the reserved area at `0xF00`, which on a Class `0x9` ROM table is `ITCTRL` -- a
    /// READ-WRITE register whose contents decode as a perfectly plausible entry.
    ///
    /// **The fixture puts a REACHABLE component behind that word on purpose.** With the reserved
    /// area left unmapped, a walk missing the `0xEFC` stop faults at `0xF00` and stops there
    /// anyway, so the test would pass for a reason that has nothing to do with the guard -- two
    /// mechanisms covering one case, and the perturbation proving nothing. Behind a word that
    /// resolves, only the guard can keep `0x2000_5000` out of the result.
    #[test]
    fn the_table_ends_at_the_last_entry_offset_even_without_a_marker() {
        const ROM: u32 = 0x2000_0000;
        const BEYOND: u32 = 0x2000_5000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x1, 0x4, 0x3b, 0x4c4)
            .component(BEYOND, 0x9, 0x4, 0x3b, 0x123);
        for offset in (0..=TableFormat::ClassOne.last_entry_offset()).step_by(4) {
            target.words.insert(ROM + offset, 0xfff0_0002);
        }
        target.words.insert(ROM + 0xf00, 0x0000_5003);

        let walk = walk(&mut target, ROM);
        let addresses: Vec<u32> = walk.components.iter().map(|c| c.address).collect();
        assert_eq!(addresses, vec![ROM], "the word at 0xF00 is not a ROM table entry");
        assert_eq!(walk.problems, vec![], "stopping at 0xEFC is not an error");
    }

    /// The 8-bit legacy format (`FORMAT == 0` on a non-zero entry) is refused rather than misparsed.
    #[test]
    fn an_eight_bit_format_entry_stops_that_table_and_says_so() {
        const ROM: u32 = 0x2000_0000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x1, 0x4, 0x3b, 0x4c4)
            .at(ROM + 0x000, 0xfff0_f001);

        let walk = walk(&mut target, ROM);
        assert_eq!(walk.components.len(), 1);
        assert_eq!(walk.problems.len(), 1);
        assert!(matches!(walk.problems[0].cause, Cause::EightBitFormat(0xfff0_f001)));
    }

    /// IHI 0031G, D1.2: a Class `0x9` component whose `DEVARCH.ARCHID` says "CoreSight ROM" IS a
    /// ROM table, and a walk that only descended into Class `0x1` would report a subset.
    #[test]
    fn a_class_nine_rom_table_is_descended_into() {
        const OUTER: u32 = 0x2000_0000;
        const INNER: u32 = 0x2001_0000;
        let mut target = FakeTarget::new()
            .component(OUTER, 0x9, 0x4, 0x3b, 0xaf7)
            .at(OUTER + DEVARCH, arm_devarch(CORESIGHT_ROM_ARCHID))
            .at(OUTER + 0x000, 0x0001_0003)
            .at(OUTER + 0x004, 0x0000_0000)
            .component(INNER, 0x9, 0x4, 0x3b, 0x008);

        let walk = walk(&mut target, OUTER);
        let addresses: Vec<u32> = walk.components.iter().map(|c| c.address).collect();
        assert_eq!(addresses, vec![OUTER, INNER]);
        assert_eq!(
            architecture_name(ARM_ELEVEN_BIT, 0x0af7),
            Some("CoreSight ROM"),
            "and the ARCHID that made it one is named"
        );
    }

    /// A Class `0x9` table's entries end at `0x7FC`, not at the Class `0x1` table's `0xEFC`.
    ///
    /// **The fixture puts a reachable component behind `0x800` for the same reason the Class `0x1`
    /// version does**: with the area after the entries left unmapped, a walk using the wrong
    /// last-entry offset faults there and stops anyway, so the test would pass without the offset
    /// ever being consulted. Behind a word that resolves, only the offset keeps `BEYOND` out.
    ///
    /// On real silicon that word is worse than unmapped: IHI 0074E, Table D3-1 puts `SYSPCR` and
    /// `DBGPCR` in `0xA00`-`0xB7C`, which are READ-WRITE, so a runaway walk decodes whatever
    /// software last wrote into a power-control register as a component address.
    #[test]
    fn a_class_nine_table_ends_at_its_own_last_entry_offset() {
        const ROM: u32 = 0x2000_0000;
        const BEYOND: u32 = 0x2000_9000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x9, 0x4, 0x3b, 0xaf7)
            .component(BEYOND, 0x9, 0x4, 0x3b, 0x123);
        target.words.insert(ROM + DEVARCH, arm_devarch(CORESIGHT_ROM_ARCHID));
        for offset in (0..=TableFormat::ClassNine32.last_entry_offset()).step_by(4) {
            target.words.insert(ROM + offset, 0xfff0_0002);
        }
        target.words.insert(ROM + 0x800, 0x0000_9003);

        let walk = walk(&mut target, ROM);
        let addresses: Vec<u32> = walk.components.iter().map(|c| c.address).collect();
        assert_eq!(addresses, vec![ROM], "the word at 0x800 is not a ROM table entry");
        assert_eq!(walk.problems, vec![], "stopping at 0x7FC is not an error");
    }

    /// A Class `0x9` gap entry (`PRESENT == 0b10`) is skipped and the table continues -- the same
    /// rule as Class `0x1`, spelled in the field rather than left to the word being non-zero.
    #[test]
    fn a_class_nine_gap_does_not_end_the_table() {
        const ROM: u32 = 0x2000_0000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x9, 0x4, 0x3b, 0xaf7)
            .component(0x2000_2000, 0x9, 0x4, 0x3b, 0x008);
        target.words.insert(ROM + DEVARCH, arm_devarch(CORESIGHT_ROM_ARCHID));
        target.words.insert(ROM + 0x000, 0x0000_1002);
        target.words.insert(ROM + 0x004, 0x0000_2003);
        target.words.insert(ROM + 0x008, 0x0000_0000);

        let walk = walk(&mut target, ROM);
        let addresses: Vec<u32> = walk.components.iter().map(|c| c.address).collect();
        assert_eq!(addresses, vec![ROM, 0x2000_2000]);
        assert_eq!(walk.problems, vec![]);
    }

    /// IHI 0074E, D3.5.10: `DEVID.FORMAT` `0x1` is the 64-bit entry format, whose `OFFSET` is
    /// bits[63:12] and does not fit the `u32` addresses this walk carries. ADIv5.2 (IHI 0031G),
    /// D1.2 forbids it on an ADIv5 implementation; a target that does it anyway is refused by name
    /// rather than read as though its entries were 32 bits.
    ///
    /// The entry it would read points at a component that RESOLVES, so a walk that ignored `DEVID`
    /// would list that component rather than fault -- which is what makes the refusal the only
    /// thing keeping it out of the result.
    #[test]
    fn a_sixty_four_bit_class_nine_table_is_refused_by_name() {
        const ROM: u32 = 0x2000_0000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x9, 0x4, 0x3b, 0xaf7)
            .component(0x2000_1000, 0x9, 0x4, 0x3b, 0x008);
        target.words.insert(ROM + DEVARCH, arm_devarch(CORESIGHT_ROM_ARCHID));
        target.words.insert(ROM + DEVID, 0x1);
        target.words.insert(ROM + 0x000, 0x0000_1003);
        target.words.insert(ROM + 0x004, 0x0000_0000);

        let walk = walk(&mut target, ROM);
        let addresses: Vec<u32> = walk.components.iter().map(|c| c.address).collect();
        assert_eq!(addresses, vec![ROM], "the table is reported, its entries are not read");
        assert_eq!(walk.problems.len(), 1);
        assert_eq!(walk.problems[0].cause, Cause::UnsupportedTableFormat(0x1));
    }

    /// A Class `0x9` component that is NOT a ROM table must not be descended into, or every
    /// ordinary component's first register would be read as a table entry.
    ///
    /// **The word at offset `0x000` resolves to a real component here on purpose.** Pointed at
    /// unmapped memory it would fault, and the walk would stop for a reason that has nothing to do
    /// with the `DEVARCH` test -- so the test would go green with the discriminator removed and
    /// prove only that the fixture is small. Pointed at something that reads, the only thing
    /// keeping `NOT_A_COMPONENT_OF_THIS_WALK` out of the result is the discriminator.
    #[test]
    fn an_ordinary_coresight_component_is_not_descended_into() {
        const BASE: u32 = 0x2000_0000;
        const WOULD_BE_FOUND: u32 = 0x2000_1000;
        let mut target = FakeTarget::new()
            .component(BASE, 0x9, 0x4, 0x3b, 0x002)
            .component(WOULD_BE_FOUND, 0x9, 0x4, 0x3b, 0x008)
            .at(BASE + DEVARCH, arm_devarch(0x1a02))
            .at(BASE + 0x000, 0x0000_1003)
            .at(BASE + 0x004, 0x0000_0000);

        let walk = walk(&mut target, BASE);
        let addresses: Vec<u32> = walk.components.iter().map(|c| c.address).collect();
        assert_eq!(addresses, vec![BASE], "the DWT, and nothing read out of its CTRL");
        assert_eq!(walk.problems, vec![]);
    }

    /// A table that points at itself is refused rather than walked forever.
    #[test]
    fn a_circular_reference_is_caught() {
        const ROM: u32 = 0x2000_0000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x1, 0x4, 0x3b, 0x4c4)
            .at(ROM + 0x000, 0x0000_0003)
            .at(ROM + 0x004, 0x0000_0000);

        let walk = walk(&mut target, ROM);
        assert!(walk.problems.iter().any(|p| matches!(p.cause, Cause::CircularReference(_))));
        assert_eq!(walk.components.len(), 1, "and the table is listed once, not once per visit");
    }

    /// `sole_designer` must not answer when the components disagree -- that disagreement is the
    /// signal that a vendor designed some of them.
    #[test]
    fn a_mixed_designer_table_has_no_sole_designer() {
        const ROM: u32 = 0x2000_0000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x1, 0x0, 0x1f, 0xcd0)
            .at(ROM + 0x000, 0x0001_0003)
            .at(ROM + 0x004, 0x0000_0000)
            .component(0x2001_0000, 0x9, 0x4, 0x3b, 0x008);

        let walk = walk(&mut target, ROM);
        assert_eq!(walk.sole_designer(), None);
        assert_eq!(
            walk.designers(),
            vec![
                (Designer::Compressed { continuation: 0x0, identity: 0x1f }, 1),
                (Designer::Compressed { continuation: 0x4, identity: 0x3b }, 1),
            ]
        );
    }

    /// A debug port that is not a DPv3 is refused BEFORE its base pointer is believed.
    ///
    /// **THE FIXTURE IS A REAL DEBUG-PORT ID, AND THAT IS THE WHOLE POINT.** On a pre-DPv3 port,
    /// register offset `0x0` reads `DPIDR` whatever `SELECT.DPBANKSEL` holds -- so the words that
    /// reach this decode are three copies of the id itself, and the id's low bit is set. Without
    /// the version gate that is `VALID`, and `0x2ba01477` masked to a 4KB boundary is
    /// `0x2ba01000`: an address the walk would go and read. **A missing gate fails as a plausible
    /// number here, not as an error.**
    #[test]
    fn a_pre_dpv3_port_is_refused_rather_than_decoded_as_an_address() {
        const ID: u32 = 0x2ba0_1477;
        assert_eq!(
            decode_base_pointer(ID, ID, ID, ID),
            DebugBasePointer::NotDpv3 { version: 0x1 },
            "a DPv1 port has no BASEPTR and its id must not be read as one"
        );
        assert_ne!(
            decode_base_pointer(ID, ID, ID, ID),
            DebugBasePointer::At { address: 0x2ba0_1000, address_bits: 0x77 },
            "which is the address an ungated decode would have produced"
        );
    }

    /// The address is assembled from the two halves the specification names, and the reserved bits
    /// between them are dropped.
    ///
    /// IHI 0074E, B2.2.2: `BASEPTR1.PTR` is bits[63:32], `BASEPTR0` bits[31:12] are bits[31:12],
    /// and bits[11:1] of `BASEPTR0` are RES0. A target leaving something in those reserved bits
    /// would otherwise shift the address by up to 4 KB less one and it would still look like one.
    #[test]
    fn the_two_halves_assemble_and_the_reserved_bits_are_dropped() {
        let dpidr = u32::from(DPV3) << 12;
        assert_eq!(
            decode_base_pointer(dpidr, 0x20, 0xe00f_ffff, 0),
            DebugBasePointer::At { address: 0xe00f_f000, address_bits: 0x20 },
            "bits[11:1] are reserved and contribute nothing"
        );
        assert_eq!(
            decode_base_pointer(dpidr, 0x28, 0x1000_0001, 0x0000_00ff),
            DebugBasePointer::At { address: 0x0000_00ff_1000_0000, address_bits: 0x28 },
            "the upper word is bits[63:32], which a 40-bit ASIZE says is meaningful"
        );
    }

    /// `VALID` clear says the port reaches no debug components, which is an answer.
    #[test]
    fn a_port_with_no_valid_base_says_absent_rather_than_address_zero() {
        let dpidr = u32::from(DPV3) << 12;
        assert_eq!(decode_base_pointer(dpidr, 0x20, 0xe00f_f000, 0), DebugBasePointer::Absent);
        assert_eq!(
            decode_base_pointer(dpidr, 0x0c, 0x0000_0001, 0),
            DebugBasePointer::At { address: 0, address_bits: 0x0c },
            "a 12-bit ASIZE puts a real component at zero, which is not the same as Absent"
        );
    }

    /// The vendor is the non-Arm designer, and it is chosen by WHOSE it is rather than by counting.
    ///
    /// **The fixture inverts the majority on purpose.** A vendor with more components than Arm is
    /// the case a minority-count rule gets wrong, and it is not hypothetical -- a system ROM table
    /// naming several vendor blocks is an ordinary shape. Only a rule that reads Arm's code can
    /// produce the green here.
    #[test]
    fn the_vendor_is_the_designer_that_is_not_arms_even_in_the_majority() {
        const ROM: u32 = 0x2000_0000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x1, 0x0, 0x1f, 0xcd0)
            .at(ROM + 0x000, 0x0001_0003)
            .at(ROM + 0x004, 0x0002_0003)
            .at(ROM + 0x008, 0x0003_0003)
            .at(ROM + 0x00c, 0x0000_0000)
            .component(0x2001_0000, 0x9, 0x0, 0x1f, 0x001)
            .component(0x2002_0000, 0x9, 0x0, 0x1f, 0x002)
            .component(0x2003_0000, 0x9, 0x4, 0x3b, 0x008);

        let walk = walk(&mut target, ROM);
        assert_eq!(walk.components.len(), 4, "the fixture resolved as built");
        assert_eq!(
            walk.vendor_designer(),
            VendorDesigner::One(Designer::Compressed { continuation: 0x0, identity: 0x1f }),
            "the vendor is the non-Arm designer, whichever has more components"
        );
    }

    /// A part whose vendor designed no CoreSight component of its own answers Arm, and that is an
    /// answer.
    #[test]
    fn a_table_that_is_arm_throughout_says_so_rather_than_naming_arm_as_the_vendor() {
        const ROM: u32 = 0x2000_0000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x1, 0x4, 0x3b, 0x4c0)
            .at(ROM + 0x000, 0x0001_0003)
            .at(ROM + 0x004, 0x0000_0000)
            .component(0x2001_0000, 0x9, 0x4, 0x3b, 0x008);

        let walk = walk(&mut target, ROM);
        assert_eq!(walk.vendor_designer(), VendorDesigner::ArmThroughout);
    }

    /// Two non-Arm designers is refused rather than resolved to the first one seen.
    #[test]
    fn two_vendors_in_one_walk_choose_neither() {
        const ROM: u32 = 0x2000_0000;
        let mut target = FakeTarget::new()
            .component(ROM, 0x1, 0x0, 0x1f, 0xcd0)
            .at(ROM + 0x000, 0x0001_0003)
            .at(ROM + 0x004, 0x0002_0003)
            .at(ROM + 0x008, 0x0000_0000)
            .component(0x2001_0000, 0x9, 0x0, 0x20, 0x001)
            .component(0x2002_0000, 0x9, 0x4, 0x3b, 0x008);

        let walk = walk(&mut target, ROM);
        assert_eq!(walk.vendor_designer(), VendorDesigner::Several);
    }

    /// The two spellings of one designer must reach the same row.
    ///
    /// IHI 0029G Example A1-1 gives Arm both ways, so a component that spells its designer in the
    /// extended form is the same vendor as one that spells it compressed. A table keyed on the
    /// raw register fields rather than on [`Designer::jep106`] would name one and not the other,
    /// and the two components would sit in one walk looking like two vendors.
    #[test]
    fn one_designer_spelled_two_ways_reaches_one_row() {
        let compressed = Designer::Compressed { continuation: 0x4, identity: 0x3b };
        let extended = Designer::Extended { continuation: 0x4, identity: 0x3b };
        for form in [compressed, extended] {
            let (continuation, identity) = form.jep106().expect("both forms carry a code");
            assert_eq!(designer_name(continuation, identity), Some("Arm"), "{form:?}");
        }
    }

    /// The 11-bit entry point must decode the field the way `DPIDR` and `DEVARCH` encode it.
    ///
    /// **This is the encoding that ties the walk to the debug port**: `as_eleven_bit` builds the
    /// value and this takes it apart, so a walk's designer and a `DPIDR.DESIGNER` are comparable.
    /// Getting the split backwards names a different vendor rather than none, which is the failure
    /// that would not look like one.
    #[test]
    fn the_eleven_bit_form_splits_where_the_register_does() {
        let arm = Designer::Compressed { continuation: 0x4, identity: 0x3b };
        assert_eq!(arm.as_eleven_bit(), Some(ARM_ELEVEN_BIT), "0x23B, per IHI 0029G B2.3.4.1");
        assert_eq!(designer_name_of_eleven_bit(ARM_ELEVEN_BIT), Some("Arm"));
        assert_eq!(designer_name_of_eleven_bit(0x01f), Some("Atmel (Microchip)"));
        assert_eq!(designer_name_of_eleven_bit(0x21f), None, "bank 4, id 0x1F is a different code");
    }

    /// An unnamed code answers `None` rather than a plausible name.
    ///
    /// The codes below were read from real ROM tables and no row sources them. **`None` has to be
    /// distinguishable from a failed read** -- a walk that could not reach a component reports a
    /// [`Problem`], and a component whose designer this table cannot name is a successful read.
    #[test]
    fn a_code_with_no_sourced_row_is_not_named() {
        for (continuation, identity) in [(0x2, 0x44), (0x4, 0x23), (0x9, 0x13)] {
            assert_eq!(
                designer_name(continuation, identity),
                None,
                "bank {continuation}, id {identity:#04x} has no sourced row"
            );
        }
    }

    /// IHI 0029G, B2.3.4.1 makes `DEVARCH` optional: `PRESENT` clear means the whole word is RAZ.
    #[test]
    fn an_absent_devarch_is_none_rather_than_a_zero_architecture() {
        assert_eq!(DeviceArchitecture::decode(0), None);
        let present = DeviceArchitecture::decode((u32::from(ARM_ELEVEN_BIT) << 21) | (1 << 20) | (3 << 16) | 0x1a02);
        assert_eq!(
            present,
            Some(DeviceArchitecture { architect: ARM_ELEVEN_BIT, revision: 3, archid: 0x1a02 })
        );
    }

    /// The ARCHID names are Arm's, and mean nothing under another architect.
    #[test]
    fn an_archid_is_only_named_when_arm_is_the_architect() {
        assert_eq!(architecture_name(ARM_ELEVEN_BIT, 0x1a02), Some("DWT"));
        assert_eq!(architecture_name(0x020, 0x1a02), None);
        assert_eq!(architecture_name(ARM_ELEVEN_BIT, 0xffff), None, "the table is not exhaustive");
    }

    /// A table naming more components than the walk will report stops ONCE and says so once.
    ///
    /// **The count matters as much as the stop.** Before the limit propagated out of `descend`,
    /// every remaining entry of the table produced its own `TooLarge`, so a table of 960 buried
    /// the finding under 700 copies of it -- a report that is technically complete and unreadable.
    #[test]
    fn the_component_cap_stops_the_walk_once_rather_than_per_entry() {
        const ROM: u32 = 0x2000_0000;
        let mut target = FakeTarget::new().component(ROM, 0x1, 0x4, 0x3b, 0x4c4);
        for index in 0..300u32 {
            let at = ROM + (index + 1) * 0x1000;
            target.words.insert(ROM + index * 4, ((index + 1) * 0x1000) | 0b11);
            target = target.component(at, 0x9, 0x4, 0x3b, 0x008);
        }

        let walk = walk(&mut target, ROM);
        assert_eq!(walk.components.len(), MAX_COMPONENTS, "reported up to the cap");
        assert_eq!(walk.problems.len(), 1, "and said so exactly once");
        assert_eq!(walk.problems[0].cause, Cause::TooLarge);
    }

    /// A chain of tables deeper than the walk follows stops, rather than recursing until the stack
    /// runs out. ADIv6.0 (IHI 0074E), D1 prohibits circular references; it does not bound depth.
    #[test]
    fn a_chain_deeper_than_the_limit_stops() {
        const FIRST: u32 = 0x2000_0000;
        let mut target = FakeTarget::new();
        for level in 0..(u32::from(MAX_DEPTH) + 4) {
            let at = FIRST + level * 0x1000;
            target = target.component(at, 0x1, 0x4, 0x3b, 0x4c4);
            target.words.insert(at, 0x0000_1003);
            target.words.insert(at + 4, 0x0000_0000);
        }

        let walk = walk(&mut target, FIRST);
        assert_eq!(walk.components.len(), usize::from(MAX_DEPTH) + 1, "depths 0..=MAX_DEPTH");
        assert_eq!(walk.problems.len(), 1);
        assert_eq!(walk.problems[0].cause, Cause::TooLarge);
        assert_eq!(walk.problems[0].depth, MAX_DEPTH + 1);
    }

    /// The `BASE` word an Armv7-M part presents: the ROM table at `0xE00FF000`, ADIv5 format, debug
    /// entry present. `0xE00FF000 | Format | P` is `0xE00FF003`.
    #[test]
    fn the_cortex_m_base_word_decodes_to_the_rom_table() {
        assert_eq!(decode_base(0xe00f_f003), DebugBase::At(0xe00f_f000));
    }

    /// IHI 0031G, C2.6.1: `0xFFFFFFFF` is the legacy "no debug entries" value, and `P` clear in the
    /// ADIv5 format says the same thing.
    #[test]
    fn both_spellings_of_absent_are_recognized() {
        assert_eq!(decode_base(0xffff_ffff), DebugBase::Absent);
        assert_eq!(decode_base(0xe00f_f002), DebugBase::Absent, "Format set, P clear");
    }

    /// The legacy format's bit[0] is RAZ, not a presence bit -- so a legacy `BASE` naming a real
    /// address has `P == 0` and must NOT be read as absent.
    #[test]
    fn a_legacy_base_with_a_real_address_is_not_absent() {
        assert_eq!(decode_base(0xe00f_f000), DebugBase::At(0xe00f_f000));
    }

    /// An unpopulated AP reads back zero to everything, which is a different answer from "this AP
    /// has no debug components".
    #[test]
    fn an_all_zero_base_is_its_own_answer() {
        assert_eq!(decode_base(0), DebugBase::Zero);
    }
}
