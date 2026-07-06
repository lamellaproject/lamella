//! Generated MIR helper builders shared by the WASM, ARM, and RISC-V backends: the bodies that the
//! `StringConcat`, `IntToString`, and `StringEquals` marker instructions lower to. Each is an
//! ordinary verified [`Function`] of pure [`lamella_ir`] MIR (no target specifics) -- a string is the
//! array layout `[u32 unit_count][u16 units]`, so the helpers build/read their results with
//! `AllocArray` (element size 2), array loads/stores, `FieldLoad` of the count word, and the integer
//! Div/Rem. A backend rewrites the marker to a call to the appended helper and lowers it through its
//! usual path.

use alloc::vec;
use alloc::vec::Vec;

use lamella_ir::{BasicBlock, BinOp, BlockId, CmpOp, Function, Inst, MirType, Terminator, ValueId};

/// Rewrites each `StringConcat` to a call to a generated `__string_concat` helper appended to the
/// program, so string concatenation reuses the normal call + structuring path on every backend.
pub(crate) fn lower_string_concat(program: &mut Vec<Function>) {
    let has_concat = program
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.insts)
        .any(|(_, inst)| matches!(inst, Inst::StringConcat { .. }));
    if !has_concat {
        return;
    }
    let helper = program.len() as u32;
    for func in program.iter_mut() {
        for block in &mut func.blocks {
            for (_, inst) in &mut block.insts {
                if let Inst::StringConcat { lhs, rhs } = inst {
                    *inst = Inst::Call {
                        callee: helper,
                        args: vec![*lhs, *rhs],
                    };
                }
            }
        }
    }
    program.push(string_concat_mir());
}

/// Rewrites each `IntToString` to a call to a generated `__int_to_string` helper (appended after the
/// string helpers, if any), so integer formatting reuses the normal call + structuring path.
pub(crate) fn lower_int_to_string(program: &mut Vec<Function>) {
    let has = program
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.insts)
        .any(|(_, inst)| matches!(inst, Inst::IntToString { .. }));
    if !has {
        return;
    }
    let helper = program.len() as u32;
    for func in program.iter_mut() {
        for block in &mut func.blocks {
            for (_, inst) in &mut block.insts {
                if let Inst::IntToString { value } = inst {
                    *inst = Inst::Call {
                        callee: helper,
                        args: vec![*value],
                    };
                }
            }
        }
    }
    program.push(int_to_string_mir());
}

/// The `__int_to_string(v) -> ObjectRef` helper: formats a signed i32 as decimal. Branchlessly splits
/// `v` into magnitude + sign (`mask = v >> 31`; `mag = (v ^ mask) - mask`; `sign = mask & 1`), counts
/// the decimal digits (a `/10` loop), allocates a `[u32 unit_count][u16 units]` blob of digits + the
/// optional `-`, fills the digits back-to-front (`%10` + `/10`), then writes a leading `-` if negative.
/// Built as MIR so it reloops + lowers like any function (uses Div/Rem).
fn int_to_string_mir() -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let bin = |op, lhs, rhs| Inst::Binary { op, lhs, rhs };
    let cmp = |op, lhs, rhs| Inst::Compare { op, lhs, rhs };
    let put = |array, index, value| Inst::ArrayStore {
        array,
        index,
        value,
        element_size: 2,
    };
    let v = ValueId;
    let branch = |cond, t: u32, f: u32, ta: Vec<ValueId>, fa: Vec<ValueId>| Terminator::Branch {
        cond,
        if_true: BlockId(t),
        true_args: ta,
        if_false: BlockId(f),
        false_args: fa,
    };
    let jump = |t: u32, args: Vec<ValueId>| Terminator::Jump {
        target: BlockId(t),
        args,
    };
    Function {
        params: vec![objt],
        ret: Some(objt),
        value_types: vec![
            i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t,
            i32t, i32t, i32t, i32t, i32t, objt, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t,
            i32t, i32t, i32t,
        ],
        entry: BlockId(0),
        blocks: vec![
            BasicBlock {
                params: vec![v(0)],
                insts: vec![
                    (v(1), c(0)),
                    (v(2), c(1)),
                    (v(3), c(10)),
                    (v(4), c(i64::from(b'0'))),
                    (v(5), c(i64::from(b'-'))),
                    (v(6), c(31)),
                    (v(7), bin(BinOp::ShrSigned, v(0), v(6))),
                    (v(8), bin(BinOp::Xor, v(0), v(7))),
                    (v(9), bin(BinOp::Sub, v(8), v(7))),
                    (v(10), bin(BinOp::And, v(7), v(2))),
                    (v(11), bin(BinOp::DivUnsigned, v(9), v(3))),
                ],
                terminator: Some(jump(1, vec![v(11), v(2)])),
            },
            BasicBlock {
                params: vec![v(12), v(13)],
                insts: vec![(v(14), cmp(CmpOp::Ne, v(12), v(1)))],
                terminator: Some(branch(v(14), 2, 3, Vec::new(), Vec::new())),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(15), bin(BinOp::Add, v(13), v(2))),
                    (v(16), bin(BinOp::DivUnsigned, v(12), v(3))),
                ],
                terminator: Some(jump(1, vec![v(16), v(15)])),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(18), bin(BinOp::Add, v(13), v(10))),
                    (
                        v(19),
                        Inst::AllocArray {
                            handle: lamella_ir::TypeHandle(0),
                            length: v(18),
                            element_size: 2,
                        },
                    ),
                    (v(20), bin(BinOp::Sub, v(18), v(2))),
                ],
                terminator: Some(jump(4, vec![v(20), v(9)])),
            },
            BasicBlock {
                params: vec![v(21), v(22)],
                insts: vec![(v(23), cmp(CmpOp::SignedGe, v(21), v(10)))],
                terminator: Some(branch(v(23), 5, 6, Vec::new(), Vec::new())),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(24), bin(BinOp::RemUnsigned, v(22), v(3))),
                    (v(25), bin(BinOp::Add, v(24), v(4))),
                    (v(26), put(v(19), v(21), v(25))),
                    (v(27), bin(BinOp::DivUnsigned, v(22), v(3))),
                    (v(28), bin(BinOp::Sub, v(21), v(2))),
                ],
                terminator: Some(jump(4, vec![v(28), v(27)])),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(v(29), cmp(CmpOp::Ne, v(10), v(1)))],
                terminator: Some(branch(v(29), 7, 8, Vec::new(), Vec::new())),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(v(30), put(v(19), v(1), v(5)))],
                terminator: Some(jump(8, Vec::new())),
            },
            BasicBlock {
                params: Vec::new(),
                insts: Vec::new(),
                terminator: Some(Terminator::Return(Some(v(19)))),
            },
        ],
    }
}

