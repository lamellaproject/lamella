//! The dispatch records a managed object carries, written and read.
//!
//! A type descriptor holds one slot per virtual method its type has and one entry per interface
//! method it implements, each a reference to the method that answers it. A link that keeps a
//! descriptor and follows every one of those references keeps every override of every virtual
//! method of every kept type -- a type's formatting, hashing and comparison methods, and everything
//! they call -- whether the program ever calls them or not. So the backend records, for each
//! function it emits, which table entries that function dispatches through, and a link keeps an
//! entry's method only when a function it keeps dispatches through that entry.
//!
//! Each record is a symbol whose NAME is the record: local, of size zero, and the target of no
//! relocation. It adds no byte to an image and no edge to the reference graph, so a record never
//! keeps anything by itself. An object marks that it records every dispatch its functions make with
//! [`DISPATCH_RECORDS`]; an object without the mark -- one from another producer -- keeps every
//! entry of its tables, as a link always did.
//!
//! A virtual call names a slot and the type that declares the called method. Only a receiver of
//! that type, or of a type deriving from it, can reach the call, so the slot is live only in the
//! descriptors of those types; a call through `object`'s `ToString` slot keeps every kept type's
//! `ToString`, and a call through a slot some unrelated class happens to number the same keeps
//! nothing of that class. Descriptors record their immediate base for this, as
//! [`hierarchy_record_name`] spells it.

use alloc::format;
use alloc::string::String;

/// The symbol that marks an object whose emitter records every table dispatch its functions make,
/// in the format this module reads. An object without it keeps every table entry it lays.
pub const DISPATCH_RECORDS: &str = "__lamella_dispatch_records_v1";

/// The prefix of a virtual-slot record whose declaring type is named.
const VIRTUAL_PREFIX: &str = "__lamella_dispatch_v_";
/// The prefix of a virtual-slot record whose call site could not name the declaring type.
const ANY_VIRTUAL_PREFIX: &str = "__lamella_dispatch_any_";
/// The prefix of an interface-method record.
const INTERFACE_PREFIX: &str = "__lamella_dispatch_i_";
/// The prefix of a descriptor's immediate-base record.
const HIERARCHY_PREFIX: &str = "__lamella_hierarchy_";

/// The function a link points every table entry at whose method no kept function dispatches to.
///
/// An entry cannot simply be dropped: a table with an entry missing is not a smaller table but a
/// shifted one, and every call computed against its numbering would land on a different method.
/// So the entry stays and its method goes, and this one shared function stands in for all of
/// them. It faults: no call reaches an entry whose method nothing dispatches to, so reaching it is
/// a defect in the records, and a fault says so where a function that returned would answer.
pub const UNUSED_SLOT_TRAP: &str = "__lamella_unused_slot";

/// One table entry a function dispatches through.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DispatchSlot {
    /// A vtable slot, with the descriptor symbol of the type that declares the called method.
    Virtual {
        /// The slot's index in the vtable.
        slot: u32,
        /// The declaring type's descriptor symbol.
        declaring: String,
    },
    /// A vtable slot whose call site could not name the declaring type: live in every descriptor
    /// that has a slot at this index.
    AnyVirtual(u32),
    /// An interface method, by the tag the itable entries are keyed on.
    Interface(u32),
}

/// The record name for `caller` dispatching through `slot`.
///
/// The declaring descriptor's symbol is written with its length in front of it, because both it and
/// the caller's symbol may contain any character a symbol name can, underscores included.
#[must_use]
pub fn dispatch_record_name(slot: &DispatchSlot, caller: &str) -> String {
    match slot {
        DispatchSlot::Virtual { slot, declaring } => {
            format!(
                "{VIRTUAL_PREFIX}{slot}_{}_{declaring}{caller}",
                declaring.len()
            )
        }
        DispatchSlot::AnyVirtual(slot) => format!("{ANY_VIRTUAL_PREFIX}{slot}_{caller}"),
        DispatchSlot::Interface(tag) => format!("{INTERFACE_PREFIX}{tag}_{caller}"),
    }
}

