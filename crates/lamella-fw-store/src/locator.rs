//! The boot locator record: which firmware slot runs next, and for how long.

use crate::{FirmwareFlash, FlashError};
use lamella_wire::msg::fw_slot;

/// Which of the two ping-ponged locator slots. NOT a firmware A/B slot -- see the module note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocatorSlot {
    /// The first slot in the region.
    First,
    /// The second.
    Second,
}

impl LocatorSlot {
    /// Where this slot starts within the locator region.
    ///
    /// The two slots sit back to back, so the second begins one whole slot in. Region-relative,
    /// like every offset on the seam -- the region base is the implementor's business.
    #[must_use]
    pub fn base(self, write_unit: usize) -> usize {
        match self {
            LocatorSlot::First => 0,
            LocatorSlot::Second => slot_size(write_unit),
        }
    }

    /// The other one.
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            LocatorSlot::First => LocatorSlot::Second,
            LocatorSlot::Second => LocatorSlot::First,
        }
    }
}

/// The choice a locator records: which firmware slot boots next, and for how long.
///
/// **ONLY WHAT `FW_ACTIVATE` DECIDES**, so a reader of this type knows the whole of what a boot
/// path acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootChoice {
    /// The firmware slot to run next. Never [`fw_slot::OTHER`] -- that is a request a host makes,
    /// and it is resolved against the running slot before anything is written, because a record
    /// meaning "the other one" would change meaning depending on who read it.
    pub next_slot: u8,
    /// [`lamella_wire::msg::fw_intent::PERMANENT`] or [`lamella_wire::msg::fw_intent::ONE_BOOT`].
    pub intent: u8,
}

/// The bytes of the record, excluding the magic that goes in the last unit alone.
///
/// Kept small and fixed rather than a struct with a derived layout, because this is a wire-like
/// format read by a boot path that may be a different build from the one that wrote it.
const PAYLOAD_LEN: usize = 4;

/// The magic that means "this record is complete", written ALONE in the final write unit.
///
/// **IT IS TRUNCATED TO THE WRITE UNIT, AND THAT IS NOT A COMPROMISE -- IT IS THE ONLY THING THAT
/// WORKS.** The magic has to be alone in ONE write unit, and an STM32F091 commits TWO BYTES per
/// program. A four-byte magic simply does not fit there, so on that part the marker is the first
/// two bytes and on every wider part it is all four.
///
/// **ITS VALUE MUST NOT BE THE ERASED PATTERN AT ANY LENGTH**, or an erased slot would read as a
/// valid record and a blank board would boot a slot nobody chose. A test asserts that for every
/// supported unit against both erased values.
const MAGIC: [u8; 4] = [0x4C, 0x4F, 0x43, 0x31];

/// How many bytes of [`MAGIC`] a part with this write unit actually stores.
fn magic_len(write_unit: usize) -> usize {
    if write_unit < MAGIC.len() { write_unit } else { MAGIC.len() }
}

/// The size of one locator slot on a part with this write unit, in bytes.
///
/// **A CALLER MUST SIZE ITS REGION FROM THIS AND NOT FROM `PAYLOAD_LEN`.** Two slots of this, not
/// two of the payload -- and on the widest granule that is 128 bytes for a four-byte choice.
#[must_use]
pub fn slot_size(write_unit: usize) -> usize {
    debug_assert!(write_unit > 0, "a write unit of zero describes no part");
    (PAYLOAD_LEN.div_ceil(write_unit) + 1) * write_unit
}

/// The offset of the magic's unit within a slot: the last one.
fn magic_offset(write_unit: usize) -> usize {
    slot_size(write_unit) - write_unit
}

/// Reads the choice a slot holds, or `None` if it holds none.
///
/// A slot reads as holding none when its magic unit is not the magic -- which is what an erased
/// slot looks like, and what a slot torn part-way through a write looks like. **Those two are
/// deliberately the same answer**: neither is a choice, and a boot path that distinguished them
/// would have to decide what to do about the difference.
fn read_slot<F: FirmwareFlash>(flash: &F, slot: LocatorSlot) -> Option<Record> {
    let unit = flash.write_unit();
    let base = slot.base(unit);
    let len = magic_len(unit);
    let mut magic = [0u8; MAGIC.len()];
    flash.read(base + magic_offset(unit), &mut magic[..len]).ok()?;
    if magic[..len] != MAGIC[..len] {
        return None;
    }
    let mut payload = [0u8; PAYLOAD_LEN];
    flash.read(base, &mut payload).ok()?;
    Some(Record {
        choice: BootChoice { next_slot: payload[0], intent: payload[1] },
        sequence: u16::from_le_bytes([payload[2], payload[3]]),
    })
}

