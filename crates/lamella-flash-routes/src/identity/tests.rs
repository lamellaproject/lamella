//! The identity chain driven over a modelled part, so what it asks and what it concludes are
//! checked without a board on a wire.

use super::*;
use lamella_probe_core::ProbeError;
use lamella_probe_core::coresight::{
    CIDR0, DEVARCH, DEVID, DEVTYPE_OR_MEMTYPE, PIDR0, PIDR4, walk,
};
use std::collections::HashMap;

/// A target whose memory is a map: a word that was not put there faults, exactly as an unmapped
/// address does on a part.
///
/// **THE FAULT IS THE POINT.** Every route this module offers is tried against every part, so most
/// of them read an address the part does not implement -- and a fake that answered zero everywhere
/// would turn each of those into a decode of `0x00000000` rather than into the refusal a real bus
/// produces. Modelling absence is what makes "a wrong address does not become a wrong answer"
/// testable at all.
struct FakePart {
    words: HashMap<u32, u32>,
    /// Every address the chain asked for, in order, so a test can assert on what was NOT read.
    asked: Vec<u32>,
}

impl FakePart {
    fn new() -> Self {
        FakePart { words: HashMap::new(), asked: Vec::new() }
    }

    fn at(mut self, address: u32, word: u32) -> Self {
        self.words.insert(address, word);
        self
    }
}

/// The narrower seam [`walk`] takes, so one fake serves the table walk and the chain that reads
/// its conclusion -- which is the arrangement on a real probe too.
impl lamella_probe_core::CoreMemory for FakePart {
    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        TargetAccess::read_word(self, address)
    }
    fn write_word(&mut self, _address: u32, _value: u32) -> Result<(), ProbeError> {
        unreachable!("walking a ROM table writes nothing")
    }
    fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
        unreachable!("walking a ROM table does not drive the reset line")
    }
}

impl TargetAccess for FakePart {
    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        self.asked.push(address);
        self.words.get(&address).copied().ok_or(ProbeError::Ack(lamella_probe_core::Ack::Fault))
    }

    fn connect(&mut self) -> Result<(), ProbeError> {
        unreachable!("the chain is handed a connected target; connecting is the caller's")
    }
    fn read_idcode(&mut self) -> Result<u32, ProbeError> {
        unreachable!("the chain reads the ROM table, not the debug port")
    }
    fn init_mem(&mut self) -> Result<(), ProbeError> {
        unreachable!("the chain is handed a powered MEM-AP; powering it is the caller's")
    }
    fn write_word(&mut self, _address: u32, _value: u32) -> Result<(), ProbeError> {
        unreachable!("identifying a part writes nothing")
    }
    fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
        for (index, word) in out.iter_mut().enumerate() {
            *word = self.read_word(address + (index * 4) as u32)?;
        }
        Ok(())
    }
    fn write_words(&mut self, _address: u32, _words: &[u32]) -> Result<(), ProbeError> {
        unreachable!("identifying a part writes nothing")
    }
    fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
        unreachable!("every identity register here is a word")
    }
    fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
        unreachable!("identifying a part writes nothing")
    }
    fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
        unreachable!("every identity register here is a word")
    }
    fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
        unreachable!("identifying a part writes nothing")
    }
    fn halt(&mut self) -> Result<(), ProbeError> {
        unreachable!("identifying a part does not halt it")
    }
    fn resume(&mut self) -> Result<(), ProbeError> {
        unreachable!("identifying a part does not halt it")
    }
    fn step(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the identify path")
    }
    fn is_halted(&mut self) -> Result<bool, ProbeError> {
        unreachable!("run control is not the identify path")
    }
    fn wait_halted(&mut self) -> Result<(), ProbeError> {
        unreachable!("run control is not the identify path")
    }
    fn reset_and_run(&mut self) -> Result<(), ProbeError> {
        unreachable!("identifying a part does not reset it")
    }
    fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
        unreachable!("identifying a part does not reset it")
    }
    fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
        unreachable!("identifying a part does not drive the reset line")
    }
    fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
        unreachable!("core registers are not the identify path")
    }
    fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
        unreachable!("identifying a part writes nothing")
    }
    fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the identify path")
    }
    fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
        unreachable!("reset catch is not the identify path")
    }
    fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the identify path")
    }
    fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the identify path")
    }
    fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
        unreachable!("breakpoints are not the identify path")
    }
    fn call_target(
        &mut self,
        _address: u32,
        _args: &[u32],
        _frame: &lamella_probe_core::CallFrame,
    ) -> Result<u32, ProbeError> {
        unreachable!("identifying a part does not run code on it")
    }
}

