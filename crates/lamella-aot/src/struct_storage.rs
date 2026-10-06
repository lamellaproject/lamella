//! Which struct values keep ONE storage across a function's edges, rather than a copy per block.
//!
//! The front end threads every local through a join's or a loop head's block parameters, an
//! address-taken one included: a scalar whose address is taken as a one-word struct cell, and a
//! struct as its own storage. A struct block parameter normally takes a COPY: on every edge, each
//! argument's bytes move into the parameter's storage, all of them as one simultaneous move. That
//! copy is what keeps a value intact across a loop's back edge while the code that made it runs
//! again and refills its own storage -- in `for (...) { F(prev, prev = cur); }` the first argument
//! is `prev` as it was before the assignment.
//!
//! A copy is wrong only when a POINTER into the storage outlives the block that took it: `int* p =
//! &x;` before a loop that writes through `p`, and a read of `x` after it, name one storage two ways,
//! and a copy at the loop's head would split it in two -- the read sees the copy and the write went
//! to the original. So every value joined by edges to storage whose address escapes its block keeps
//! one storage instead. An address escapes when the pointer, or one computed from it, is stored to
//! memory, passed along an edge, returned, or read in another block.
//!
//! How a backend keeps one storage is its own: on WebAssembly a struct value is the address of its
//! bytes, so a sharing parameter takes its argument's address; on ARM and RISC-V every member of
//! such a class lives in one stack slot, so its edge moves are moves of a slot onto itself, which the
//! schedule drops. The answer to WHICH storage is shared is here, once.

use alloc::vec::Vec;

use lamella_ir::{BlockId, Function, Inst, MirType, Terminator, ValueId};

/// The size of `value`'s bytes, for a struct that has any.
pub(crate) fn struct_bytes(value_types: &[MirType], value: ValueId) -> Option<u32> {
    match value_types.get(value.index()) {
        Some(MirType::ValueType { size, .. }) if *size > 0 => Some(*size),
        _ => None,
    }
}

/// The classes of storage a function's edges join through its struct block parameters, and which
/// of them must stay one storage.
pub(crate) struct StorageClasses {
    /// Each value's class, named by its lowest-numbered member.
    class: Vec<usize>,
    /// Per class: a pointer into its storage outlives the block that took it.
    escapes: Vec<bool>,
    /// Per class: an edge joins a struct parameter to an argument with nothing to copy -- a struct
    /// with no bytes, or a value that is not a struct at all.
    #[cfg_attr(not(feature = "wasm"), allow(dead_code))]
    nothing_to_copy: Vec<bool>,
    /// Per class: one stack slot can hold every member. Each is a struct and all take the same slot
    /// size; none is a parameter of the entry block, whose storage is the function's own argument;
    /// and no two are parameters of one block, which would be two destinations of one edge.
    #[cfg_attr(not(any(feature = "arm32", feature = "riscv32")), allow(dead_code))]
    one_slot: Vec<bool>,
    /// Per value: whether it is a struct.
    is_struct: Vec<bool>,
}

