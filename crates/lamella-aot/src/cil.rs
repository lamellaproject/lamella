//! Lowering CIL method bodies to the middle IR by abstract interpretation.

use alloc::vec;
use alloc::vec::Vec;

use lamella_cil::{Instruction, MethodBodyImage, Opcode, Operand};
use lamella_ir::{BasicBlock, BinOp, BlockId, CmpOp, Function, Inst, MirType, Terminator, ValueId};

/// Why a method body could not be lowered to MIR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CilError {
    /// An opcode needed more operands than the evaluation stack held.
    StackUnderflow,
    /// The method body did not end in `ret` (control flow is not lowered yet).
    NoReturn,
    /// An opcode's decoded operand was not the shape the opcode requires.
    BadOperand,
    /// A CIL opcode this spike does not lower yet.
    Unsupported(Opcode),
}

/// Lowers a straight-line integer [`MethodBodyImage`] to a MIR [`Function`] by
/// abstract interpretation of the evaluation stack.
pub fn lower_method(body: &MethodBodyImage) -> Result<Function, CilError> {
    let (arg_count, local_count) = scan_slots(&body.code);
    let mut value_types: Vec<MirType> = Vec::new();
    let mut args: Vec<ValueId> = Vec::with_capacity(arg_count);
    for _ in 0..arg_count {
        args.push(new_value(&mut value_types, MirType::I32));
    }
    let mut locals: Vec<Option<ValueId>> = Vec::new();
    locals.resize(local_count, None);
    let mut stack: Vec<ValueId> = Vec::new();
    let mut insts: Vec<(ValueId, Inst)> = Vec::new();
    let mut terminator: Option<Terminator> = None;

    for instruction in body.code.iter() {
        match instruction.opcode {
            Opcode::Nop => {}
            Opcode::Ldarg0 => push_arg(&args, &mut stack, 0)?,
            Opcode::Ldarg1 => push_arg(&args, &mut stack, 1)?,
            Opcode::Ldarg2 => push_arg(&args, &mut stack, 2)?,
            Opcode::Ldarg3 => push_arg(&args, &mut stack, 3)?,
            Opcode::LdargS | Opcode::Ldarg => {
                let Operand::Variable(n) = &instruction.operand else {
                    return Err(CilError::BadOperand);
                };
                push_arg(&args, &mut stack, *n as usize)?;
            }
            Opcode::Ldloc0 => push_local(&mut value_types, &mut locals, &mut stack, &mut insts, 0)?,
            Opcode::Ldloc1 => push_local(&mut value_types, &mut locals, &mut stack, &mut insts, 1)?,
            Opcode::Ldloc2 => push_local(&mut value_types, &mut locals, &mut stack, &mut insts, 2)?,
            Opcode::Ldloc3 => push_local(&mut value_types, &mut locals, &mut stack, &mut insts, 3)?,
            Opcode::LdlocS | Opcode::Ldloc => {
                let Operand::Variable(n) = &instruction.operand else {
                    return Err(CilError::BadOperand);
                };
                push_local(
                    &mut value_types,
                    &mut locals,
                    &mut stack,
                    &mut insts,
                    *n as usize,
                )?;
            }
            Opcode::Stloc0 => store_local(&mut locals, &mut stack, 0)?,
            Opcode::Stloc1 => store_local(&mut locals, &mut stack, 1)?,
            Opcode::Stloc2 => store_local(&mut locals, &mut stack, 2)?,
            Opcode::Stloc3 => store_local(&mut locals, &mut stack, 3)?,
            Opcode::StlocS | Opcode::Stloc => {
                let Operand::Variable(n) = &instruction.operand else {
                    return Err(CilError::BadOperand);
                };
                store_local(&mut locals, &mut stack, *n as usize)?;
            }
            Opcode::LdcI4M1 => push_const(&mut value_types, &mut stack, &mut insts, -1),
            Opcode::LdcI40 => push_const(&mut value_types, &mut stack, &mut insts, 0),
            Opcode::LdcI41 => push_const(&mut value_types, &mut stack, &mut insts, 1),
            Opcode::LdcI42 => push_const(&mut value_types, &mut stack, &mut insts, 2),
            Opcode::LdcI43 => push_const(&mut value_types, &mut stack, &mut insts, 3),
            Opcode::LdcI44 => push_const(&mut value_types, &mut stack, &mut insts, 4),
            Opcode::LdcI45 => push_const(&mut value_types, &mut stack, &mut insts, 5),
            Opcode::LdcI46 => push_const(&mut value_types, &mut stack, &mut insts, 6),
            Opcode::LdcI47 => push_const(&mut value_types, &mut stack, &mut insts, 7),
            Opcode::LdcI48 => push_const(&mut value_types, &mut stack, &mut insts, 8),
            Opcode::LdcI4S => {
                let Operand::Int8(v) = &instruction.operand else {
                    return Err(CilError::BadOperand);
                };
                push_const(&mut value_types, &mut stack, &mut insts, i64::from(*v));
            }
            Opcode::LdcI4 => {
                let Operand::Int32(v) = &instruction.operand else {
                    return Err(CilError::BadOperand);
                };
                push_const(&mut value_types, &mut stack, &mut insts, i64::from(*v));
            }
            Opcode::Add => binary(&mut value_types, &mut stack, &mut insts, BinOp::Add)?,
            Opcode::Sub => binary(&mut value_types, &mut stack, &mut insts, BinOp::Sub)?,
            Opcode::Mul => binary(&mut value_types, &mut stack, &mut insts, BinOp::Mul)?,
            Opcode::And => binary(&mut value_types, &mut stack, &mut insts, BinOp::And)?,
            Opcode::Or => binary(&mut value_types, &mut stack, &mut insts, BinOp::Or)?,
            Opcode::Xor => binary(&mut value_types, &mut stack, &mut insts, BinOp::Xor)?,
            Opcode::Shl => binary(&mut value_types, &mut stack, &mut insts, BinOp::Shl)?,
            Opcode::Shr => binary(&mut value_types, &mut stack, &mut insts, BinOp::ShrSigned)?,
            Opcode::ShrUn => binary(&mut value_types, &mut stack, &mut insts, BinOp::ShrUnsigned)?,
            Opcode::Ceq => compare(&mut value_types, &mut stack, &mut insts, CmpOp::Eq)?,
            Opcode::Cgt => compare(&mut value_types, &mut stack, &mut insts, CmpOp::SignedGt)?,
            Opcode::CgtUn => compare(&mut value_types, &mut stack, &mut insts, CmpOp::UnsignedGt)?,
            Opcode::Clt => compare(&mut value_types, &mut stack, &mut insts, CmpOp::SignedLt)?,
            Opcode::CltUn => compare(&mut value_types, &mut stack, &mut insts, CmpOp::UnsignedLt)?,
            Opcode::Pop => {
                stack.pop().ok_or(CilError::StackUnderflow)?;
            }
            Opcode::Dup => {
                let top = *stack.last().ok_or(CilError::StackUnderflow)?;
                stack.push(top);
            }
            Opcode::Ret => {
                terminator = Some(Terminator::Return(stack.pop()));
                break;
            }
            other => return Err(CilError::Unsupported(other)),
        }
    }

    let terminator = terminator.ok_or(CilError::NoReturn)?;
    let ret = match &terminator {
        Terminator::Return(Some(v)) => value_types.get(v.index()).copied(),
        _ => None,
    };

    Ok(Function {
        params: (0..arg_count).map(|_| MirType::I32).collect(),
        ret,
        value_types,
        entry: BlockId(0),
        blocks: vec![BasicBlock {
            params: args,
            insts,
            terminator: Some(terminator),
        }],
    })
}