/// The `__string_concat(a, b) -> ObjectRef` helper: allocates a `[u32 unit_count][u16 units]` blob of
/// `a.length + b.length` units (an `AllocArray` of element size 2, which stores the count word) and
/// copies a's then b's units in with two length-2 array-copy loops. (Non-null operands; null handling
/// is a follow-up.)
fn string_concat_mir() -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let ci = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let len = |s| Inst::FieldLoad { base: s, offset: 0 };
    let unit = |array, index| Inst::ArrayLoad {
        array,
        index,
        element_size: 2,
        signed: false,
    };
    let put = |array, index, value| Inst::ArrayStore {
        array,
        index,
        value,
        element_size: 2,
    };
    let add = |lhs, rhs| Inst::Binary {
        op: BinOp::Add,
        lhs,
        rhs,
    };
    let lt = |lhs, rhs| Inst::Compare {
        op: CmpOp::SignedLt,
        lhs,
        rhs,
    };
    let v = ValueId;
    Function {
        params: vec![objt, objt],
        ret: Some(objt),
        value_types: vec![
            objt, objt, i32t, i32t, i32t, objt, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t,
            i32t, i32t, i32t, i32t, i32t, i32t, i32t,
        ],
        entry: BlockId(0),
        blocks: vec![
            BasicBlock {
                params: vec![v(0), v(1)],
                insts: vec![
                    (v(2), len(v(0))),
                    (v(3), len(v(1))),
                    (v(4), add(v(2), v(3))),
                    (
                        v(5),
                        Inst::AllocArray {
                            handle: lamella_ir::TypeHandle(0),
                            length: v(4),
                            element_size: 2,
                        },
                    ),
                    (v(6), ci(0)),
                ],
                terminator: Some(Terminator::Jump {
                    target: BlockId(1),
                    args: vec![v(6)],
                }),
            },
            BasicBlock {
                params: vec![v(7)],
                insts: vec![(v(8), lt(v(7), v(2)))],
                terminator: Some(Terminator::Branch {
                    cond: v(8),
                    if_true: BlockId(2),
                    true_args: Vec::new(),
                    if_false: BlockId(3),
                    false_args: Vec::new(),
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(9), unit(v(0), v(7))),
                    (v(10), put(v(5), v(7), v(9))),
                    (v(11), ci(1)),
                    (v(12), add(v(7), v(11))),
                ],
                terminator: Some(Terminator::Jump {
                    target: BlockId(1),
                    args: vec![v(12)],
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(v(13), ci(0))],
                terminator: Some(Terminator::Jump {
                    target: BlockId(4),
                    args: vec![v(13)],
                }),
            },
            BasicBlock {
                params: vec![v(14)],
                insts: vec![(v(15), lt(v(14), v(3)))],
                terminator: Some(Terminator::Branch {
                    cond: v(15),
                    if_true: BlockId(5),
                    true_args: Vec::new(),
                    if_false: BlockId(6),
                    false_args: Vec::new(),
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (v(16), unit(v(1), v(14))),
                    (v(17), add(v(2), v(14))),
                    (v(18), put(v(5), v(17), v(16))),
                    (v(19), ci(1)),
                    (v(20), add(v(14), v(19))),
                ],
                terminator: Some(Terminator::Jump {
                    target: BlockId(4),
                    args: vec![v(20)],
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: Vec::new(),
                terminator: Some(Terminator::Return(Some(v(5)))),
            },
        ],
    }
}

/// Rewrites each `StringEquals` to a call to a generated `__string_eq` helper appended to the
/// program, so ordinal string comparison reuses the normal call + structuring path on every backend
/// rather than a bespoke inline expansion.
pub(crate) fn lower_string_equals(program: &mut Vec<Function>) {
    let has_string_equals = program
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.insts)
        .any(|(_, inst)| matches!(inst, Inst::StringEquals { .. }));
    if !has_string_equals {
        return;
    }
    let helper = program.len() as u32;
    for func in program.iter_mut() {
        for block in &mut func.blocks {
            for (_, inst) in &mut block.insts {
                if let Inst::StringEquals { lhs, rhs } = inst {
                    *inst = Inst::Call {
                        callee: helper,
                        args: vec![*lhs, *rhs],
                    };
                }
            }
        }
    }
    program.push(string_eq_mir());
}

/// The `__string_eq(a, b) -> i32` helper: ordinal UTF-16 string equality matching the runtime's
/// contract -- two nulls are equal, null and non-null are not, otherwise length-then-content. The
/// string blob is the array layout `[u32 length][u16 units]`, so the content loop reads units with a
/// length-2 array load. Built as MIR so it goes through the same verifier + structurer as any
/// function (the loop and branches relooped, the reference null-checks lowered as i32 compares).
fn string_eq_mir() -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let ci = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let cmp = |op, lhs, rhs| Inst::Compare { op, lhs, rhs };
    let unit = |array, index| Inst::ArrayLoad {
        array,
        index,
        element_size: 2,
        signed: false,
    };
    let branch = |cond, if_true: u32, if_false: u32| Terminator::Branch {
        cond,
        if_true: BlockId(if_true),
        true_args: Vec::new(),
        if_false: BlockId(if_false),
        false_args: Vec::new(),
    };
    let ret = |v| Some(Terminator::Return(Some(v)));
    Function {
        params: vec![objt, objt],
        ret: Some(i32t),
        value_types: vec![
            objt, objt, objt, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t, i32t,
            i32t, i32t, i32t, i32t, i32t, i32t, i32t,
        ],
        entry: BlockId(0),
        blocks: vec![
            BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![
                    (ValueId(2), Inst::ConstInt { ty: objt, value: 0 }),
                    (ValueId(3), cmp(CmpOp::Eq, ValueId(0), ValueId(2))),
                ],
                terminator: Some(branch(ValueId(3), 1, 2)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(4), cmp(CmpOp::Eq, ValueId(1), ValueId(2)))],
                terminator: ret(ValueId(4)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(5), cmp(CmpOp::Eq, ValueId(1), ValueId(2)))],
                terminator: Some(branch(ValueId(5), 3, 4)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(6), ci(0))],
                terminator: ret(ValueId(6)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(7),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 0,
                        },
                    ),
                    (
                        ValueId(8),
                        Inst::FieldLoad {
                            base: ValueId(1),
                            offset: 0,
                        },
                    ),
                    (ValueId(9), cmp(CmpOp::Ne, ValueId(7), ValueId(8))),
                ],
                terminator: Some(branch(ValueId(9), 5, 6)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(10), ci(0))],
                terminator: ret(ValueId(10)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(11), ci(0))],
                terminator: Some(Terminator::Jump {
                    target: BlockId(7),
                    args: vec![ValueId(11)],
                }),
            },
            BasicBlock {
                params: vec![ValueId(12)],
                insts: vec![(ValueId(13), cmp(CmpOp::UnsignedGe, ValueId(12), ValueId(7)))],
                terminator: Some(branch(ValueId(13), 8, 9)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(14), ci(1))],
                terminator: ret(ValueId(14)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (ValueId(15), unit(ValueId(0), ValueId(12))),
                    (ValueId(16), unit(ValueId(1), ValueId(12))),
                    (ValueId(17), cmp(CmpOp::Ne, ValueId(15), ValueId(16))),
                ],
                terminator: Some(branch(ValueId(17), 10, 11)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(18), ci(0))],
                terminator: ret(ValueId(18)),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (ValueId(19), ci(1)),
                    (
                        ValueId(20),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(12),
                            rhs: ValueId(19),
                        },
                    ),
                ],
                terminator: Some(Terminator::Jump {
                    target: BlockId(7),
                    args: vec![ValueId(20)],
                }),
            },
        ],
    }
}