/// The table entry and the calling function a record name carries, or `None` for a name that is
/// not a well-formed record.
#[must_use]
pub fn dispatch_record(name: &str) -> Option<(DispatchSlot, &str)> {
    if let Some(rest) = name.strip_prefix(VIRTUAL_PREFIX) {
        let (slot, rest) = rest.split_once('_')?;
        let (declaring, caller) = length_prefixed(rest)?;
        if declaring.is_empty() || caller.is_empty() {
            return None;
        }
        let slot = DispatchSlot::Virtual {
            slot: slot.parse().ok()?,
            declaring: String::from(declaring),
        };
        return Some((slot, caller));
    }
    let (slot, rest) = if let Some(rest) = name.strip_prefix(ANY_VIRTUAL_PREFIX) {
        let (slot, rest) = rest.split_once('_')?;
        (DispatchSlot::AnyVirtual(slot.parse().ok()?), rest)
    } else {
        let (tag, rest) = name.strip_prefix(INTERFACE_PREFIX)?.split_once('_')?;
        (DispatchSlot::Interface(tag.parse().ok()?), rest)
    };
    (!rest.is_empty()).then_some((slot, rest))
}

/// The record name for a descriptor and its immediate base's descriptor, `None` where the type has
/// no base a descriptor names -- the end of its chain.
#[must_use]
pub fn hierarchy_record_name(descriptor: &str, base: Option<&str>) -> String {
    format!(
        "{HIERARCHY_PREFIX}{}_{descriptor}{}",
        descriptor.len(),
        base.unwrap_or("")
    )
}

/// The descriptor and immediate base a hierarchy record name carries -- the base `None` at the end
/// of a chain -- or `None` for a name that is not a well-formed record.
#[must_use]
pub fn hierarchy_record(name: &str) -> Option<(&str, Option<&str>)> {
    let (descriptor, base) = length_prefixed(name.strip_prefix(HIERARCHY_PREFIX)?)?;
    if descriptor.is_empty() {
        return None;
    }
    Some((descriptor, (!base.is_empty()).then_some(base)))
}

/// Whether a symbol name is a record of either kind, or the mark -- the names a trim keeps without
/// a byte span to copy.
#[must_use]
pub fn is_record(name: &str) -> bool {
    name == DISPATCH_RECORDS || dispatch_record(name).is_some() || hierarchy_record(name).is_some()
}

/// Splits `<length>_<that many bytes><rest>` into the counted part and the rest.
fn length_prefixed(encoded: &str) -> Option<(&str, &str)> {
    let (length, rest) = encoded.split_once('_')?;
    let length = length.parse().ok()?;
    Some((rest.get(..length)?, rest.get(length..)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESCRIPTOR: &str = "__lamella_typedesc_abcdef01_33554433";
    const CALLER: &str = "L12345678.Sensors.Reader::Read_with_underscores";

    #[test]
    fn a_virtual_record_reads_back_its_slot_its_declaring_type_and_its_caller() {
        let slot = DispatchSlot::Virtual {
            slot: 4,
            declaring: String::from(DESCRIPTOR),
        };
        let name = dispatch_record_name(&slot, CALLER);
        assert_eq!(dispatch_record(&name), Some((slot, CALLER)));
    }

    #[test]
    fn a_record_with_no_declaring_type_and_an_interface_record_read_back() {
        for slot in [
            DispatchSlot::AnyVirtual(7),
            DispatchSlot::Interface(0xDEAD_BEEF),
        ] {
            let name = dispatch_record_name(&slot, CALLER);
            assert_eq!(dispatch_record(&name), Some((slot, CALLER)));
        }
    }

    #[test]
    fn a_hierarchy_record_reads_back_its_base_or_the_end_of_its_chain() {
        let base = "__lamella_typedesc_cafef00d_33554434";
        let name = hierarchy_record_name(DESCRIPTOR, Some(base));
        assert_eq!(hierarchy_record(&name), Some((DESCRIPTOR, Some(base))));
        let end = hierarchy_record_name(DESCRIPTOR, None);
        assert_eq!(hierarchy_record(&end), Some((DESCRIPTOR, None)));
    }

    #[test]
    fn a_name_that_is_not_a_whole_record_is_no_record() {
        for name in [
            "__lamella_dispatch_v_4_999_short",
            "__lamella_dispatch_v_x_1_ab",
            "__lamella_dispatch_v_4_3_abc",
            "__lamella_dispatch_any_4_",
            "__lamella_dispatch_i_notanumber_f",
            "__lamella_hierarchy_0_",
            "__lamella_typedesc_abcdef01_33554433",
            "main",
        ] {
            assert_eq!(dispatch_record(name), None, "{name}");
            assert_eq!(hierarchy_record(name), None, "{name}");
            assert!(!is_record(name), "{name}");
        }
        assert!(is_record(DISPATCH_RECORDS));
    }
}