/// Defines a fresh MIR value of `ty` and returns its id.
fn new_value(value_types: &mut Vec<MirType>, ty: MirType) -> ValueId {
    let id = ValueId(value_types.len() as u32);
    value_types.push(ty);
    id
}

/// Pushes an integer constant: a new value defined by a `ConstInt`.
fn push_const(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    value: i64,
) {
    let result = new_value(value_types, MirType::I32);
    insts.push((
        result,
        Inst::ConstInt {
            ty: MirType::I32,
            value,
        },
    ));
    stack.push(result);
}

/// Pops two operands (CIL order: the top is the right operand) and pushes a
/// binary operation over them.
fn binary(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    op: BinOp,
) -> Result<(), CilError> {
    let rhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let lhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let result = new_value(value_types, MirType::I32);
    insts.push((result, Inst::Binary { op, lhs, rhs }));
    stack.push(result);
    Ok(())
}

/// Pops two operands and pushes a comparison yielding 0 or 1.
fn compare(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    op: CmpOp,
) -> Result<(), CilError> {
    let rhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let lhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let result = new_value(value_types, MirType::I32);
    insts.push((result, Inst::Compare { op, lhs, rhs }));
    stack.push(result);
    Ok(())
}

/// Scans a method body for the highest argument and local slot it references, to
/// size the entry parameters and the locals table.
fn scan_slots(code: &[Instruction]) -> (usize, usize) {
    let mut args = 0usize;
    let mut locals = 0usize;
    for instruction in code {
        match instruction.opcode {
            Opcode::Ldarg0 => args = args.max(1),
            Opcode::Ldarg1 => args = args.max(2),
            Opcode::Ldarg2 => args = args.max(3),
            Opcode::Ldarg3 => args = args.max(4),
            Opcode::LdargS | Opcode::Ldarg | Opcode::StargS | Opcode::Starg => {
                if let Operand::Variable(n) = &instruction.operand {
                    args = args.max(*n as usize + 1);
                }
            }
            Opcode::Ldloc0 | Opcode::Stloc0 => locals = locals.max(1),
            Opcode::Ldloc1 | Opcode::Stloc1 => locals = locals.max(2),
            Opcode::Ldloc2 | Opcode::Stloc2 => locals = locals.max(3),
            Opcode::Ldloc3 | Opcode::Stloc3 => locals = locals.max(4),
            Opcode::LdlocS | Opcode::Ldloc | Opcode::StlocS | Opcode::Stloc => {
                if let Operand::Variable(n) = &instruction.operand {
                    locals = locals.max(*n as usize + 1);
                }
            }
            _ => {}
        }
    }
    (args, locals)
}

