//! Liveness analysis over the middle IR, the foundation for register allocation.

use alloc::vec::Vec;

use lamella_ir::{BlockId, Function, Inst, Terminator, ValueId};

/// The values live on entry to and exit from each block, indexed by block number.
/// Sets are bitsets over value ids (one `bool` per value).
pub struct Liveness {
    live_in: Vec<Vec<bool>>,
    live_out: Vec<Vec<bool>>,
}

impl Liveness {
    /// Computes liveness for `func` by backward dataflow to a fixpoint.
    pub fn analyze(func: &Function) -> Liveness {
        let n = func.blocks.len();
        let m = func.value_types.len();

        let mut uses: Vec<Vec<bool>> = Vec::with_capacity(n);
        let mut defs: Vec<Vec<bool>> = Vec::with_capacity(n);
        for block in &func.blocks {
            let mut block_uses = bitset(m);
            let mut block_defs = bitset(m);
            for &param in &block.params {
                set(&mut block_defs, param);
            }
            for (result, inst) in &block.insts {
                each_inst_use(inst, |u| {
                    if !get(&block_defs, u) {
                        set(&mut block_uses, u);
                    }
                });
                set(&mut block_defs, *result);
            }
            each_terminator_use(&block.terminator, |u| {
                if !get(&block_defs, u) {
                    set(&mut block_uses, u);
                }
            });
            uses.push(block_uses);
            defs.push(block_defs);
        }

        let mut live_in: Vec<Vec<bool>> = (0..n).map(|_| bitset(m)).collect();
        let mut live_out: Vec<Vec<bool>> = (0..n).map(|_| bitset(m)).collect();

        let mut changed = true;
        while changed {
            changed = false;
            for b in (0..n).rev() {
                for succ in successors(&func.blocks[b].terminator) {
                    if let Some(in_s) = live_in.get(succ.index()) {
                        if union_into(&mut live_out[b], in_s) {
                            changed = true;
                        }
                    }
                }
                for i in 0..m {
                    let want = uses[b][i] || (live_out[b][i] && !defs[b][i]);
                    if want && !live_in[b][i] {
                        live_in[b][i] = true;
                        changed = true;
                    }
                }
            }
        }

        Liveness { live_in, live_out }
    }

    /// Whether `value` is live on entry to block `block`.
    pub fn live_in(&self, block: usize, value: ValueId) -> bool {
        self.live_in
            .get(block)
            .and_then(|s| s.get(value.index()))
            .copied()
            .unwrap_or(false)
    }

    /// Whether `value` is live on exit from block `block`.
    pub fn live_out(&self, block: usize, value: ValueId) -> bool {
        self.live_out
            .get(block)
            .and_then(|s| s.get(value.index()))
            .copied()
            .unwrap_or(false)
    }
}

/// A fresh bitset over `m` value ids, all clear.
fn bitset(m: usize) -> Vec<bool> {
    let mut v = Vec::new();
    v.resize(m, false);
    v
}

fn set(s: &mut [bool], value: ValueId) {
    if let Some(slot) = s.get_mut(value.index()) {
        *slot = true;
    }
}

fn get(s: &[bool], value: ValueId) -> bool {
    s.get(value.index()).copied().unwrap_or(false)
}

/// Sets every bit of `dst` that is set in `src`, reporting whether `dst` changed.
fn union_into(dst: &mut [bool], src: &[bool]) -> bool {
    let mut changed = false;
    for (d, s) in dst.iter_mut().zip(src) {
        if *s && !*d {
            *d = true;
            changed = true;
        }
    }
    changed
}

/// Calls `f` with each value an instruction reads.
fn each_inst_use(inst: &Inst, mut f: impl FnMut(ValueId)) {
    match inst {
        Inst::ConstInt { .. } => {}
        Inst::Binary { lhs, rhs, .. } | Inst::Compare { lhs, rhs, .. } => {
            f(*lhs);
            f(*rhs);
        }
    }
}

/// Calls `f` with each value a terminator reads (branch condition and arguments,
/// or a returned value).
fn each_terminator_use(terminator: &Option<Terminator>, mut f: impl FnMut(ValueId)) {
    match terminator {
        Some(Terminator::Jump { args, .. }) => args.iter().for_each(|a| f(*a)),
        Some(Terminator::Branch {
            cond,
            true_args,
            false_args,
            ..
        }) => {
            f(*cond);
            true_args.iter().for_each(|a| f(*a));
            false_args.iter().for_each(|a| f(*a));
        }
        Some(Terminator::Return(Some(v))) => f(*v),
        _ => {}
    }
}

/// The blocks a terminator may transfer control to.
fn successors(terminator: &Option<Terminator>) -> Vec<BlockId> {
    let mut succ = Vec::new();
    match terminator {
        Some(Terminator::Jump { target, .. }) => succ.push(*target),
        Some(Terminator::Branch {
            if_true, if_false, ..
        }) => {
            succ.push(*if_true);
            succ.push(*if_false);
        }
        _ => {}
    }
    succ
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_ir::{BasicBlock, BinOp, CmpOp, MirType};

    /// A counting loop: block 0 sets up, block 1 (header) compares, block 2 (body)
    /// updates and jumps back, block 3 returns. The bound (v2) is defined in block 0
    /// and read in block 1, so it stays live around the loop -- including through
    /// the body, which never mentions it.
    fn loop_function() -> Function {
        Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32; 8],
            entry: BlockId(0),
            blocks: vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![
                        (
                            ValueId(0),
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: 0,
                            },
                        ),
                        (
                            ValueId(1),
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: 1,
                            },
                        ),
                        (
                            ValueId(2),
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: 5,
                            },
                        ),
                    ],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: vec![ValueId(0), ValueId(1)],
                    }),
                },
                BasicBlock {
                    params: vec![ValueId(3), ValueId(4)],
                    insts: vec![(
                        ValueId(5),
                        Inst::Compare {
                            op: CmpOp::SignedGt,
                            lhs: ValueId(4),
                            rhs: ValueId(2),
                        },
                    )],
                    terminator: Some(Terminator::Branch {
                        cond: ValueId(5),
                        if_true: BlockId(3),
                        true_args: Vec::new(),
                        if_false: BlockId(2),
                        false_args: Vec::new(),
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![
                        (
                            ValueId(6),
                            Inst::Binary {
                                op: BinOp::Add,
                                lhs: ValueId(3),
                                rhs: ValueId(4),
                            },
                        ),
                        (
                            ValueId(7),
                            Inst::Binary {
                                op: BinOp::Add,
                                lhs: ValueId(4),
                                rhs: ValueId(1),
                            },
                        ),
                    ],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: vec![ValueId(6), ValueId(7)],
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Some(Terminator::Return(Some(ValueId(3)))),
                },
            ],
        }
    }

    #[test]
    fn loop_carried_value_is_live_around_the_loop() {
        let live = Liveness::analyze(&loop_function());
        assert!(live.live_out(0, ValueId(2)));
        assert!(live.live_in(1, ValueId(2)));
        assert!(live.live_in(2, ValueId(2)));
        assert!(live.live_out(2, ValueId(2)));
        assert!(!live.live_in(3, ValueId(2)));
    }

    #[test]
    fn increment_constant_is_live_through_the_body() {
        let live = Liveness::analyze(&loop_function());
        assert!(live.live_in(2, ValueId(1)));
        assert!(!live.live_out(1, ValueId(5)));
        assert!(!live.live_in(2, ValueId(5)));
    }
}