impl StorageClasses {
    /// The classes of `func`'s struct storage.
    pub(crate) fn of(func: &Function) -> StorageClasses {
        let count = func.value_types.len();
        let is_struct: Vec<bool> = func
            .value_types
            .iter()
            .map(|ty| matches!(ty, MirType::ValueType { .. }))
            .collect();
        fn root(parent: &mut [usize], mut value: usize) -> usize {
            while parent[value] != value {
                parent[value] = parent[parent[value]];
                value = parent[value];
            }
            value
        }
        let edges = |terminator: &Option<Terminator>| -> Vec<(BlockId, Vec<ValueId>)> {
            match terminator {
                Some(Terminator::Jump { target, args }) => alloc::vec![(*target, args.clone())],
                Some(Terminator::Branch {
                    if_true,
                    true_args,
                    if_false,
                    false_args,
                    ..
                }) => alloc::vec![
                    (*if_true, true_args.clone()),
                    (*if_false, false_args.clone()),
                ],
                _ => Vec::new(),
            }
        };

        let mut parent: Vec<usize> = (0..count).collect();
        let mut nothing_to_copy: Vec<ValueId> = Vec::new();
        for block in &func.blocks {
            for (target, args) in edges(&block.terminator) {
                let Some(target) = func.blocks.get(target.index()) else {
                    continue;
                };
                for (&param, &arg) in target.params.iter().zip(&args) {
                    if is_struct[param.index()] && arg.index() < count {
                        let (a, b) = (
                            root(&mut parent, param.index()),
                            root(&mut parent, arg.index()),
                        );
                        parent[a] = b;
                        if struct_bytes(&func.value_types, param).is_none()
                            || struct_bytes(&func.value_types, arg).is_none()
                        {
                            nothing_to_copy.push(param);
                        }
                    }
                }
            }
        }

        let mut points_into: Vec<Option<usize>> = alloc::vec![None; count];
        let mut defined_in: Vec<usize> = alloc::vec![usize::MAX; count];
        for (index, block) in func.blocks.iter().enumerate() {
            for &param in &block.params {
                if let Some(slot) = defined_in.get_mut(param.index()) {
                    *slot = index;
                }
            }
            for (result, inst) in &block.insts {
                if result.index() >= count {
                    continue;
                }
                defined_in[result.index()] = index;
                let pointer_of =
                    |value: &ValueId| points_into.get(value.index()).copied().flatten();
                let into = match inst {
                    Inst::FieldAddr { base, .. } if is_struct[base.index()] => {
                        Some(root(&mut parent, base.index()))
                    }
                    Inst::FieldAddr { base: value, .. } | Inst::Convert { value, .. } => {
                        pointer_of(value)
                    }
                    Inst::Binary { lhs, rhs, .. } => pointer_of(lhs).or_else(|| pointer_of(rhs)),
                    _ => None,
                };
                points_into[result.index()] = into;
            }
        }

        let mut escapes = alloc::vec![false; count];
        for (index, block) in func.blocks.iter().enumerate() {
            let mut pin_if_pointer = |value: ValueId| {
                if let Some(class) = points_into.get(value.index()).copied().flatten() {
                    escapes[class] = true;
                }
            };
            for (_, inst) in &block.insts {
                crate::regalloc::each_inst_use(inst, |used| {
                    if defined_in.get(used.index()).is_some_and(|&b| b != index) {
                        pin_if_pointer(used);
                    }
                });
                match inst {
                    Inst::Store { value, .. }
                    | Inst::FieldStore { value, .. }
                    | Inst::FieldStoreNarrow { value, .. }
                    | Inst::StaticStore { value, .. }
                    | Inst::ArrayStore { value, .. }
                    | Inst::Array2DStore { value, .. }
                    | Inst::ArrayMDStore { value, .. } => pin_if_pointer(*value),
                    _ => {}
                }
            }
            match &block.terminator {
                Some(Terminator::Return(Some(value))) => pin_if_pointer(*value),
                Some(Terminator::Branch { cond, .. })
                    if defined_in.get(cond.index()).is_some_and(|&b| b != index) =>
                {
                    pin_if_pointer(*cond);
                }
                _ => {}
            }
            for (_, args) in edges(&block.terminator) {
                args.into_iter().for_each(&mut pin_if_pointer);
            }
        }
        let mut copies_nothing = alloc::vec![false; count];
        for param in nothing_to_copy {
            copies_nothing[root(&mut parent, param.index())] = true;
        }

        let roots: Vec<usize> = (0..count).map(|value| root(&mut parent, value)).collect();
        let mut lowest = alloc::vec![usize::MAX; count];
        for (value, &root) in roots.iter().enumerate() {
            lowest[root] = lowest[root].min(value);
        }
        let class: Vec<usize> = roots.iter().map(|&root| lowest[root]).collect();
        let mut class_escapes = alloc::vec![false; count];
        let mut class_copies_nothing = alloc::vec![false; count];
        for value in 0..count {
            let root = roots[value];
            class_escapes[class[value]] |= escapes[root];
            class_copies_nothing[class[value]] |= copies_nothing[root];
        }

        let mut one_slot = alloc::vec![true; count];
        let mut slot_bytes: Vec<Option<u32>> = alloc::vec![None; count];
        for (value, ty) in func.value_types.iter().enumerate() {
            let c = class[value];
            let bytes = ty.stack_slot_bytes();
            if !is_struct[value] || slot_bytes[c].is_some_and(|first| first != bytes) {
                one_slot[c] = false;
            }
            slot_bytes[c] = Some(bytes);
        }
        let mut param_of: Vec<Option<usize>> = alloc::vec![None; count];
        for (index, block) in func.blocks.iter().enumerate() {
            for &param in &block.params {
                let c = class[param.index()];
                if index == func.entry.index() || param_of[c] == Some(index) {
                    one_slot[c] = false;
                }
                param_of[c] = Some(index);
            }
        }

        StorageClasses {
            class,
            escapes: class_escapes,
            nothing_to_copy: class_copies_nothing,
            one_slot,
            is_struct,
        }
    }