/// Pushes the value currently in argument slot `index`.
fn push_arg(args: &[ValueId], stack: &mut Vec<ValueId>, index: usize) -> Result<(), CilError> {
    let value = *args.get(index).ok_or(CilError::BadOperand)?;
    stack.push(value);
    Ok(())
}

/// Pushes the value in local slot `index`, materializing a zero for a local read
/// before it is stored (CIL zero-initializes locals).
fn push_local(
    value_types: &mut Vec<MirType>,
    locals: &mut [Option<ValueId>],
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    index: usize,
) -> Result<(), CilError> {
    let slot = locals.get_mut(index).ok_or(CilError::BadOperand)?;
    let value = match *slot {
        Some(value) => value,
        None => {
            let zero = new_value(value_types, MirType::I32);
            insts.push((
                zero,
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: 0,
                },
            ));
            *slot = Some(zero);
            zero
        }
    };
    stack.push(value);
    Ok(())
}

/// Stores the stack top into local slot `index`.
fn store_local(
    locals: &mut [Option<ValueId>],
    stack: &mut Vec<ValueId>,
    index: usize,
) -> Result<(), CilError> {
    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
    let slot = locals.get_mut(index).ok_or(CilError::BadOperand)?;
    *slot = Some(value);
    Ok(())
}

/// Control-flow graph analysis over a CIL instruction stream: basic-block
/// discovery and predecessors.
#[allow(dead_code)]
mod control_flow {
    use alloc::vec;
    use alloc::vec::Vec;

    use alloc::collections::BTreeSet;
    use lamella_cil::{Instruction, Opcode, Operand};

    /// Whether an opcode is a branch and, if so, whether it also falls through.
    #[derive(Clone, Copy)]
    pub enum BranchKind {
        /// `br`/`leave`: control always leaves to the target.
        Unconditional,
        /// `brtrue`/`beq`/...: control goes to the target or falls through.
        Conditional,
    }

    /// Classifies an opcode's control flow; `None` if it is not a branch.
    pub fn branch_kind(op: Opcode) -> Option<BranchKind> {
        match op {
            Opcode::Br | Opcode::BrS | Opcode::Leave | Opcode::LeaveS => {
                Some(BranchKind::Unconditional)
            }
            Opcode::BrtrueS
            | Opcode::Brtrue
            | Opcode::BrfalseS
            | Opcode::Brfalse
            | Opcode::BeqS
            | Opcode::Beq
            | Opcode::BgeS
            | Opcode::Bge
            | Opcode::BgtS
            | Opcode::Bgt
            | Opcode::BleS
            | Opcode::Ble
            | Opcode::BltS
            | Opcode::Blt
            | Opcode::BneUnS
            | Opcode::BneUn
            | Opcode::BgeUnS
            | Opcode::BgeUn
            | Opcode::BgtUnS
            | Opcode::BgtUn
            | Opcode::BleUnS
            | Opcode::BleUn
            | Opcode::BltUnS
            | Opcode::BltUn => Some(BranchKind::Conditional),
            _ => None,
        }
    }

