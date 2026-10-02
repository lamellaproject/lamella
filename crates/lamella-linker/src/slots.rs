//! Called-slot liveness: a kept descriptor's table entry keeps its method only while a kept function
//! dispatches through that entry.
//!
//! The backend writes the facts this reads as records in each object's symbol table
//! ([`lamella_elf::dispatch`]): which table entries each function dispatches through, and each
//! descriptor's immediate base. Without them a kept descriptor keeps every method its tables name,
//! so a program that keeps a type keeps that type's formatting, hashing and comparison methods, and
//! everything they call, whether it calls them or not.
//!
//! **IT IS ON ONLY WHEN EVERY MANAGED OBJECT IN THE LINK RECORDS ITS DISPATCHES.** A function with no
//! records is invisible to this analysis, and an entry it dispatches through would be pointed at the
//! trap. A function that dispatches makes a call, every call is a safepoint, and every safepoint-
//! bearing managed function has a stack-map record -- so an object that defines a descriptor or a
//! stack-map record and carries no mark switches the whole analysis off, and every entry is kept, as
//! a link always kept them.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use lamella_elf::dispatch::{
    DISPATCH_RECORDS, DispatchSlot, UNUSED_SLOT_TRAP, dispatch_record, hierarchy_record,
};
use lamella_elf::{Machine, Object, ParsedRelocation, STACKMAP_RECORD_PREFIX, TYPE_DESC_PREFIX};

/// One entry of a descriptor's tables, as its relocation lays it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TableEntry {
    /// A vtable slot, by index.
    Slot(u32),
    /// An interface-method entry, by the tag it is keyed on.
    Tag(u32),
}

/// The table entry a relocation inside a descriptor lays, or `None` for any other word of it -- the
/// base pointer, the name, the header -- or for a relocation in an encoding this does not read.
///
/// A vtable slot holds `method - descriptor`, laid at `descriptor - 4 - 4 * slot`, so its addend is
/// `-(4 + 4 * slot)`. An interface entry is laid after the descriptor's words with a positive addend,
/// and the word in front of it is the interface method's tag.
pub(crate) fn table_entry(obj: &Object, r: &ParsedRelocation) -> Option<TableEntry> {
    let descriptor_relative = match obj.machine {
        Machine::Arm => lamella_elf::arm::R_LAMELLA_REL_DESC,
        Machine::RiscV => lamella_elf::riscv::R_LAMELLA_REL_DESC,
    };
    if r.kind != descriptor_relative || r.implicit_addend {
        return None;
    }
    if r.addend < 0 {
        let distance = u32::try_from(r.addend.checked_neg()?).ok()?;
        return (distance >= 4 && distance % 4 == 0).then_some(TableEntry::Slot(distance / 4 - 1));
    }
    let tag_at = r.offset.checked_sub(4)? as usize;
    let tag = obj.text.get(tag_at..tag_at + 4)?;
    Some(TableEntry::Tag(u32::from_le_bytes(tag.try_into().ok()?)))
}

/// Whether `obj` marks that it records every dispatch its functions make.
pub(crate) fn records_dispatches(obj: &Object) -> bool {
    obj.symbols.iter().any(|s| s.name == DISPATCH_RECORDS)
}

/// The analysis: every object's records, and the table entries found live so far.
pub(crate) struct Slots<'o> {
    /// Whether the analysis is on; see the module documentation.
    active: bool,
    /// Each calling function's records, by its symbol name.
    calls: BTreeMap<&'o str, Vec<DispatchSlot>>,
    /// Each descriptor's ancestors, itself included, or `None` when some descriptor on its chain
    /// records no base -- a chain this cannot see the end of.
    ancestry: BTreeMap<&'o str, Option<BTreeSet<&'o str>>>,
    /// Slot indices a call that named no declaring type dispatches through.
    any_slots: BTreeSet<u32>,
    /// Slot index -> the declaring types calls through it named.
    declared_slots: BTreeMap<u32, BTreeSet<String>>,
    /// Interface tags called.
    tags: BTreeSet<u32>,
}