/// Lays down the Component and Peripheral ID registers of one component, so a walk over the fake
/// produces a designer.
///
/// The same nine registers `coresight`'s own fixture writes, in the compressed designer form.
fn component(part: FakePart, base: u32, class: u8, continuation: u8, identity: u8) -> FakePart {
    let des_0 = u32::from(identity & 0xf);
    let des_1 = u32::from(identity >> 4) & 0b111;
    part.at(base + CIDR0, 0x0d)
        .at(base + CIDR0 + 4, u32::from(class) << 4)
        .at(base + CIDR0 + 8, 0x05)
        .at(base + CIDR0 + 12, 0xb1)
        .at(base + PIDR0, 0)
        .at(base + PIDR0 + 4, des_0 << 4)
        .at(base + PIDR0 + 8, 0b1000 | des_1)
        .at(base + PIDR0 + 12, 0)
        .at(base + PIDR4, u32::from(continuation))
        .at(base + DEVARCH, 0)
        .at(base + DEVID, 0)
        .at(base + DEVTYPE_OR_MEMTYPE, 0)
}

/// A one-component ROM table carrying `(continuation, identity)` as its designer.
const TABLE: u32 = 0x4000_0000;

fn walked(part: FakePart, continuation: u8, identity: u8) -> (FakePart, Walk) {
    let mut part = component(part, TABLE, 0x1, continuation, identity)
        .at(TABLE, 0x0000_0000);
    let walk = walk(&mut part, TABLE);
    (part, walk)
}

/// The Atmel chain, end to end: a designer read off a table selects the DSU, and the DSU names the
/// part.
///
/// The `DID` is the value a SAM D21 answers, and it is the reading whose part name is sourced by
/// the board naming itself. **This is the whole chain in one test** -- table to designer, designer
/// to register, register to part -- and it is the shape the onboarding bar asks for.
#[test]
fn an_atmel_designer_reaches_the_dsu_and_the_dsu_names_the_part() {
    let (mut part, walk) = walked(FakePart::new().at(SAM_DSU_DID, 0x1001_2693), 0x0, 0x1f);
    let found = identify(&mut part, &walk);

    assert_eq!(found.vendor_name, Some("Atmel (Microchip)"));
    let named = found.named().expect("the DSU named the part");
    assert_eq!(named.register, "DSU DID");
    assert_eq!(named.address, SAM_DSU_DID);
    assert_eq!(named.names.as_deref(), Some("ATSAMD21G17D"));
}

/// The routes this part does not implement fault, and a fault names nothing.
///
/// **A ROUTE THAT IS NOT THIS PART'S MUST NOT PRODUCE A NAME.** The chain offers every one of a
/// designer's registers to every part carrying that designer, so on a SAM D21 the two `CHIPID`
/// routes read addresses the part does not map. Each must come back unreadable, and the DSU's
/// answer must be the only name.
#[test]
fn a_route_the_part_does_not_implement_names_nothing() {
    let (mut part, walk) = walked(FakePart::new().at(SAM_DSU_DID, 0x1001_2693), 0x0, 0x1f);
    let found = identify(&mut part, &walk);

    assert_eq!(found.readings.len(), 3, "all three Atmel routes were tried");
    let unnamed: Vec<&str> = found
        .readings
        .iter()
        .filter(|reading| reading.names.is_none())
        .map(|reading| reading.register)
        .collect();
    assert_eq!(unnamed, vec!["SAM4 CHIPID", "SAM3X CHIPID"]);
    assert!(
        found.readings.iter().filter(|r| r.names.is_none()).all(|r| r.unreadable.is_some()),
        "and each says why it did not answer, rather than reading as a blank part"
    );
}

/// A `CHIPID` value no table sources is read successfully and named by nothing.
///
/// **THIS IS THE CASE THAT SEPARATES A GAP FROM A FAILURE.** The register answered -- the reading
/// is recorded and there is no error -- and the tables have no row for it. A chain that reported
/// this as unreadable would send somebody after a probe fault that did not happen.
#[test]
fn a_reading_with_no_sourced_row_is_recorded_rather_than_reported_as_a_fault() {
    let (mut part, walk) = walked(
        FakePart::new().at(SAM4_CHIPID_CIDR, 0xdead_0ce0).at(SAM4_CHIPID_EXID, 0x0),
        0x0,
        0x1f,
    );
    let found = identify(&mut part, &walk);

    assert!(found.named().is_none(), "no table has a row for that CIDR");
    let chipid = found
        .readings
        .iter()
        .find(|reading| reading.register == "SAM4 CHIPID")
        .expect("the SAM4 route was tried");
    assert_eq!(chipid.words, vec![0xdead_0ce0, 0x0], "the reading is kept");
    assert!(chipid.unreadable.is_none(), "the register answered, so nothing was unreadable");
}

/// An ST designer reaches the register the L0 and the C0 share, and the reading picks the family.
#[test]
fn the_shared_st_address_is_read_once_and_offered_to_both_families() {
    let (mut part, walk) = walked(FakePart::new().at(STM32L0_DBGMCU_IDCODE, 0x1000_0453), 0x0, 0x20);
    let found = identify(&mut part, &walk);

    assert_eq!(found.vendor_name, Some("STMicroelectronics"));
    let named = found.named().expect("one of the two tables claimed the value");
    assert!(named.names.as_deref().unwrap_or_default().contains("STM32C031"), "{named:?}", named = named.names);
    assert_eq!(
        found.readings.iter().filter(|r| r.address == STM32L0_DBGMCU_IDCODE).count(),
        1,
        "one address, one read -- the two families share the route rather than each having one"
    );
}