/// A decoded locator record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Record {
    choice: BootChoice,
    sequence: u16,
}

/// The choice currently in force, and which slot holds it.
///
/// Takes the valid slot with the higher sequence. **The comparison is WRAPPING**, because a u16
/// sequence on a board that is updated often will wrap, and a plain `>` would then prefer the old
/// record forever -- a failure that appears after 65,536 updates and never in a test that does ten.
#[must_use]
pub fn current<F: FirmwareFlash>(flash: &F) -> Option<(BootChoice, LocatorSlot)> {
    let first = read_slot(flash, LocatorSlot::First);
    let second = read_slot(flash, LocatorSlot::Second);
    match (first, second) {
        (None, None) => None,
        (Some(a), None) => Some((a.choice, LocatorSlot::First)),
        (None, Some(b)) => Some((b.choice, LocatorSlot::Second)),
        (Some(a), Some(b)) => {
            if b.sequence.wrapping_sub(a.sequence) < u16::MAX / 2 {
                Some((b.choice, LocatorSlot::Second))
            } else {
                Some((a.choice, LocatorSlot::First))
            }
        }
    }
}

/// Records `choice`, leaving the previous record intact until the new one is complete.
///
/// `running_slot` resolves [`fw_slot::OTHER`]: a host says "the other one" so that it need not
/// track which side the board is on, and **that has to be resolved HERE rather than stored**,
/// because a record meaning "the other one" would mean something different every time it was read.
///
/// # The write order, which is the whole point
///
/// 1. erase the slot that is NOT currently newest -- so the record in force survives all of this;
/// 2. write every unit but the last, magic absent;
/// 3. write the last unit, magic alone.
///
/// A reset at any point before step 3 completes leaves the old record newest and the new one
/// reading as absent. There is no interval in which a half-written record is a valid choice.
pub fn activate<F: FirmwareFlash>(
    flash: &mut F,
    choice: BootChoice,
    running_slot: u8,
) -> Result<BootChoice, FlashError> {
    let unit = flash.write_unit();
    let (previous, previous_slot) = match current(flash) {
        Some((choice, slot)) => (Some(choice), Some(slot)),
        None => (None, None),
    };
    let _ = previous;
    let target = previous_slot.map_or(LocatorSlot::First, LocatorSlot::other);

    let next_slot = if choice.next_slot == fw_slot::OTHER {
        u8::from(running_slot == 0)
    } else {
        choice.next_slot
    };
    let resolved = BootChoice { next_slot, intent: choice.intent };

    let sequence = read_slot(flash, target.other()).map_or(0, |r| r.sequence.wrapping_add(1));

    let base = target.base(unit);
    flash.erase(base, slot_size(unit))?;

    let body_len = magic_offset(unit);
    let mut body = [0u8; MAX_BODY];
    if body_len > MAX_BODY {
        return Err(FlashError::RegionTooSmall);
    }
    let pad = flash.erased_byte();
    for byte in body.iter_mut().take(body_len) {
        *byte = pad;
    }
    body[0] = resolved.next_slot;
    body[1] = resolved.intent;
    body[2..4].copy_from_slice(&sequence.to_le_bytes());
    flash.write(base, &body[..body_len])?;

    let mut last = [0u8; MAX_UNIT];
    if unit > MAX_UNIT {
        return Err(FlashError::RegionTooSmall);
    }
    for byte in last.iter_mut().take(unit) {
        *byte = pad;
    }
    let len = magic_len(unit);
    last[..len].copy_from_slice(&MAGIC[..len]);
    flash.write(base + magic_offset(unit), &last[..unit])?;

    Ok(resolved)
}

/// The widest write unit this crate can stage without allocating.
///
/// **128 BYTES, WHICH IS A SAM D21'S PAGE AND NOT AN STM32H7'S FLASH WORD.** The widest granule
/// among the families modelled here is a Cortex-M0+'s 64 bytes, and the H7's is 32 -- so 128
/// leaves headroom above every one of them, and a part that exceeds it is refused by name rather
/// than silently truncated.
const MAX_UNIT: usize = 128;
/// The widest body (every unit but the last) this crate can stage. One unit is enough for a
/// four-byte payload on every part; two units of headroom costs nothing on a device.
const MAX_BODY: usize = MAX_UNIT * 2;


#[cfg(test)]
#[path = "locator_tests.rs"]
mod tests;