impl<'o> Slots<'o> {
    /// Reads every object's records. Nothing is live until callers are activated.
    pub(crate) fn new(objects: &'o [Object]) -> Slots<'o> {
        let active = objects.iter().all(|obj| {
            let managed = obj.symbols.iter().any(|s| {
                s.defined
                    && (s.name.starts_with(TYPE_DESC_PREFIX)
                        || s.name.starts_with(STACKMAP_RECORD_PREFIX))
            });
            !managed || records_dispatches(obj)
        });
        let mut calls: BTreeMap<&'o str, Vec<DispatchSlot>> = BTreeMap::new();
        let mut bases: BTreeMap<&'o str, BTreeSet<&'o str>> = BTreeMap::new();
        for obj in objects.iter().filter(|obj| records_dispatches(obj)) {
            for s in &obj.symbols {
                if let Some((slot, caller)) = dispatch_record(&s.name) {
                    calls.entry(caller).or_default().push(slot);
                } else if let Some((descriptor, base)) = hierarchy_record(&s.name) {
                    let edges = bases.entry(descriptor).or_default();
                    if let Some(base) = base {
                        edges.insert(base);
                    }
                }
            }
        }
        let ancestry = bases
            .keys()
            .map(|&descriptor| (descriptor, ancestors_of(&bases, descriptor)))
            .collect();
        Slots {
            active,
            calls,
            ancestry,
            any_slots: BTreeSet::new(),
            declared_slots: BTreeMap::new(),
            tags: BTreeSet::new(),
        }
    }

    /// Whether the analysis is on.
    pub(crate) fn active(&self) -> bool {
        self.active
    }

    /// Marks `caller` kept: every table entry it dispatches through becomes live. Returns the
    /// entries' keys that were not live before -- a slot index or a tag -- so a walk can look again
    /// at the entries it set aside under them.
    pub(crate) fn keep_caller(&mut self, caller: &str) -> Vec<TableEntry> {
        let Slots {
            calls,
            any_slots,
            declared_slots,
            tags,
            ..
        } = self;
        let mut newly = Vec::new();
        for record in calls.get(caller).into_iter().flatten() {
            let (key, added) = match record {
                DispatchSlot::Virtual { slot, declaring } => (
                    TableEntry::Slot(*slot),
                    declared_slots
                        .entry(*slot)
                        .or_default()
                        .insert(declaring.clone()),
                ),
                DispatchSlot::AnyVirtual(slot) => {
                    (TableEntry::Slot(*slot), any_slots.insert(*slot))
                }
                DispatchSlot::Interface(tag) => (TableEntry::Tag(*tag), tags.insert(*tag)),
            };
            if added {
                newly.push(key);
            }
        }
        newly
    }

    /// Whether the entry `entry` of the descriptor named `descriptor` is live: some kept function
    /// dispatches through it on a receiver that can be of the descriptor's type.
    pub(crate) fn live(&self, descriptor: &str, entry: TableEntry) -> bool {
        match entry {
            TableEntry::Tag(tag) => self.tags.contains(&tag),
            TableEntry::Slot(slot) => {
                if self.any_slots.contains(&slot) {
                    return true;
                }
                let Some(declaring) = self.declared_slots.get(&slot) else {
                    return false;
                };
                match self.ancestry.get(descriptor) {
                    Some(Some(ancestors)) => declaring
                        .iter()
                        .any(|declared| ancestors.contains(declared.as_str())),
                    _ => true,
                }
            }
        }
    }

    /// Whether a relocation from the descriptor `descriptor` in `obj` to `target` is followed now:
    /// always while the analysis is off, for an object that does not record its dispatches, for any
    /// word that is not a table entry, and for an entry already pointed at the trap; otherwise only
    /// when the entry is live. A table entry that is not live yet answers its key, under which a
    /// walk sets it aside until a kept caller makes it live.
    pub(crate) fn follows(
        &self,
        obj: &Object,
        descriptor: &str,
        r: &ParsedRelocation,
        target: &str,
    ) -> Result<(), TableEntry> {
        if !self.active || !records_dispatches(obj) || target == UNUSED_SLOT_TRAP {
            return Ok(());
        }
        match table_entry(obj, r) {
            Some(entry) if !self.live(descriptor, entry) => Err(entry),
            _ => Ok(()),
        }
    }
}

/// `descriptor` and every ancestor its chain records, or `None` when some descriptor on the chain
/// has no record at all. Duplicate copies of one descriptor are one node, so they never read as a
/// cycle; a malformed cycle ends where it closes.
fn ancestors_of<'o>(
    bases: &BTreeMap<&'o str, BTreeSet<&'o str>>,
    descriptor: &'o str,
) -> Option<BTreeSet<&'o str>> {
    let mut ancestors = BTreeSet::new();
    let mut pending = alloc::vec![descriptor];
    while let Some(current) = pending.pop() {
        if !ancestors.insert(current) {
            continue;
        }
        pending.extend(bases.get(current)?.iter().copied());
    }
    Some(ancestors)
}