/// The two families behind one address must never both claim a `DEV_ID`.
///
/// **A CENSUS, NOT AN EXAMPLE.** The route reads one word and offers it to both tables in order, so
/// a value claimed by both would be named by whichever is listed first -- a wrong part off a
/// correct reading, with nothing anywhere to notice. This fails the moment a row is added that
/// collides, which is when it is cheap to fix.
#[test]
fn the_families_sharing_a_register_claim_no_id_in_common() {
    for route in identity_routes(Designer::Compressed { continuation: 0x0, identity: 0x20 }) {
        let Naming::Stm32DevId(tables) = route.naming else { continue };
        let mut claimed: Vec<(u32, usize)> = Vec::new();
        for (index, table) in tables.iter().enumerate() {
            for (id, _) in table.iter() {
                if let Some((_, first)) = claimed.iter().find(|(seen, _)| seen == id) {
                    panic!(
                        "{} at {:#010x}: DEV_ID {id:#05x} is claimed by table {first} and table \
                         {index}, so one reading names two parts",
                        route.register, route.address
                    );
                }
                claimed.push((*id, index));
            }
        }
    }
}

/// A designer with no routes stops at the vendor, and says so rather than reading anything.
#[test]
fn a_designer_with_no_routes_reads_nothing_at_all() {
    let (mut part, walk) = walked(FakePart::new(), 0x4, 0x23);
    let before = part.asked.len();
    let found = identify(&mut part, &walk);

    assert!(found.vendor_name.is_none(), "the designer code has no name row");
    assert!(found.readings.is_empty(), "and no register was guessed at");
    assert_eq!(part.asked.len(), before, "nothing was read off the target");
    assert!(found.summary().contains("no identity register is known"), "{}", found.summary());
}

/// A part whose components are all Arm's is reported as answered, not as failed.
#[test]
fn an_arm_throughout_table_asks_no_vendor_register() {
    let (mut part, walk) = walked(FakePart::new(), 0x4, 0x3b);
    let found = identify(&mut part, &walk);

    assert_eq!(found.vendor, VendorDesigner::ArmThroughout);
    assert!(found.readings.is_empty());
    assert!(found.summary().contains("designed no CoreSight component"), "{}", found.summary());
}

/// Every mechanism a SAM flash route can name is a route this chain will try.
///
/// **TWO STATEMENTS OF "WHICH REGISTER NAMES A SAM PART" NOW EXIST AND THEY ANSWER DIFFERENT
/// QUESTIONS**: [`SamFamily::identity_register`] is asked by a caller who already chose a family,
/// and [`identity_routes`] by one holding nothing but a designer. They may differ in SHAPE and must
/// not differ in the SET -- a mechanism reachable from a `--board` and not from a probe is a part
/// this chain narrows to a vendor and then fails to name, for no reason a reader could see.
///
/// **THE MATCH BELOW IS EXHAUSTIVE, so a NEW MECHANISM fails to compile here** rather than landing
/// in one list and not the other. A new [`SamFamily`] that reuses an existing mechanism is not
/// caught and does not need to be: its register is already routed.
#[test]
fn every_sam_identity_mechanism_is_a_route_this_chain_tries() {
    for family in [
        crate::SamFamily::Samd21,
        crate::SamFamily::Same54,
        crate::SamFamily::Sam4Eefc,
        crate::SamFamily::Sam4l,
        crate::SamFamily::Sam3x,
        crate::SamFamily::Sam4sDual,
    ] {
        let address = match family.identity_register() {
            crate::SamIdentity::Dsu => SAM_DSU_DID,
            crate::SamIdentity::Sam4Chipid(_) => SAM4_CHIPID_CIDR,
            crate::SamIdentity::Sam3xChipid => SAM3X_CHIPID_CIDR,
        };
        assert!(
            ATMEL.iter().any(|route| route.address == address),
            "{family:?} names its part at {address:#010x}, which no Atmel route in this chain reads"
        );
    }
}

/// Two mechanisms naming two different parts are both reported.
///
/// **A SECOND ANSWER IS A FINDING, NOT NOISE.** A part that answers both a DSU and a SAM4 `CHIPID`
/// with values two different tables claim is either a part these routes do not describe or a
/// register read at an address that means something else there -- and either way the reader has to
/// see both. Stopping at the first name would hide it.
#[test]
fn two_mechanisms_naming_two_parts_are_both_reported() {
    let (mut part, walk) = walked(
        FakePart::new()
            .at(SAM_DSU_DID, 0x1001_2693)
            .at(SAM4_CHIPID_CIDR, 0xa3cc_0ce0)
            .at(SAM4_CHIPID_EXID, 0x0012_0200),
        0x0,
        0x1f,
    );
    let found = identify(&mut part, &walk);

    let named: Vec<&str> =
        found.readings.iter().filter_map(|r| r.names.as_deref()).collect();
    assert_eq!(named.len(), 2, "both mechanisms named something: {named:?}");
    assert!(named.iter().any(|name| name.contains("ATSAMD21G17D")));
    assert!(named.iter().any(|name| name.contains("ATSAM4E16E")));
}
