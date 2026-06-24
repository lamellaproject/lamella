//! The RV32IM (RISC-V) target code generator.

use alloc::vec::Vec;

use lamella_asm_riscv32::{BranchCond, Encoder, Label, Reg};
use lamella_ir::{BinOp, CmpOp, Function, Inst, MirType, Terminator, ValueId};

/// Why a function could not be lowered to RV32IM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowerError {
    /// The function did not pass IR verification.
    NotWellFormed,
    /// An instruction or shape the initial code does not lower yet.
    Unsupported,
    /// More live values than the trivial register map holds.
    TooManyValues,
    /// A control-flow shape the initial code does not handle.
    ControlFlowUnsupported,
    /// The final image could not be assembled (an out-of-range branch).
    CodeTooLarge,
}

/// The callee-saved registers the trivial value map hands out, in order: s0-s11 (x8, x9, x18-x27).
/// Callee-saved means a value survives a `call` without spilling -- the prologue saves each one the
/// function uses and the epilogue restores it. `a0`-`a7` carry call arguments and the return value;
/// `t6` is array-addressing scratch ([`scratch`]); `ra`/`sp`/`x0` are reserved by the ABI.
fn allocatable() -> [Reg; 12] {
    let r = |n: u8| Reg::new(n).unwrap_or(Reg::ZERO);
    [
        r(8),
        r(9),
        r(18),
        r(19),
        r(20),
        r(21),
        r(22),
        r(23),
        r(24),
        r(25),
        r(26),
        r(27),
    ]
}

/// The argument/return register `a<index>` (x10-x17), or `None` past the eighth (stack-passed
/// arguments are not lowered by the initial code yet).
fn arg_reg(index: usize) -> Option<Reg> {
    (index < 8).then(|| Reg::new(10 + index as u8).unwrap_or(Reg::ZERO))
}

/// `t6` (x31), reserved out of the allocatable pool as scratch for array addressing -- the length
/// load, the index scaling, and the element address -- so it never aliases an allocated value.
fn scratch() -> Reg {
    Reg::new(31).unwrap_or(Reg::ZERO)
}

/// Lowers a single [`Function`] to RV32IM machine code -- a one-function module.
pub fn lower(func: &Function) -> Result<Vec<u8>, LowerError> {
    lower_module(core::slice::from_ref(func))
}

/// Lowers a module of [`Function`]s to RV32IM machine code with the calling convention: each
/// function gets an entry label, a `Call` jumps to it (`jal`), arguments pass in a0-a7 and the
/// result returns in a0. Module order fixes call indices -- function 0 is the entry. Values live
/// in callee-saved registers so they survive a call; each function saves the ones it uses.
pub fn lower_module(funcs: &[Function]) -> Result<Vec<u8>, LowerError> {
    for func in funcs {
        if lamella_ir::verify(func).is_err() {
            return Err(LowerError::NotWellFormed);
        }
    }
    let mut enc = Encoder::new();
    let func_labels: Vec<Label> = (0..funcs.len()).map(|_| enc.new_label()).collect();
    for (index, func) in funcs.iter().enumerate() {
        enc.bind_label(func_labels[index]);
        lower_function(&mut enc, func, &func_labels)?;
    }
    enc.finish()
        .map(|assembled| assembled.bytes)
        .map_err(|_| LowerError::CodeTooLarge)
}

