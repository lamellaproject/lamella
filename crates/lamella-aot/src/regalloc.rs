//! Liveness analysis over the middle IR, the foundation for register allocation.

use alloc::vec;
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

    /// Whether any value is live across a call -- defined before a call and used
    /// after it. Such a value cannot survive in a caller-saved register, which the
    /// call clobbers, so the lowering keeps it on the stack instead.
    pub fn any_value_live_across_call(&self, func: &Function) -> bool {
        for (b, block) in func.blocks.iter().enumerate() {
            let mut live = self.live_out[b].clone();
            each_terminator_use(&block.terminator, |u| set(&mut live, u));
            for (result, inst) in block.insts.iter().rev() {
                if matches!(inst, Inst::Call { .. })
                    && live
                        .iter()
                        .enumerate()
                        .any(|(v, &alive)| alive && v != result.index())
                {
                    return true;
                }
                live[result.index()] = false;
                each_inst_use(inst, |u| set(&mut live, u));
            }
        }
        false
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

/// A value's live interval: the half-open span of program points from its first
/// definition to its last use, conservatively contiguous across any holes. The
/// linear scan sorts intervals by `start` and assigns registers, spilling the
/// interval that ends furthest away when it runs out.
#[derive(Debug, Clone, Copy)]
pub struct Interval {
    /// The first program point at which the value is live.
    pub start: u32,
    /// The last program point at which the value is live.
    pub end: u32,
}

/// Builds a live interval per value from the block-level liveness, numbering
/// program points in block-layout order (a block-entry point, one per instruction,
/// then a terminator point). A value live into or out of a block extends to that
/// block's entry or terminator point, so loop-carried values span the loop.
pub fn live_intervals(func: &Function, live: &Liveness) -> Vec<Interval> {
    let m = func.value_types.len();
    let mut lo = vec![u32::MAX; m];
    let mut hi = vec![0u32; m];
    let mut defined = vec![false; m];

    let mut point = 0u32;
    for (b, block) in func.blocks.iter().enumerate() {
        let entry = point;
        point += 1;
        for &param in &block.params {
            mark(&mut lo, &mut hi, &mut defined, param, entry);
        }
        for v in 0..m {
            if live.live_in(b, ValueId(v as u32)) {
                lo[v] = lo[v].min(entry);
                defined[v] = true;
            }
        }
        for (result, inst) in &block.insts {
            let ip = point;
            point += 1;
            match inst {
                Inst::Binary { lhs, rhs, .. } | Inst::Compare { lhs, rhs, .. } => {
                    mark(&mut lo, &mut hi, &mut defined, *lhs, ip);
                    mark(&mut lo, &mut hi, &mut defined, *rhs, ip);
                }
                Inst::Call { args, .. } => {
                    for arg in args {
                        mark(&mut lo, &mut hi, &mut defined, *arg, ip);
                    }
                }
                Inst::Store { address, value } => {
                    mark(&mut lo, &mut hi, &mut defined, *address, ip);
                    mark(&mut lo, &mut hi, &mut defined, *value, ip);
                }
                Inst::ConstInt { .. } => {}
            }
            mark(&mut lo, &mut hi, &mut defined, *result, ip);
        }
        let term = point;
        point += 1;
        match &block.terminator {
            Some(Terminator::Jump { args, .. }) => {
                for a in args {
                    mark(&mut lo, &mut hi, &mut defined, *a, term);
                }
            }
            Some(Terminator::Branch {
                cond,
                true_args,
                false_args,
                ..
            }) => {
                mark(&mut lo, &mut hi, &mut defined, *cond, term);
                for a in true_args.iter().chain(false_args) {
                    mark(&mut lo, &mut hi, &mut defined, *a, term);
                }
            }
            Some(Terminator::Return(Some(v))) => mark(&mut lo, &mut hi, &mut defined, *v, term),
            _ => {}
        }
        for v in 0..m {
            if live.live_out(b, ValueId(v as u32)) {
                hi[v] = hi[v].max(term);
                defined[v] = true;
            }
        }
    }

    (0..m)
        .map(|v| Interval {
            start: if defined[v] { lo[v] } else { 0 },
            end: hi[v],
        })
        .collect()
}

fn mark(lo: &mut [u32], hi: &mut [u32], defined: &mut [bool], value: ValueId, point: u32) {
    let i = value.index();
    if i < lo.len() {
        lo[i] = lo[i].min(point);
        hi[i] = hi[i].max(point);
        defined[i] = true;
    }
}

/// Where the allocator places a value: an allocatable register index (in
/// `0..reg_count`, which the target maps to a real machine register) or a spill slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// An allocatable register, numbered from zero.
    Register(u32),
    /// A stack spill slot, numbered from zero.
    Spill(u32),
}

