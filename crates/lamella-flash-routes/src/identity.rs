//! Naming the part on the other end of a probe, without being told which part to expect.

use lamella_cmsis_dap_sam::{
    SAM3X_CHIPID_CIDR, SAM4_CHIPID_CIDR, SAM4_CHIPID_EXID, SAM_DSU_DID, SamIdentify,
    sam3x_identify, sam4_identify,
};
use lamella_cmsis_dap_stm32::{
    STM32C0_DBGMCU_IDCODE, STM32C0_PARTS, STM32H7_DBGMCU_IDC, STM32H7_PARTS,
    STM32L0_DBGMCU_IDCODE, STM32L0_PARTS, STM32L4_DBGMCU_IDCODE, STM32L4_PARTS,
    STM32U5_DBGMCU_IDCODE, STM32U5_PARTS, stm32_dev_id,
};
use lamella_probe_core::TargetAccess;
use lamella_probe_core::coresight::{Designer, VendorDesigner, Walk, designer_name};

/// How a reading off one register is turned into a part name.
///
/// **A VARIANT PER DECODER, AND THE MATCH THAT CONSUMES IT IS EXHAUSTIVE**, so a route cannot name
/// a decoder that nothing applies. The decoders themselves live in the driver crates, beside the
/// tables that source them: this says WHICH one applies, never how it works.
#[derive(Clone, Copy)]
pub enum Naming {
    /// The DSU `DID` a SAM D, E, L or C part answers.
    ///
    /// The read is the driver crate's, not a bare word read, because a part locked by the NVMCTRL
    /// security bit refuses the DSU's internal view and answers the same word through its external
    /// one -- and a route that read the address directly would report a locked part as absent.
    ///
    /// WARNING: **[`Reading::address`] is therefore the address this route NAMES**, and on a locked
    /// part the word came from the external alias 0x100 above it. The driver does not report which
    /// view answered, and inferring it from the value would be a guess.
    SamDsu,
    /// The SAM4 `CHIPID` pair: `CIDR` at the route's address and `EXID` in the word after it.
    Sam4Chipid,
    /// The SAM3X and SAM3A `CHIPID`, whose member is carried by `CIDR` alone.
    Sam3xChipid,
    /// An STM32 `DBGMCU` identity register, read against one family's `DEV_ID` table.
    ///
    /// The table is carried here rather than named by a variant because the families differ only
    /// in their rows: `DEV_ID` in bits 11:0 is stated identically by every reference manual behind
    /// [`stm32_dev_id`]. **Several families share one address**, so one route may carry several
    /// tables and a reading is offered to each in turn.
    Stm32DevId(&'static [&'static [(u32, &'static str)]]),
}

/// One route from a designer to a part name.
pub struct IdentityRoute {
    /// The register, as its own datasheet spells it.
    pub register: &'static str,
    /// Where it sits. Named from the driver crate's constant, never restated here.
    pub address: u32,
    /// Which decode applies to a reading off it.
    pub naming: Naming,
}

/// The routes an Atmel or Microchip designer code selects.
///
/// **THREE MECHANISMS THAT DO NOT OVERLAP.** A SAM D/E/L/C answers a DSU; a SAM4 has no DSU at all
/// and answers `CHIPID`; a SAM3X answers `CHIPID` 0x200 further up. The order is the order they are
/// tried in, and it is not arbitrary -- see [`identify`] for what stops a wrong address being
/// decoded as though it were right.
const ATMEL: &[IdentityRoute] = &[
    IdentityRoute { register: "DSU DID", address: SAM_DSU_DID, naming: Naming::SamDsu },
    IdentityRoute {
        register: "SAM4 CHIPID",
        address: SAM4_CHIPID_CIDR,
        naming: Naming::Sam4Chipid,
    },
    IdentityRoute {
        register: "SAM3X CHIPID",
        address: SAM3X_CHIPID_CIDR,
        naming: Naming::Sam3xChipid,
    },
];

/// The L0 and the C0 keep `DBGMCU_IDCODE` at one address, declared twice from two reference
/// manuals -- so the route below reads it once and offers the reading to both tables.
///
/// **STATED TO THE COMPILER RATHER THAN IN A COMMENT.** The route can only name one of the two
/// constants; if the other moved, the reading would keep going to the address this one names and
/// the second family would quietly stop being reachable, with nothing red.
const _: () = assert!(
    STM32L0_DBGMCU_IDCODE == STM32C0_DBGMCU_IDCODE,
    "the L0 and C0 DBGMCU addresses are no longer the same, so they need a route each"
);

/// The routes an STMicroelectronics designer code selects.
///
/// **THE ADDRESS IS PART OF THE FACT AND VARIES INSIDE THE VENDOR**: an L0 or a C0 keeps
/// `DBGMCU_IDCODE` at a peripheral address, an L4 and a U5 keep it in the debug region at two
/// different addresses, and an H7 keeps `DBGMCU_IDC` somewhere else again. A designer code does not
/// choose between them, so every one is a candidate and the reading decides.
const ST: &[IdentityRoute] = &[
    IdentityRoute {
        register: "DBGMCU_IDCODE",
        address: STM32L0_DBGMCU_IDCODE,
        naming: Naming::Stm32DevId(&[STM32L0_PARTS, STM32C0_PARTS]),
    },
    IdentityRoute {
        register: "DBGMCU_IDCODE",
        address: STM32L4_DBGMCU_IDCODE,
        naming: Naming::Stm32DevId(&[STM32L4_PARTS]),
    },
    IdentityRoute {
        register: "DBGMCU_IDCODE",
        address: STM32U5_DBGMCU_IDCODE,
        naming: Naming::Stm32DevId(&[STM32U5_PARTS]),
    },
    IdentityRoute {
        register: "DBGMCU_IDC",
        address: STM32H7_DBGMCU_IDC,
        naming: Naming::Stm32DevId(&[STM32H7_PARTS]),
    },
];

/// Which vendor identity registers a measured designer selects.
///
/// An empty slice is the ordinary answer for a designer with no routes, and it is the state every
/// vendor starts in: the walk read a code, and nothing here knows what to ask that vendor next.
/// **It is not the same as a failed read**, and a caller that reports the two alike would turn a
/// gap in this table into a claim about somebody's board.
#[must_use]
pub fn identity_routes(designer: Designer) -> &'static [IdentityRoute] {
    match designer.jep106() {
        Some((0x0, 0x1f)) => ATMEL,
        Some((0x0, 0x20)) => ST,
        _ => &[],
    }
}

/// What one route answered.
pub struct Reading {
    /// The register this route reads.
    pub register: &'static str,
    /// Where it was read.
    pub address: u32,
    /// The words read, in the order the route reads them. Empty when the read did not happen.
    pub words: Vec<u32>,
    /// What the reading names, where a table has a row for it.
    pub names: Option<String>,
    /// Why the register could not be read, where it could not.
    ///
    /// **A register that faults is the ORDINARY case on a part that does not have it**: a SAM D21
    /// has no `CHIPID` at the SAM4 address, and asking is how a route without a family finds out.
    /// So this is not an error to report as a failure -- it is the answer "not this one".
    pub unreadable: Option<String>,
}

/// What the chain settled, end to end.
pub struct Identification {
    /// Whose silicon the ROM table says this is.
    pub vendor: VendorDesigner,
    /// The vendor's name, where [`designer_name`] has a row for the code.
    pub vendor_name: Option<&'static str>,
    /// Every route tried, in the order they were tried.
    pub readings: Vec<Reading>,
}

impl Identification {
    /// The reading that named a part, where one did.
    #[must_use]
    pub fn named(&self) -> Option<&Reading> {
        self.readings.iter().find(|reading| reading.names.is_some())
    }

    /// Where the chain stopped, in one sentence a person can act on.
    ///
    /// **It says what was settled and what was not**, because every outcome short of a part name is
    /// still a true statement about the target, and reporting them all as "unknown" throws away the
    /// half that was answered.
    #[must_use]
    pub fn summary(&self) -> String {
        let vendor = match self.vendor_name {
            Some(name) => name.to_string(),
            None => match self.vendor {
                VendorDesigner::One(designer) => format!("{designer}"),
                _ => String::from("no vendor"),
            },
        };
        if let Some(reading) = self.named() {
            let names = reading.names.as_deref().unwrap_or_default();
            return format!(
                "{vendor}, {names} -- from {} at {:#010x}",
                reading.register, reading.address
            );
        }
        match self.vendor {
            VendorDesigner::One(_) if self.readings.is_empty() => format!(
                "{vendor}, and no identity register is known for that designer -- the chain \
                 stops at the vendor"
            ),
            VendorDesigner::One(_) => format!(
                "{vendor}, and none of its {} identity register(s) answered a value with a \
                 sourced row -- the chain stops at the vendor",
                self.readings.len()
            ),
            VendorDesigner::ArmThroughout => String::from(
                "every component names Arm, so this part's vendor designed no CoreSight component \
                 of its own -- a true answer, and the ROM table has nothing further to say",
            ),
            VendorDesigner::Several => String::from(
                "more than one designer other than Arm, which this rule does not choose between",
            ),
            VendorDesigner::Nothing => {
                String::from("no component identified a designer, so there is nothing to ask next")
            }
        }
    }
}

/// Runs the chain: takes a walk's designer to a vendor register, and a reading off it to a part.
///
/// # WHY TRYING SEVERAL ADDRESSES IS SAFE HERE AND WOULD NOT BE IN A FLASH ROUTINE
///
/// A wrong address does not fault on every part -- it decodes whatever happens to sit there. Three
/// things keep that from becoming a wrong answer, and all three are refusals somebody else already
/// wrote:
///
/// * every decode this dispatches to answers `None` for a value it has no sourced row for, rather
///   than a nearest match;
/// * `stm32_dev_id` refuses an all-zero and an all-ones reading, which is what an unmapped read and
///   an undriven bus produce;
/// * `sam_device_id` refuses the same two, in both DSU views.
///
/// So a route that is not this part's either faults, reads blank, or reads a value no table claims.
/// **And nothing here writes**, which is the difference that matters: the cost of asking a part a
/// question it does not answer is a fault, and the cost of erasing on a wrong answer is a board.
///
/// Every route is tried even after one names a part, because a second answer from a different
/// mechanism is worth reporting rather than hiding -- see
/// `two_mechanisms_naming_two_parts_are_both_reported`.
pub fn identify<A: TargetAccess>(target: &mut A, walk: &Walk) -> Identification {
    let vendor = walk.vendor_designer();
    let designer = match vendor {
        VendorDesigner::One(designer) => Some(designer),
        _ => None,
    };
    let vendor_name = designer
        .and_then(Designer::jep106)
        .and_then(|(continuation, identity)| designer_name(continuation, identity));
    let routes = designer.map_or(&[][..], identity_routes);

    let mut readings = Vec::new();
    for route in routes {
        readings.push(ask(target, route));
    }
    Identification { vendor, vendor_name, readings }
}

/// Reads one route and names what came back.
fn ask<A: TargetAccess>(target: &mut A, route: &IdentityRoute) -> Reading {
    let mut reading = Reading {
        register: route.register,
        address: route.address,
        words: Vec::new(),
        names: None,
        unreadable: None,
    };
    match route.naming {
        Naming::SamDsu => match target.sam_device_id() {
            Ok(id) => {
                reading.words.push(id.raw);
                reading.names = id.part().map(str::to_string);
            }
            Err(error) => reading.unreadable = Some(error.to_string()),
        },
        Naming::Sam4Chipid => match (target.read_word(route.address), target.read_word(SAM4_CHIPID_EXID))
        {
            (Ok(cidr), Ok(exid)) => {
                reading.words.extend([cidr, exid]);
                reading.names = sam4_identify(cidr, exid)
                    .map(|part| format!("{} ({})", part.part, part.family));
            }
            (Err(error), _) | (_, Err(error)) => reading.unreadable = Some(error.to_string()),
        },
        Naming::Sam3xChipid => match target.read_word(route.address) {
            Ok(cidr) => {
                reading.words.push(cidr);
                reading.names = sam3x_identify(cidr).map(str::to_string);
            }
            Err(error) => reading.unreadable = Some(error.to_string()),
        },
        Naming::Stm32DevId(tables) => match stm32_dev_id(target, route.address) {
            Ok((dev_id, _)) => {
                reading.words.push(dev_id);
                reading.names = tables
                    .iter()
                    .flat_map(|table| table.iter())
                    .find(|(id, _)| *id == dev_id)
                    .map(|(_, what)| (*what).to_string());
            }
            Err(error) => reading.unreadable = Some(error.to_string()),
        },
    }
    reading
}

#[cfg(test)]
mod tests;