/// Lowers one function into `enc`: a prologue that allocates a frame and saves the callee-saved
/// registers it uses (plus `ra` if it calls), the incoming arguments moved from a0-a7 into the
/// entry block's parameters, the block bodies, and -- at each return -- a value moved to a0 then
/// the saved registers restored and `ret`.
fn lower_function(
    enc: &mut Encoder,
    func: &Function,
    func_labels: &[Label],
) -> Result<(), LowerError> {
    let pool = allocatable();
    let value_count = func.value_types.len();
    if value_count > pool.len() {
        return Err(LowerError::TooManyValues);
    }
    let reg = |v: ValueId| pool[v.index()];
    let used = &pool[..value_count];
    let has_calls = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| matches!(i, Inst::Call { .. })));
    let saved = value_count + has_calls as usize;
    let frame = (saved * 4).div_ceil(16) * 16;

    if frame > 0 {
        enc.addi(Reg::SP, Reg::SP, -(frame as i32));
    }
    let mut offset = 0i32;
    if has_calls {
        enc.sw(Reg::RA, Reg::SP, offset);
        offset += 4;
    }
    for &r in used {
        enc.sw(r, Reg::SP, offset);
        offset += 4;
    }
    let entry = &func.blocks[func.entry.index()];
    for (i, &param) in entry.params.iter().enumerate() {
        let arg = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
        if reg(param) != arg {
            enc.mv(reg(param), arg);
        }
    }

    let block_labels: Vec<Label> = (0..func.blocks.len()).map(|_| enc.new_label()).collect();
    if func.entry != lamella_ir::BlockId(0) {
        enc.j(block_labels[func.entry.index()]);
    }

    for (index, block) in func.blocks.iter().enumerate() {
        enc.bind_label(block_labels[index]);

        let fused = match &block.terminator {
            Some(Terminator::Branch { cond, .. }) => match block.insts.last() {
                Some((r, Inst::Compare { op, lhs, rhs })) if r == cond => Some((*op, *lhs, *rhs)),
                _ => None,
            },
            _ => None,
        };
        let body = if fused.is_some() {
            &block.insts[..block.insts.len() - 1]
        } else {
            &block.insts[..]
        };

        for (result, inst) in body {
            lower_inst(enc, &reg, &func.value_types, func_labels, *result, inst)?;
        }

        match &block.terminator {
            Some(Terminator::Return(value)) => {
                if let Some(v) = value {
                    enc.mv(Reg::A0, reg(*v));
                }
                let mut offset = 0i32;
                if has_calls {
                    enc.lw(Reg::RA, Reg::SP, offset);
                    offset += 4;
                }
                for &r in used {
                    enc.lw(r, Reg::SP, offset);
                    offset += 4;
                }
                if frame > 0 {
                    enc.addi(Reg::SP, Reg::SP, frame as i32);
                }
                enc.ret();
            }
            Some(Terminator::Jump { target, args }) => {
                let params = &func
                    .block(*target)
                    .ok_or(LowerError::ControlFlowUnsupported)?
                    .params;
                if args.len() != params.len() {
                    return Err(LowerError::ControlFlowUnsupported);
                }
                for (p, a) in params.iter().zip(args) {
                    if reg(*p) != reg(*a) {
                        enc.mv(reg(*p), reg(*a));
                    }
                }
                enc.j(block_labels[target.index()]);
            }
            Some(Terminator::Branch {
                cond,
                if_true,
                true_args,
                if_false,
                false_args,
            }) => {
                if !true_args.is_empty() || !false_args.is_empty() {
                    return Err(LowerError::ControlFlowUnsupported);
                }
                let true_label = block_labels[if_true.index()];
                let false_label = block_labels[if_false.index()];
                match fused {
                    Some((op, lhs, rhs)) => {
                        let (cond, a, b) = branch_for(op, reg(lhs), reg(rhs));
                        enc.branch(cond, a, b, true_label);
                    }
                    None => enc.branch(BranchCond::Ne, reg(*cond), Reg::ZERO, true_label),
                }
                enc.j(false_label);
            }
            Some(Terminator::Unreachable) => enc.ebreak(),
            None => return Err(LowerError::ControlFlowUnsupported),
        }
    }
    Ok(())
}