    /// Whether each value is a struct that shares its class's storage rather than taking a copy on
    /// its edges: every struct joined to storage a pointer escapes from, and every struct joined to
    /// an argument with nothing to copy. Indexed by value.
    #[cfg_attr(not(feature = "wasm"), allow(dead_code))]
    pub(crate) fn shared(&self) -> Vec<bool> {
        (0..self.class.len())
            .map(|value| {
                let class = self.class[value];
                self.is_struct[value] && (self.escapes[class] || self.nothing_to_copy[class])
            })
            .collect()
    }

    /// The value whose stack slot `value` lives in, when its storage must stay one and one slot can
    /// hold its whole class: the class's lowest-numbered member, which may be `value` itself. `None`
    /// for a value that keeps a slot of its own and takes copies on its edges.
    #[cfg_attr(not(any(feature = "arm32", feature = "riscv32")), allow(dead_code))]
    pub(crate) fn slot_owner(&self, value: ValueId) -> Option<ValueId> {
        let class = *self.class.get(value.index())?;
        (self.escapes[class] && self.one_slot[class]).then_some(ValueId(class as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_ir::{BasicBlock, RefWords, TypeHandle};

    /// A one-word struct cell: what the front end makes of a scalar local whose address is taken.
    fn cell() -> MirType {
        MirType::ValueType {
            handle: TypeHandle(0),
            refs: RefWords::NONE,
            size: 4,
        }
    }

    /// `int x; int* p = &x;`, then a join whose parameter carries the cell, a write through `p` and a
    /// read of `x` there. The pointer is taken in the entry block or in the join itself.
    fn pointer_through_a_join(pointer_in_entry: bool) -> Function {
        let address = (
            ValueId(1),
            Inst::FieldAddr {
                base: if pointer_in_entry {
                    ValueId(0)
                } else {
                    ValueId(2)
                },
                offset: 0,
            },
        );
        let mut entry = alloc::vec![(ValueId(0), Inst::InitStruct)];
        let mut join = Vec::new();
        if pointer_in_entry {
            entry.push(address);
        } else {
            join.push(address);
        }
        join.push((
            ValueId(3),
            Inst::ConstInt {
                ty: MirType::I32,
                value: 7,
            },
        ));
        join.push((
            ValueId(4),
            Inst::Store {
                address: ValueId(1),
                value: ValueId(3),
                width: 4,
            },
        ));
        join.push((
            ValueId(5),
            Inst::FieldLoad {
                base: ValueId(2),
                offset: 0,
            },
        ));
        Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: alloc::vec![
                cell(),
                MirType::ManagedPtr,
                cell(),
                MirType::I32,
                MirType::I32,
                MirType::I32,
            ],
            entry: BlockId(0),
            blocks: alloc::vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: entry,
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: alloc::vec![ValueId(0)],
                    }),
                },
                BasicBlock {
                    params: alloc::vec![ValueId(2)],
                    insts: join,
                    terminator: Some(Terminator::Return(Some(ValueId(5)))),
                },
            ],
        }
    }

    /// A POINTER USED PAST THE BLOCK THAT TOOK IT KEEPS ITS STORAGE ONE ACROSS THE JOIN, and one used
    /// only where it was taken leaves the join's parameter its own copy.
    #[test]
    fn a_pointer_used_past_its_block_keeps_its_storage_one_across_the_join() {
        let escaping = StorageClasses::of(&pointer_through_a_join(true));
        assert_eq!(
            escaping.slot_owner(ValueId(0)),
            Some(ValueId(0)),
            "the cell owns the slot"
        );
        assert_eq!(
            escaping.slot_owner(ValueId(2)),
            Some(ValueId(0)),
            "the join's parameter shares it"
        );
        assert!(
            escaping.shared()[2],
            "and WebAssembly shares the cell's address with it"
        );

        let local = StorageClasses::of(&pointer_through_a_join(false));
        assert_eq!(
            local.slot_owner(ValueId(2)),
            None,
            "a pointer that stays in its block pins nothing"
        );
        assert!(
            !local.shared()[2],
            "the parameter takes a copy on WebAssembly too"
        );
    }
}
