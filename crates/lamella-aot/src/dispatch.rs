//! The dispatch records an emitted object carries, read off the MIR it lowers: which table entries
//! each function dispatches through, and each laid descriptor's immediate base. The format, and why
//! a link wants them, are [`lamella_elf::dispatch`]'s.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use lamella_elf::dispatch::{
    DISPATCH_RECORDS, DispatchSlot, dispatch_record_name, hierarchy_record_name,
};
use lamella_ir::{Function, Inst, TypeHandle};

use crate::resolver::{DescQualifiers, TypeMeta, descriptor_symbol, reference_handle_parts};

/// Every record name for an object of `funcs`, each named by the entry of `names` at its index, that
/// lays `descriptors`: the mark, one record per table entry a function dispatches through, and one
/// per descriptor whose base is known. Sorted and without duplicates, so the records do not depend
/// on the order the functions were lowered in.
///
/// **A DESCRIPTOR RECORDS THE END OF ITS CHAIN ONLY WHEN ITS TYPE HAS NO BASE**, which among classes
/// is `System.Object` alone. A descriptor's `base` is also `None` when the base cannot be named here
/// -- a base outside the plan, or one in another assembly's token space -- and recording that as an
/// end would tell the link the type derives from nothing, so a call declared on `object` would not
/// keep this type's override. Such a descriptor records nothing: another copy may name its base,
/// and if none does, the link keeps all of its slots.
pub(crate) fn record_names(
    funcs: &[Function],
    names: &[&str],
    descriptors: &[TypeMeta],
    qualifiers: &DescQualifiers,
) -> Vec<String> {
    let mut records = BTreeSet::new();
    records.insert(String::from(DISPATCH_RECORDS));
    for descriptor in descriptors {
        let Some(symbol) = named_descriptor(descriptor.handle, qualifiers) else {
            continue;
        };
        match descriptor.base {
            Some(base) => {
                if let Some(base) = named_descriptor(base, qualifiers) {
                    records.insert(hierarchy_record_name(&symbol, Some(&base)));
                }
            }
            None if descriptor.full_name.as_deref() == Some("System.Object") => {
                records.insert(hierarchy_record_name(&symbol, None));
            }
            None => {}
        }
    }
    for (func, name) in funcs.iter().zip(names) {
        for (_, inst) in func.blocks.iter().flat_map(|block| &block.insts) {
            let slot = match inst {
                Inst::CallVirtual {
                    slot,
                    declaring_type,
                    ..
                }
                | Inst::VirtualFuncAddr {
                    slot,
                    declaring_type,
                    ..
                } => match declaring_type.and_then(|handle| named_descriptor(handle, qualifiers)) {
                    Some(declaring) => DispatchSlot::Virtual {
                        slot: *slot,
                        declaring,
                    },
                    None => DispatchSlot::AnyVirtual(*slot),
                },
                Inst::CallInterface { tag, .. } => DispatchSlot::Interface(*tag),
                _ => continue,
            };
            records.insert(dispatch_record_name(&slot, name));
        }
    }
    records.into_iter().collect()
}

/// The symbol a record name is written as: local, of size zero, at the start of the text, and the
/// target of no relocation.
pub(crate) fn record_symbol(name: &str) -> lamella_elf::Symbol<'_> {
    lamella_elf::Symbol {
        name,
        value: 0,
        size: 0,
        binding: lamella_elf::Binding::Local,
        kind: lamella_elf::SymbolType::NoType,
        section: lamella_elf::SymbolSection::Text,
    }
}