/// The result of register allocation: a location per value, the number of registers
/// used (for the prologue's saves), and the number of spill slots (for the frame).
pub struct Allocation {
    /// The location chosen for each value, indexed by value id.
    pub locations: Vec<Location>,
    /// The count of distinct registers used (highest index plus one). The
    /// callee-saved prologue consults it; unused until spilling and saves land.
    #[allow(dead_code)]
    pub registers_used: u32,
    /// The count of spill slots used.
    pub spill_count: u32,
}

/// Linear-scan register allocation (Poletto and Sarkar): intervals are taken in
/// order of start point, each claiming a free register; when none is free, the
/// interval reaching furthest -- the new one or an active one -- is spilled. With
/// `reg_count` registers and never a panic, even for zero registers.
pub fn allocate(intervals: &[Interval], reg_count: usize) -> Allocation {
    let mut order: Vec<usize> = (0..intervals.len()).collect();
    order.sort_by_key(|&v| intervals[v].start);

    let mut locations = vec![Location::Spill(0); intervals.len()];
    let mut active: Vec<usize> = Vec::new();
    let mut free: Vec<u32> = (0..reg_count as u32).rev().collect();
    let mut spill_count = 0u32;
    let mut registers_used = 0u32;

    for &v in &order {
        let start = intervals[v].start;
        let mut kept = Vec::with_capacity(active.len());
        for &a in &active {
            if intervals[a].end < start {
                if let Location::Register(r) = locations[a] {
                    free.push(r);
                }
            } else {
                kept.push(a);
            }
        }
        active = kept;

        if let Some(r) = free.pop() {
            locations[v] = Location::Register(r);
            registers_used = registers_used.max(r + 1);
            active.push(v);
        } else {
            let furthest = active.iter().max_by_key(|&&a| intervals[a].end).copied();
            match furthest {
                Some(a) if intervals[a].end > intervals[v].end => {
                    if let Location::Register(r) = locations[a] {
                        locations[v] = Location::Register(r);
                        registers_used = registers_used.max(r + 1);
                    }
                    locations[a] = Location::Spill(spill_count);
                    spill_count += 1;
                    active.retain(|&x| x != a);
                    active.push(v);
                }
                _ => {
                    locations[v] = Location::Spill(spill_count);
                    spill_count += 1;
                }
            }
        }
    }

    Allocation {
        locations,
        registers_used,
        spill_count,
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
        Inst::Call { args, .. } => args.iter().for_each(|a| f(*a)),
        Inst::Store { address, value } => {
            f(*address);
            f(*value);
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

    #[test]
    fn loop_carried_values_have_longer_intervals() {
        let func = loop_function();
        let live = Liveness::analyze(&func);
        let intervals = live_intervals(&func, &live);
        let span = |i: usize| intervals[i].end.saturating_sub(intervals[i].start);
        assert!(span(2) > span(5));
        assert!(span(1) > span(5));
    }

    #[test]
    fn linear_scan_assigns_and_spills() {
        let func = loop_function();
        let intervals = live_intervals(&func, &Liveness::analyze(&func));
        let roomy = allocate(&intervals, 8);
        assert_eq!(roomy.locations.len(), func.value_types.len());
        assert!(allocate(&intervals, 2).spill_count > 0);
        let none = allocate(&intervals, 0);
        assert_eq!(none.registers_used, 0);
        assert_eq!(none.spill_count as usize, func.value_types.len());
    }
}