/// Lowers one value-defining instruction into its assigned register.
fn lower_inst(
    enc: &mut Encoder,
    reg: &impl Fn(ValueId) -> Reg,
    value_types: &[MirType],
    func_labels: &[Label],
    result: ValueId,
    inst: &Inst,
) -> Result<(), LowerError> {
    match inst {
        Inst::ConstInt { value, .. } => enc.li(reg(result), *value as i32),
        Inst::Call { callee, args } => {
            for (i, &arg) in args.iter().enumerate() {
                let target = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                if reg(arg) != target {
                    enc.mv(target, reg(arg));
                }
            }
            let label = *func_labels
                .get(*callee as usize)
                .ok_or(LowerError::ControlFlowUnsupported)?;
            enc.jal(Reg::RA, label);
            if reg(result) != Reg::A0 {
                enc.mv(reg(result), Reg::A0);
            }
        }
        Inst::Load { address } => enc.lw(reg(result), reg(*address), 0),
        Inst::Store { address, value } => enc.sw(reg(*value), reg(*address), 0),
        Inst::FieldLoad { base, offset } => {
            if !is_pointer(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            enc.lw(reg(result), reg(*base), field_offset(*offset)?);
        }
        Inst::FieldStore {
            base,
            offset,
            value,
        } => {
            if !is_pointer(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            enc.sw(reg(*value), reg(*base), field_offset(*offset)?);
        }
        Inst::FieldAddr { base, offset } => {
            if !is_pointer(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            enc.addi(reg(result), reg(*base), field_offset(*offset)?);
        }
        Inst::ArrayLoad {
            array,
            index,
            element_size,
            signed: _,
        } => {
            if *element_size != 4 {
                return Err(LowerError::Unsupported);
            }
            emit_element_address(enc, reg(*array), reg(*index));
            enc.lw(reg(result), scratch(), 4);
        }
        Inst::ArrayStore {
            array,
            index,
            value,
            element_size,
        } => {
            if *element_size != 4 {
                return Err(LowerError::Unsupported);
            }
            emit_element_address(enc, reg(*array), reg(*index));
            enc.sw(reg(*value), scratch(), 4);
        }
        Inst::Binary { op, lhs, rhs } => {
            let (d, a, b) = (reg(result), reg(*lhs), reg(*rhs));
            match op {
                BinOp::Add => enc.add(d, a, b),
                BinOp::Sub => enc.sub(d, a, b),
                BinOp::And => enc.and(d, a, b),
                BinOp::Or => enc.or(d, a, b),
                BinOp::Xor => enc.xor(d, a, b),
                BinOp::Mul => enc.mul(d, a, b),
                BinOp::DivSigned => enc.div(d, a, b),
                BinOp::DivUnsigned => enc.divu(d, a, b),
                BinOp::RemSigned => enc.rem(d, a, b),
                BinOp::RemUnsigned => enc.remu(d, a, b),
                BinOp::Shl => enc.sll(d, a, b),
                BinOp::ShrSigned => enc.sra(d, a, b),
                BinOp::ShrUnsigned => enc.srl(d, a, b),
            }
        }
        Inst::Compare { op, lhs, rhs } => {
            materialize_compare(enc, reg(result), reg(*lhs), reg(*rhs), *op);
        }
        _ => return Err(LowerError::Unsupported),
    }
    Ok(())
}

/// The RISC-V branch condition and operand order so that `b<cond> a, b` is taken exactly when
/// `lhs <op> rhs` holds (the IR branch goes to `if_true` when the comparison is true).
fn branch_for(op: CmpOp, lhs: Reg, rhs: Reg) -> (BranchCond, Reg, Reg) {
    match op {
        CmpOp::Eq => (BranchCond::Eq, lhs, rhs),
        CmpOp::Ne => (BranchCond::Ne, lhs, rhs),
        CmpOp::SignedLt => (BranchCond::Lt, lhs, rhs),
        CmpOp::SignedGe => (BranchCond::Ge, lhs, rhs),
        CmpOp::SignedGt => (BranchCond::Lt, rhs, lhs),
        CmpOp::SignedLe => (BranchCond::Ge, rhs, lhs),
        CmpOp::UnsignedLt => (BranchCond::LtU, lhs, rhs),
        CmpOp::UnsignedGe => (BranchCond::GeU, lhs, rhs),
        CmpOp::UnsignedGt => (BranchCond::LtU, rhs, lhs),
        CmpOp::UnsignedLe => (BranchCond::GeU, rhs, lhs),
    }
}

/// Materializes `dest = (lhs <op> rhs) ? 1 : 0` from the `slt`/`sltu` set-less-than primitives.
fn materialize_compare(enc: &mut Encoder, dest: Reg, lhs: Reg, rhs: Reg, op: CmpOp) {
    match op {
        CmpOp::SignedLt => enc.slt(dest, lhs, rhs),
        CmpOp::SignedGt => enc.slt(dest, rhs, lhs),
        CmpOp::UnsignedLt => enc.sltu(dest, lhs, rhs),
        CmpOp::UnsignedGt => enc.sltu(dest, rhs, lhs),
        CmpOp::SignedGe => {
            enc.slt(dest, lhs, rhs);
            enc.xori(dest, dest, 1);
        }
        CmpOp::SignedLe => {
            enc.slt(dest, rhs, lhs);
            enc.xori(dest, dest, 1);
        }
        CmpOp::UnsignedGe => {
            enc.sltu(dest, lhs, rhs);
            enc.xori(dest, dest, 1);
        }
        CmpOp::UnsignedLe => {
            enc.sltu(dest, rhs, lhs);
            enc.xori(dest, dest, 1);
        }
        CmpOp::Eq => {
            enc.sub(dest, lhs, rhs);
            enc.sltiu(dest, dest, 1);
        }
        CmpOp::Ne => {
            enc.sub(dest, lhs, rhs);
            enc.sltu(dest, Reg::ZERO, dest);
        }
    }
}

/// Whether `value` is a pointer -- a heap ObjectRef or a managed pointer (`this` / `&field`) --
/// the only field base the register-only intiial code can dereference. A value type spans several words
/// and needs the stack slots the initial code does not have yet.
fn is_pointer(value_types: &[MirType], value: ValueId) -> bool {
    matches!(
        value_types.get(value.index()),
        Some(MirType::ObjectRef | MirType::ManagedPtr)
    )
}

/// Converts a field/element byte offset to the signed 12-bit immediate RISC-V `lw`/`sw`/`addi`
/// take, or `Unsupported` if it does not fit. The initial code does not materialize a large offset into
/// the base register yet -- every struct/array layout it lowers is well within the 2 KiB reach.
fn field_offset(offset: u32) -> Result<i32, LowerError> {
    if offset <= 2047 {
        Ok(offset as i32)
    } else {
        Err(LowerError::Unsupported)
    }
}

/// Emits the bounds check and element-address computation for word-array access: traps (`ebreak`)
/// unless `index < length` (the length at `[array+0]`, compared UNSIGNED so a negative index -- a
/// huge unsigned value -- traps too, matching IndexOutOfRangeException's effect), then leaves
/// `array + index*4` in [`scratch`] so the caller's `lw`/`sw` at offset 4 hits the element past the
/// length word.
fn emit_element_address(enc: &mut Encoder, array: Reg, index: Reg) {
    let s = scratch();
    enc.lw(s, array, 0);
    let ok = enc.new_label();
    enc.branch(BranchCond::LtU, index, s, ok);
    enc.ebreak();
    enc.bind_label(ok);
    enc.slli(s, index, 2);
    enc.add(s, array, s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_ir::{BasicBlock, BlockId};

    /// A reference-field round-trip: store 40 at `[base+4]`, load it back, add 2, store the 42
    /// through a computed field address, and load it -- exercising FieldStore/FieldLoad/FieldAddr
    /// and raw Store/Load over a pointer base. `base` is a ConstInt typed as an ObjectRef.
    fn field_function() -> Function {
        let i32t = MirType::I32;
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![
                MirType::ObjectRef,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                MirType::ManagedPtr,
                i32t,
                i32t,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::ObjectRef,
                            value: 0x8010_0000,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 40,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::FieldStore {
                            base: ValueId(0),
                            offset: 4,
                            value: ValueId(1),
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 4,
                        },
                    ),
                    (ValueId(4), Inst::ConstInt { ty: i32t, value: 2 }),
                    (
                        ValueId(5),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(3),
                            rhs: ValueId(4),
                        },
                    ),
                    (
                        ValueId(6),
                        Inst::FieldAddr {
                            base: ValueId(0),
                            offset: 8,
                        },
                    ),
                    (
                        ValueId(7),
                        Inst::Store {
                            address: ValueId(6),
                            value: ValueId(5),
                        },
                    ),
                    (
                        ValueId(8),
                        Inst::Load {
                            address: ValueId(6),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(8)))),
            }],
        }
    }

    #[test]
    fn lowers_reference_field_access() {
        let func = field_function();
        assert!(lamella_ir::verify(&func).is_ok());
        let code = lower(&func).expect("field access lowers to RV32IM");
        assert!(!code.is_empty());
    }

    #[test]
    fn classifies_pointer_bases() {
        let types = [
            MirType::ObjectRef,
            MirType::ManagedPtr,
            MirType::I32,
            MirType::ValueType {
                handle: lamella_ir::TypeHandle(0),
                size: 8,
            },
        ];
        assert!(is_pointer(&types, ValueId(0)));
        assert!(is_pointer(&types, ValueId(1)));
        assert!(!is_pointer(&types, ValueId(2)));
        assert!(!is_pointer(&types, ValueId(3)));
    }

    /// A two-element `int[]` hand-laid in RAM: set the length, store a[0]=20 and a[1]=22, load
    /// them back and sum -> 42. Exercises ArrayStore/ArrayLoad (with the bounds check) over a
    /// pointer base; the length word at offset 0 is set with a FieldStore.
    fn array_function() -> Function {
        let i32t = MirType::I32;
        let cint = |v: i64| Inst::ConstInt { ty: i32t, value: v };
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![
                MirType::ObjectRef,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::ObjectRef,
                            value: 0x8010_0000,
                        },
                    ),
                    (ValueId(1), cint(2)),
                    (
                        ValueId(2),
                        Inst::FieldStore {
                            base: ValueId(0),
                            offset: 0,
                            value: ValueId(1),
                        },
                    ),
                    (ValueId(3), cint(20)),
                    (ValueId(4), cint(0)),
                    (
                        ValueId(5),
                        Inst::ArrayStore {
                            array: ValueId(0),
                            index: ValueId(4),
                            value: ValueId(3),
                            element_size: 4,
                        },
                    ),
                    (ValueId(6), cint(22)),
                    (ValueId(7), cint(1)),
                    (
                        ValueId(8),
                        Inst::ArrayStore {
                            array: ValueId(0),
                            index: ValueId(7),
                            value: ValueId(6),
                            element_size: 4,
                        },
                    ),
                    (
                        ValueId(9),
                        Inst::ArrayLoad {
                            array: ValueId(0),
                            index: ValueId(4),
                            element_size: 4,
                            signed: false,
                        },
                    ),
                    (
                        ValueId(10),
                        Inst::ArrayLoad {
                            array: ValueId(0),
                            index: ValueId(7),
                            element_size: 4,
                            signed: false,
                        },
                    ),
                    (
                        ValueId(11),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(9),
                            rhs: ValueId(10),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(11)))),
            }],
        }
    }

    #[test]
    fn lowers_word_array_access() {
        let func = array_function();
        assert!(lamella_ir::verify(&func).is_ok());
        let code = lower(&func).expect("word array access lowers to RV32IM");
        assert!(!code.is_empty());
    }

    #[test]
    fn rejects_sub_word_array_elements() {
        let mut func = array_function();
        if let Inst::ArrayStore { element_size, .. } = &mut func.blocks[0].insts[5].1 {
            *element_size = 1;
        }
        assert_eq!(lower(&func), Err(LowerError::Unsupported));
    }

    #[test]
    fn lowers_a_call() {
        let i32t = MirType::I32;
        let main = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 40,
                        },
                    ),
                    (ValueId(1), Inst::ConstInt { ty: i32t, value: 2 }),
                    (
                        ValueId(2),
                        Inst::Call {
                            callee: 1,
                            args: vec![ValueId(0), ValueId(1)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let add = Function {
            params: vec![i32t, i32t],
            ret: Some(i32t),
            value_types: vec![i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![(
                    ValueId(2),
                    Inst::Binary {
                        op: BinOp::Add,
                        lhs: ValueId(0),
                        rhs: ValueId(1),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let code = lower_module(&[main, add]).expect("a module with a call lowers");
        assert!(!code.is_empty());
    }
}