    /// Whether an opcode ends control flow with no fall-through.
    fn is_return(op: Opcode) -> bool {
        matches!(op, Opcode::Ret | Opcode::Throw | Opcode::Rethrow)
    }

    /// The instruction indices control can reach from the terminator at `index`.
    pub fn successors(inst: &Instruction, index: usize) -> Vec<usize> {
        let mut out = Vec::new();
        match branch_kind(inst.opcode) {
            Some(BranchKind::Unconditional) => {
                if let Operand::Target(t) = &inst.operand {
                    out.push(*t as usize);
                }
            }
            Some(BranchKind::Conditional) => {
                if let Operand::Target(t) = &inst.operand {
                    out.push(*t as usize);
                }
                out.push(index + 1);
            }
            None => {
                if !is_return(inst.opcode) {
                    out.push(index + 1);
                }
            }
        }
        out
    }

    /// Partitions a method's CIL into basic blocks, as `[start, end)` index ranges.
    /// Leaders are instruction 0, every branch target, and the instruction after a
    /// branch or a return.
    pub fn discover_blocks(code: &[Instruction]) -> Vec<(usize, usize)> {
        let mut leaders: BTreeSet<usize> = BTreeSet::new();
        leaders.insert(0);
        for (i, inst) in code.iter().enumerate() {
            if branch_kind(inst.opcode).is_some() {
                if let Operand::Target(t) = &inst.operand {
                    leaders.insert(*t as usize);
                }
                leaders.insert(i + 1);
            } else if is_return(inst.opcode) {
                leaders.insert(i + 1);
            }
        }
        let starts: Vec<usize> = leaders.into_iter().filter(|&l| l < code.len()).collect();
        starts
            .iter()
            .enumerate()
            .map(|(idx, &start)| (start, starts.get(idx + 1).copied().unwrap_or(code.len())))
            .collect()
    }

    /// The predecessor block indices of each block.
    pub fn predecessors(code: &[Instruction], blocks: &[(usize, usize)]) -> Vec<Vec<usize>> {
        let block_of = |instr: usize| blocks.iter().position(|&(s, e)| instr >= s && instr < e);
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); blocks.len()];
        for (b, &(_, end)) in blocks.iter().enumerate() {
            let Some(last) = end.checked_sub(1) else {
                continue;
            };
            let Some(inst) = code.get(last) else { continue };
            for succ in successors(inst, last) {
                if let Some(target) = block_of(succ) {
                    if !preds[target].contains(&b) {
                        preds[target].push(b);
                    }
                }
            }
        }
        preds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ldc.i4.s 40 ; ldc.i4.s 2 ; add ; ret`, the CIL of `fn() -> i32 { 40 + 2 }`.
    fn forty_plus_two() -> MethodBodyImage {
        MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdcI4S, Operand::Int8(40)),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(2)),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        }
    }

    #[test]
    fn lowers_ldc_add_ret_to_a_returning_function() {
        let func = lower_method(&forty_plus_two()).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        assert_eq!(func.value_types.len(), 3);
        assert_eq!(func.ret, Some(MirType::I32));
    }

    #[test]
    fn lowers_arguments_and_locals() {
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: true,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        assert_eq!(func.params.len(), 2);
        assert_eq!(func.ret, Some(MirType::I32));
    }

    #[test]
    fn discovers_an_if_else_cfg() {
        let code = [
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::LdcI40),
            Instruction::new(Opcode::BgtS, Operand::Target(5)),
            Instruction::simple(Opcode::LdcI42),
            Instruction::simple(Opcode::Ret),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Ret),
        ];
        let blocks = control_flow::discover_blocks(&code);
        assert_eq!(blocks, vec![(0, 3), (3, 5), (5, 7)]);
        let preds = control_flow::predecessors(&code, &blocks);
        assert!(preds[0].is_empty());
        assert_eq!(preds[1], vec![0]);
        assert_eq!(preds[2], vec![0]);
    }

    #[test]
    fn rejects_a_body_with_no_return() {
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![Instruction::simple(Opcode::Nop)].into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        assert_eq!(lower_method(&body), Err(CilError::NoReturn));
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn ldc_add_ret_lowers_through_to_arm32() {
        let func = lower_method(&forty_plus_two()).unwrap();
        let bytes = crate::arm32::lower(&func).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], &[0x70, 0x47]);
    }
}