/// The descriptor symbol `handle` is laid under, or `None` for a reference handle this object has
/// no qualifier for -- a record that cannot name a descriptor names none, rather than a wrong one.
fn named_descriptor(handle: TypeHandle, qualifiers: &DescQualifiers) -> Option<String> {
    if let Some((ordinal, _)) = reference_handle_parts(handle)
        && ordinal >= qualifiers.references.len()
    {
        return None;
    }
    Some(descriptor_symbol(handle.0, qualifiers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use lamella_elf::dispatch::{dispatch_record, hierarchy_record};
    use lamella_ir::{BasicBlock, BlockId, MirType, Terminator, ValueId};

    /// A program build's qualifiers: its own descriptors under `own`, the corlib's under `corlib`.
    fn qualifiers() -> DescQualifiers {
        DescQualifiers {
            own: Some(String::from("own00001")),
            references: vec![String::from("corlib01")],
            string: None,
        }
    }

    /// A descriptor with no tables, named `full_name`, deriving from `base`.
    fn descriptor(handle: u32, base: Option<u32>, full_name: &str) -> TypeMeta {
        TypeMeta {
            handle: TypeHandle(handle),
            type_tag: 0,
            vtable: vec![],
            itable: vec![],
            base: base.map(TypeHandle),
            words: None,
            exported: true,
            full_name: Some(full_name.into()),
        }
    }

    /// One function of the instructions `insts`, its receiver the one parameter.
    fn function(insts: Vec<Inst>) -> Function {
        let insts: Vec<(ValueId, Inst)> = insts
            .into_iter()
            .enumerate()
            .map(|(i, inst)| (ValueId(i as u32 + 1), inst))
            .collect();
        let count = insts.len();
        Function {
            params: vec![MirType::ObjectRef],
            ret: None,
            value_types: core::iter::once(MirType::ObjectRef)
                .chain((0..count).map(|_| MirType::I32))
                .collect(),
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts,
                terminator: Some(Terminator::Return(None)),
            }],
        }
    }

    /// The reference handle the corlib's TypeDef row `row` is named by in a program build.
    fn corlib_type(row: u32) -> TypeHandle {
        crate::resolver::reference_handle(0, 0x0200_0000 | row)
    }

    #[test]
    fn each_dispatch_records_its_slot_or_tag_and_the_type_that_declares_it() {
        let object_type = corlib_type(1);
        let funcs = vec![function(vec![
            Inst::CallVirtual {
                slot: 0,
                declaring_type: Some(object_type),
                args: vec![ValueId(0)],
                returns_value: false,
            },
            Inst::VirtualFuncAddr {
                object: ValueId(0),
                slot: 2,
                declaring_type: Some(TypeHandle(0x0200_0003)),
            },
            Inst::CallVirtual {
                slot: 5,
                declaring_type: None,
                args: vec![ValueId(0)],
                returns_value: false,
            },
            Inst::CallInterface {
                tag: 0xBEEF,
                args: vec![ValueId(0)],
                returns_value: false,
            },
        ])];
        let records = record_names(&funcs, &["Main"], &[], &qualifiers());
        let mut dispatches: Vec<(DispatchSlot, &str)> = records
            .iter()
            .filter_map(|name| dispatch_record(name))
            .collect();
        dispatches.sort();
        let object_symbol = descriptor_symbol(object_type.0, &qualifiers());
        let own_symbol = descriptor_symbol(0x0200_0003, &qualifiers());
        assert_eq!(
            dispatches,
            [
                (
                    DispatchSlot::Virtual {
                        slot: 0,
                        declaring: object_symbol
                    },
                    "Main"
                ),
                (
                    DispatchSlot::Virtual {
                        slot: 2,
                        declaring: own_symbol
                    },
                    "Main"
                ),
                (DispatchSlot::AnyVirtual(5), "Main"),
                (DispatchSlot::Interface(0xBEEF), "Main"),
            ]
        );
        assert!(
            records.iter().any(|name| name == DISPATCH_RECORDS),
            "the object marks that it records its dispatches"
        );
    }

    /// A descriptor records the end of its chain only as `System.Object`: any other descriptor
    /// with no base named here is a type whose base this build could not name, and recording an
    /// end for it would tell the link it derives from nothing.
    #[test]
    fn only_system_object_records_the_end_of_its_chain() {
        let object_type = corlib_type(1);
        let descriptors = vec![
            descriptor(0x0200_0002, Some(object_type.0), "Shapes.Shape"),
            descriptor(object_type.0, None, "System.Object"),
            descriptor(0x0200_0004, None, "Shapes.Elsewhere"),
        ];
        let records = record_names(&[], &[], &descriptors, &qualifiers());
        let mut chains: Vec<(String, Option<String>)> = records
            .iter()
            .filter_map(|name| hierarchy_record(name))
            .map(|(d, base)| (String::from(d), base.map(String::from)))
            .collect();
        chains.sort();
        let symbol = |handle: u32| descriptor_symbol(handle, &qualifiers());
        let mut expected = vec![
            (symbol(0x0200_0002), Some(symbol(object_type.0))),
            (symbol(object_type.0), None),
        ];
        expected.sort();
        assert_eq!(
            chains, expected,
            "`Shapes.Elsewhere` has no base named here and records nothing"
        );
    }

    /// A record never names a descriptor this object has no qualifier for: the call falls back to
    /// the slot alone, which keeps the slot everywhere, rather than naming a different type.
    #[test]
    fn a_declaring_type_with_no_qualifier_records_the_slot_alone() {
        let unqualified = crate::resolver::reference_handle(3, 0x0200_0001);
        let funcs = vec![function(vec![Inst::CallVirtual {
            slot: 1,
            declaring_type: Some(unqualified),
            args: vec![ValueId(0)],
            returns_value: false,
        }])];
        let records = record_names(&funcs, &["Main"], &[], &qualifiers());
        assert!(
            records
                .iter()
                .any(|name| dispatch_record(name) == Some((DispatchSlot::AnyVirtual(1), "Main"))),
            "got {records:?}"
        );
    }
}
