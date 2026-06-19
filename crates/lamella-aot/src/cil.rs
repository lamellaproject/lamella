//! Lowering CIL method bodies to the middle IR by abstract interpretation.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use lamella_cil::{Instruction, MethodBodyImage, Opcode, Operand, OperandKind};
use lamella_ir::{
    BasicBlock, BinOp, BlockId, CmpOp, ConvKind, Function, Inst, MirType, Terminator, ValueId,
};

/// Why a method body could not be lowered to MIR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CilError {
    /// An opcode needed more operands than the evaluation stack held.
    StackUnderflow,
    /// The method body did not end in `ret` (control flow is not lowered yet).
    NoReturn,
    /// An opcode's decoded operand was not the shape the opcode requires.
    BadOperand,
    /// A CIL opcode this lowering does not handle yet.
    Unsupported(Opcode),
    /// A `call` target token could not be resolved (no [`CallResolver`] mapping).
    UnresolvedCall,
    /// A control-flow shape this lowering does not handle yet: a conditional branch
    /// into a merge block (which would need its edge split), an entry block reached
    /// by a back-edge, or a block that runs off the end of the method.
    UnsupportedControlFlow,
}

/// What a `call`'s target is, recovered from its metadata token by a [`CallResolver`].
pub enum CallTarget {
    /// A method within this program, by function index -- lowered to a direct call.
    Internal(u32),
    /// A recognized BCL method, lowered to a backend intrinsic instead of a call.
    Intrinsic(Intrinsic),
}

/// A BCL method the AOT lowers specially rather than as a managed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intrinsic {
    /// `System.Diagnostics.Debug.WriteLine(string)` -> semihosting output.
    DebugWriteLine,
}

/// What the lowering needs about a `call` target: how many arguments to pop, whether it
/// yields a value, and what it resolves to.
pub struct CallInfo {
    /// The number of arguments the callee takes (popped from the evaluation stack).
    pub args: usize,
    /// Whether the call pushes a return value onto the stack.
    pub has_result: bool,
    /// The resolved target.
    pub target: CallTarget,
}

/// Resolves a `call`'s metadata token to a [`CallInfo`]. The lowering owns this seam; the
/// implementation (over `lamella-metadata`) lives in the caller, so CIL->MIR lowering
/// needs no metadata of its own and stays testable against a mock.
pub trait CallResolver {
    /// Resolves a `call`'s operand (its metadata token) to a [`CallInfo`], or `None` if
    /// the target is unknown or unsupported.
    fn resolve(&self, operand: &Operand) -> Option<CallInfo>;

    /// Resolves an `ldstr`'s operand (a `#US` user-string token) to the string's bytes,
    /// or `None`. Defaults to `None` for resolvers that handle only calls; the lowering
    /// adds the newline and NUL terminator semihosting needs.
    fn user_string(&self, _operand: &Operand) -> Option<Box<[u8]>> {
        None
    }

    /// The byte offset of a field (an `ldfld`/`stfld` operand token) within its declaring
    /// value type's layout. Defaults to `None`.
    fn field_offset(&self, _operand: &Operand) -> Option<u32> {
        None
    }

    /// The size in bytes of a value type (an `initobj` type-operand token), from its layout.
    /// Defaults to `None`.
    fn value_type_size(&self, _operand: &Operand) -> Option<u32> {
        None
    }
}

/// A [`CallResolver`] for call-free bodies: every resolution fails. The default for the
/// existing entry points, which lower methods that make no calls (the MMIO drivers).
pub struct NoCalls;

impl CallResolver for NoCalls {
    fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
        None
    }
}

/// Lowers an integer [`MethodBodyImage`] to a MIR [`Function`] by abstract
/// interpretation: the CIL is split into basic blocks, the evaluation stack and
/// locals are tracked per block, and join points (merges) become block parameters.
fn lower_with_source(
    body: &MethodBodyImage,
    resolver: &dyn CallResolver,
    arg_types: &[MirType],
    local_types: &[MirType],
) -> Result<(Function, CilSourceMap), CilError> {
    let code = &body.code;
    let mut byte_offsets: Vec<u32> = Vec::with_capacity(code.len());
    let mut running = 0u32;
    for instr in code.iter() {
        byte_offsets.push(running);
        let opcode = instr.opcode.encoding().byte_len() as u32;
        let operand = match instr.opcode.operand_kind() {
            OperandKind::Switch => match &instr.operand {
                Operand::Switch(targets) => 4 + targets.len() as u32 * 4,
                _ => 4,
            },
            kind => kind.fixed_operand_len().unwrap_or(0) as u32,
        };
        running = running.wrapping_add(opcode + operand);
    }
    let blocks = control_flow::discover_blocks(code);
    let preds = control_flow::predecessors(code, &blocks);
    let (arg_count, local_count) = scan_slots(code);

    let block_of = |instr: usize| blocks.iter().position(|&(s, e)| instr >= s && instr < e);
    let is_merge = |b: usize| preds.get(b).is_some_and(|p| p.len() > 1);

    if is_merge(0) {
        return Err(CilError::UnsupportedControlFlow);
    }

    let mut value_types: Vec<MirType> = Vec::new();
    let mut strings: Vec<(ValueId, Box<[u8]>)> = Vec::new();
    let args: Vec<ValueId> = (0..arg_count)
        .map(|i| {
            new_value(
                &mut value_types,
                arg_types.get(i).copied().unwrap_or(MirType::I32),
            )
        })
        .collect();

    let mut block_params: Vec<Vec<ValueId>> = Vec::with_capacity(blocks.len());
    for b in 0..blocks.len() {
        let params = if b == 0 {
            args.clone()
        } else if is_merge(b) {
            (0..local_count)
                .map(|i| {
                    let ty = local_types.get(i).copied().unwrap_or(MirType::I32);
                    new_value(&mut value_types, ty)
                })
                .collect()
        } else {
            Vec::new()
        };
        block_params.push(params);
    }

    let mut mir_blocks: Vec<BasicBlock> = Vec::with_capacity(blocks.len());
    let mut source_map: Vec<Vec<u32>> = Vec::with_capacity(blocks.len());
    let mut exit_locals: Vec<Vec<Option<ValueId>>> = vec![Vec::new(); blocks.len()];

    for (b, &(start, end)) in blocks.iter().enumerate() {
        let mut locals: Vec<Option<ValueId>> = if b == 0 {
            vec![None; local_count]
        } else if is_merge(b) {
            block_params[b].iter().map(|&p| Some(p)).collect()
        } else {
            let pred = *preds[b].first().ok_or(CilError::UnsupportedControlFlow)?;
            if pred < b {
                exit_locals[pred].clone()
            } else if is_merge(pred) {
                block_params[pred].iter().map(|&p| Some(p)).collect()
            } else {
                return Err(CilError::UnsupportedControlFlow);
            }
        };
        locals.resize(local_count, None);

        let mut stack: Vec<ValueId> = Vec::new();
        let mut insts: Vec<(ValueId, Inst)> = Vec::new();
        let mut il_index: Vec<u32> = Vec::new();
        let mut terminator: Option<Terminator> = None;
        let mut last_local_addr: Option<usize> = None;

        for i in start..end {
            let inst = &code[i];
            let is_last = i + 1 == end;
            let before = insts.len();
            if is_last && inst.opcode == Opcode::Ret {
                terminator = Some(Terminator::Return(stack.pop()));
            } else if is_last && control_flow::branch_kind(inst.opcode).is_some() {
                terminator = Some(build_branch(
                    inst,
                    end,
                    &block_of,
                    &is_merge,
                    local_count,
                    &mut stack,
                    &locals,
                    &mut value_types,
                    &mut insts,
                )?);
            } else {
                apply_value_op(
                    inst,
                    &mut value_types,
                    &mut stack,
                    &mut locals,
                    &args,
                    &mut insts,
                    &mut strings,
                    resolver,
                    &mut last_local_addr,
                )?;
            }
            for _ in before..insts.len() {
                il_index.push(byte_offsets[i]);
            }
        }

        let terminator = match terminator {
            Some(t) => t,
            None => {
                let next = b + 1;
                if next >= blocks.len() {
                    return Err(CilError::UnsupportedControlFlow);
                }
                let merge_args = merge_args(
                    is_merge(next),
                    local_count,
                    &locals,
                    &mut value_types,
                    &mut insts,
                );
                Terminator::Jump {
                    target: BlockId(next as u32),
                    args: merge_args,
                }
            }
        };

        while il_index.len() < insts.len() {
            il_index.push(
                byte_offsets
                    .get(end.saturating_sub(1))
                    .copied()
                    .unwrap_or(0),
            );
        }

        exit_locals[b] = locals.clone();
        mir_blocks.push(BasicBlock {
            params: block_params[b].clone(),
            insts,
            terminator: Some(terminator),
        });
        source_map.push(il_index);
    }

    let ret = mir_blocks.iter().find_map(|blk| match &blk.terminator {
        Some(Terminator::Return(Some(v))) => value_types.get(v.index()).copied(),
        _ => None,
    });

    let function = Function {
        params: (0..arg_count)
            .map(|i| arg_types.get(i).copied().unwrap_or(MirType::I32))
            .collect(),
        ret,
        value_types,
        entry: BlockId(0),
        blocks: mir_blocks,
    };
    Ok((function, CilSourceMap(source_map)))
}

/// The CIL byte offset each MIR instruction was lowered from, indexed by block then by
/// instruction within the block -- the lowering's half of the native-to-source mapping.
/// The target lowering pairs these with native code offsets to build a line table; the
/// compiler's sequence points then carry a CIL byte offset to a source line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CilSourceMap(pub Vec<Vec<u32>>);

/// Lowers an integer [`MethodBodyImage`] to a MIR [`Function`]. See
/// [`lower_method_debug`] for the accompanying [`CilSourceMap`].
pub fn lower_method(body: &MethodBodyImage) -> Result<Function, CilError> {
    lower_with_source(body, &NoCalls, &[], &[]).map(|(function, _)| function)
}

/// Lowers a method body, also returning the [`CilSourceMap`] tying each MIR
/// instruction back to the CIL instruction it came from.
pub fn lower_method_debug(body: &MethodBodyImage) -> Result<(Function, CilSourceMap), CilError> {
    lower_with_source(body, &NoCalls, &[], &[])
}

/// Lowers a method body that makes calls, using `resolver` to map each `call`'s token to
/// its target -- an internal callee or a recognized [`Intrinsic`] -- and returns the
/// [`CilSourceMap`] as well. See [`CallResolver`].
pub fn lower_method_debug_with(
    body: &MethodBodyImage,
    resolver: &dyn CallResolver,
) -> Result<(Function, CilSourceMap), CilError> {
    lower_with_source(body, resolver, &[], &[])
}

/// Lowers a method body with explicit parameter and local types (mapped from the method's
/// signature and local-variable signature), so `int64`, value-type, and other non-`int32`
/// slots type correctly instead of defaulting to `int32`. A slot with no supplied type
/// defaults to `int32`.
pub fn lower_method_typed(
    body: &MethodBodyImage,
    resolver: &dyn CallResolver,
    arg_types: &[MirType],
    local_types: &[MirType],
) -> Result<(Function, CilSourceMap), CilError> {
    lower_with_source(body, resolver, arg_types, local_types)
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
    let ty = value_types
        .get(lhs.0 as usize)
        .copied()
        .unwrap_or(MirType::I32);
    let result = new_value(value_types, ty);
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

/// Pops one operand and pushes its sub-word width conversion (the CLI's `conv.i1`/
/// `conv.u1`/`conv.i2`/`conv.u2`); the result is `int32`.
fn convert(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    kind: ConvKind,
) -> Result<(), CilError> {
    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
    let result = new_value(value_types, MirType::I32);
    insts.push((result, Inst::Convert { value, kind }));
    stack.push(result);
    Ok(())
}

/// Widens the top of stack to `int64` (sign- or zero-extended); a no-op if it is already
/// `int64` (the CLI's `conv.i8`/`conv.u8`).
fn widen(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    signed: bool,
) -> Result<(), CilError> {
    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
    if value_types.get(value.0 as usize) == Some(&MirType::I64) {
        stack.push(value);
        return Ok(());
    }
    let result = new_value(value_types, MirType::I64);
    insts.push((result, Inst::Widen { value, signed }));
    stack.push(result);
    Ok(())
}

/// Narrows the top of stack to `int32`: truncates an `int64`, or a no-op on a 32-bit value
/// (the CLI's `conv.i4`/`conv.u4`).
fn narrow_to_i32(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> Result<(), CilError> {
    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
    if value_types.get(value.0 as usize) != Some(&MirType::I64) {
        stack.push(value);
        return Ok(());
    }
    let result = new_value(value_types, MirType::I32);
    insts.push((result, Inst::Truncate { value }));
    stack.push(result);
    Ok(())
}

/// Applies one value-producing CIL instruction to the abstract state. Control-flow
/// terminators (`ret` and the branches) are handled by the caller, not here.
#[allow(clippy::too_many_arguments)]
fn apply_value_op(
    inst: &Instruction,
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    locals: &mut [Option<ValueId>],
    args: &[ValueId],
    insts: &mut Vec<(ValueId, Inst)>,
    strings: &mut Vec<(ValueId, Box<[u8]>)>,
    resolver: &dyn CallResolver,
    last_local_addr: &mut Option<usize>,
) -> Result<(), CilError> {
    match inst.opcode {
        Opcode::Nop => {}
        Opcode::Ldarg0 => push_arg(args, stack, 0)?,
        Opcode::Ldarg1 => push_arg(args, stack, 1)?,
        Opcode::Ldarg2 => push_arg(args, stack, 2)?,
        Opcode::Ldarg3 => push_arg(args, stack, 3)?,
        Opcode::LdargS | Opcode::Ldarg => {
            let Operand::Variable(n) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            push_arg(args, stack, *n as usize)?;
        }
        Opcode::Ldloc0 => push_local(value_types, locals, stack, insts, 0)?,
        Opcode::Ldloc1 => push_local(value_types, locals, stack, insts, 1)?,
        Opcode::Ldloc2 => push_local(value_types, locals, stack, insts, 2)?,
        Opcode::Ldloc3 => push_local(value_types, locals, stack, insts, 3)?,
        Opcode::LdlocS | Opcode::Ldloc => {
            let Operand::Variable(n) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            push_local(value_types, locals, stack, insts, *n as usize)?;
        }
        Opcode::Stloc0 => store_local(locals, stack, 0)?,
        Opcode::Stloc1 => store_local(locals, stack, 1)?,
        Opcode::Stloc2 => store_local(locals, stack, 2)?,
        Opcode::Stloc3 => store_local(locals, stack, 3)?,
        Opcode::StlocS | Opcode::Stloc => {
            let Operand::Variable(n) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            store_local(locals, stack, *n as usize)?;
        }
        Opcode::LdcI4M1 => push_const(value_types, stack, insts, -1),
        Opcode::LdcI40 => push_const(value_types, stack, insts, 0),
        Opcode::LdcI41 => push_const(value_types, stack, insts, 1),
        Opcode::LdcI42 => push_const(value_types, stack, insts, 2),
        Opcode::LdcI43 => push_const(value_types, stack, insts, 3),
        Opcode::LdcI44 => push_const(value_types, stack, insts, 4),
        Opcode::LdcI45 => push_const(value_types, stack, insts, 5),
        Opcode::LdcI46 => push_const(value_types, stack, insts, 6),
        Opcode::LdcI47 => push_const(value_types, stack, insts, 7),
        Opcode::LdcI48 => push_const(value_types, stack, insts, 8),
        Opcode::LdcI4S => {
            let Operand::Int8(v) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            push_const(value_types, stack, insts, i64::from(*v));
        }
        Opcode::LdcI4 => {
            let Operand::Int32(v) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            push_const(value_types, stack, insts, i64::from(*v));
        }
        Opcode::LdcI8 => {
            let Operand::Int64(v) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            let result = new_value(value_types, MirType::I64);
            insts.push((
                result,
                Inst::ConstInt {
                    ty: MirType::I64,
                    value: *v,
                },
            ));
            stack.push(result);
        }
        Opcode::Add => binary(value_types, stack, insts, BinOp::Add)?,
        Opcode::Sub => binary(value_types, stack, insts, BinOp::Sub)?,
        Opcode::Mul => binary(value_types, stack, insts, BinOp::Mul)?,
        Opcode::And => binary(value_types, stack, insts, BinOp::And)?,
        Opcode::Or => binary(value_types, stack, insts, BinOp::Or)?,
        Opcode::Xor => binary(value_types, stack, insts, BinOp::Xor)?,
        Opcode::Shl => binary(value_types, stack, insts, BinOp::Shl)?,
        Opcode::Shr => binary(value_types, stack, insts, BinOp::ShrSigned)?,
        Opcode::ShrUn => binary(value_types, stack, insts, BinOp::ShrUnsigned)?,
        Opcode::Ceq => compare(value_types, stack, insts, CmpOp::Eq)?,
        Opcode::Cgt => compare(value_types, stack, insts, CmpOp::SignedGt)?,
        Opcode::CgtUn => compare(value_types, stack, insts, CmpOp::UnsignedGt)?,
        Opcode::Clt => compare(value_types, stack, insts, CmpOp::SignedLt)?,
        Opcode::Neg => {
            let x = stack.pop().ok_or(CilError::StackUnderflow)?;
            let zero = new_value(value_types, MirType::I32);
            insts.push((
                zero,
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: 0,
                },
            ));
            let result = new_value(value_types, MirType::I32);
            insts.push((
                result,
                Inst::Binary {
                    op: BinOp::Sub,
                    lhs: zero,
                    rhs: x,
                },
            ));
            stack.push(result);
        }
        Opcode::Not => {
            let x = stack.pop().ok_or(CilError::StackUnderflow)?;
            let ones = new_value(value_types, MirType::I32);
            insts.push((
                ones,
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: -1,
                },
            ));
            let result = new_value(value_types, MirType::I32);
            insts.push((
                result,
                Inst::Binary {
                    op: BinOp::Xor,
                    lhs: x,
                    rhs: ones,
                },
            ));
            stack.push(result);
        }
        Opcode::ConvI1 => convert(value_types, stack, insts, ConvKind::SignExtend8)?,
        Opcode::ConvU1 => convert(value_types, stack, insts, ConvKind::ZeroExtend8)?,
        Opcode::ConvI2 => convert(value_types, stack, insts, ConvKind::SignExtend16)?,
        Opcode::ConvU2 => convert(value_types, stack, insts, ConvKind::ZeroExtend16)?,
        Opcode::ConvI8 => widen(value_types, stack, insts, true)?,
        Opcode::ConvU8 => widen(value_types, stack, insts, false)?,
        Opcode::ConvI4 | Opcode::ConvU4 => narrow_to_i32(value_types, stack, insts)?,
        Opcode::Pop => {
            stack.pop().ok_or(CilError::StackUnderflow)?;
        }
        Opcode::Dup => {
            let top = *stack.last().ok_or(CilError::StackUnderflow)?;
            stack.push(top);
        }
        Opcode::ConvI | Opcode::ConvU => {}
        Opcode::StindI4 => {
            let value = stack.pop().ok_or(CilError::StackUnderflow)?;
            let address = stack.pop().ok_or(CilError::StackUnderflow)?;
            let result = new_value(value_types, MirType::I32);
            insts.push((result, Inst::Store { address, value }));
        }
        Opcode::LdindI4 | Opcode::LdindU4 => {
            let address = stack.pop().ok_or(CilError::StackUnderflow)?;
            let result = new_value(value_types, MirType::I32);
            insts.push((result, Inst::Load { address }));
            stack.push(result);
        }
        Opcode::Ldstr => {
            let bytes = resolver
                .user_string(&inst.operand)
                .ok_or(CilError::UnresolvedCall)?;
            let value = new_value(value_types, MirType::I32);
            strings.push((value, bytes));
            stack.push(value);
        }
        Opcode::Call => {
            let info = resolver
                .resolve(&inst.operand)
                .ok_or(CilError::UnresolvedCall)?;
            let mut call_args = Vec::with_capacity(info.args);
            for _ in 0..info.args {
                call_args.push(stack.pop().ok_or(CilError::StackUnderflow)?);
            }
            call_args.reverse();
            match info.target {
                CallTarget::Internal(callee) => {
                    let result = new_value(value_types, MirType::I32);
                    insts.push((
                        result,
                        Inst::Call {
                            callee,
                            args: call_args,
                        },
                    ));
                    if info.has_result {
                        stack.push(result);
                    }
                }
                CallTarget::Intrinsic(Intrinsic::DebugWriteLine) => {
                    let string_value = *call_args.first().ok_or(CilError::StackUnderflow)?;
                    let bytes = strings
                        .iter()
                        .rev()
                        .find(|(v, _)| *v == string_value)
                        .map(|(_, b)| b.clone())
                        .ok_or(CilError::UnresolvedCall)?;
                    let mut text = bytes.into_vec();
                    text.push(b'\n');
                    text.push(0);
                    let result = new_value(value_types, MirType::I32);
                    insts.push((
                        result,
                        Inst::SemihostWrite {
                            text: text.into_boxed_slice(),
                        },
                    ));
                }
            }
        }
        Opcode::LdlocaS | Opcode::Ldloca => {
            let Operand::Variable(n) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            *last_local_addr = Some(*n as usize);
        }
        Opcode::Initobj => {
            let n = last_local_addr.take().ok_or(CilError::BadOperand)?;
            let size = resolver
                .value_type_size(&inst.operand)
                .ok_or(CilError::BadOperand)?;
            let zeroed = new_value(
                value_types,
                MirType::ValueType {
                    handle: lamella_ir::TypeHandle(0),
                    size,
                },
            );
            insts.push((zeroed, Inst::InitStruct));
            *locals.get_mut(n).ok_or(CilError::BadOperand)? = Some(zeroed);
        }
        Opcode::Ldfld => {
            let n = last_local_addr.take().ok_or(CilError::BadOperand)?;
            let base = locals.get(n).and_then(|x| *x).ok_or(CilError::BadOperand)?;
            let offset = resolver
                .field_offset(&inst.operand)
                .ok_or(CilError::BadOperand)?;
            let result = new_value(value_types, MirType::I32);
            insts.push((result, Inst::FieldLoad { base, offset }));
            stack.push(result);
        }
        Opcode::Stfld => {
            let n = last_local_addr.take().ok_or(CilError::BadOperand)?;
            let base = locals.get(n).and_then(|x| *x).ok_or(CilError::BadOperand)?;
            let offset = resolver
                .field_offset(&inst.operand)
                .ok_or(CilError::BadOperand)?;
            let value = stack.pop().ok_or(CilError::StackUnderflow)?;
            let placeholder = new_value(value_types, MirType::I32);
            insts.push((
                placeholder,
                Inst::FieldStore {
                    base,
                    offset,
                    value,
                },
            ));
        }
        other => return Err(CilError::Unsupported(other)),
    }
    Ok(())
}

/// The arguments a predecessor passes to a successor. A merge block takes a
/// parameter per local, so the predecessor passes its current locals, materializing
/// a zero for any never written along this path (CIL zero-initializes locals). A
/// non-merge successor inherits directly and receives no arguments.
fn merge_args(
    target_is_merge: bool,
    local_count: usize,
    locals: &[Option<ValueId>],
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> Vec<ValueId> {
    if !target_is_merge {
        return Vec::new();
    }
    (0..local_count)
        .map(|slot| match locals.get(slot).copied().flatten() {
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
                zero
            }
        })
        .collect()
}

/// Builds the terminator for a block ending in a branch. `fallthrough` is the
/// instruction index immediately after the block (the not-taken successor).
#[allow(clippy::too_many_arguments)]
fn build_branch(
    inst: &Instruction,
    fallthrough: usize,
    block_of: &impl Fn(usize) -> Option<usize>,
    is_merge: &impl Fn(usize) -> bool,
    local_count: usize,
    stack: &mut Vec<ValueId>,
    locals: &[Option<ValueId>],
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> Result<Terminator, CilError> {
    let Operand::Target(target_instr) = &inst.operand else {
        return Err(CilError::BadOperand);
    };
    let target = block_of(*target_instr as usize).ok_or(CilError::UnsupportedControlFlow)?;

    match control_flow::branch_kind(inst.opcode) {
        Some(control_flow::BranchKind::Unconditional) => {
            let args = merge_args(is_merge(target), local_count, locals, value_types, insts);
            Ok(Terminator::Jump {
                target: BlockId(target as u32),
                args,
            })
        }
        Some(control_flow::BranchKind::Conditional) => {
            let other = block_of(fallthrough).ok_or(CilError::UnsupportedControlFlow)?;
            if is_merge(target) || is_merge(other) {
                return Err(CilError::UnsupportedControlFlow);
            }
            let (cond, if_true, if_false) =
                build_condition(inst.opcode, target, other, stack, value_types, insts)?;
            Ok(Terminator::Branch {
                cond,
                if_true: BlockId(if_true as u32),
                true_args: Vec::new(),
                if_false: BlockId(if_false as u32),
                false_args: Vec::new(),
            })
        }
        None => Err(CilError::UnsupportedControlFlow),
    }
}

/// Builds the condition value for a conditional branch and resolves which block is
/// the taken (`if_true`) and not-taken (`if_false`) successor. The compare-branches
/// (`beq`/`blt`/...) test two popped operands; `brtrue`/`brfalse` test one.
fn build_condition(
    op: Opcode,
    target: usize,
    fallthrough: usize,
    stack: &mut Vec<ValueId>,
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> Result<(ValueId, usize, usize), CilError> {
    let compare_op = match op {
        Opcode::BeqS | Opcode::Beq => Some(CmpOp::Eq),
        Opcode::BneUnS | Opcode::BneUn => Some(CmpOp::Ne),
        Opcode::BgtS | Opcode::Bgt => Some(CmpOp::SignedGt),
        Opcode::BgtUnS | Opcode::BgtUn => Some(CmpOp::UnsignedGt),
        Opcode::BltS | Opcode::Blt => Some(CmpOp::SignedLt),
        Opcode::BltUnS | Opcode::BltUn => Some(CmpOp::UnsignedLt),
        Opcode::BgeS | Opcode::Bge => Some(CmpOp::SignedGe),
        Opcode::BgeUnS | Opcode::BgeUn => Some(CmpOp::UnsignedGe),
        Opcode::BleS | Opcode::Ble => Some(CmpOp::SignedLe),
        Opcode::BleUnS | Opcode::BleUn => Some(CmpOp::UnsignedLe),
        _ => None,
    };
    if let Some(cmpop) = compare_op {
        let rhs = stack.pop().ok_or(CilError::StackUnderflow)?;
        let lhs = stack.pop().ok_or(CilError::StackUnderflow)?;
        let cond = new_value(value_types, MirType::I32);
        insts.push((
            cond,
            Inst::Compare {
                op: cmpop,
                lhs,
                rhs,
            },
        ));
        return Ok((cond, target, fallthrough));
    }
    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
    match op {
        Opcode::BrtrueS | Opcode::Brtrue => Ok((value, target, fallthrough)),
        Opcode::BrfalseS | Opcode::Brfalse => Ok((value, fallthrough, target)),
        _ => Err(CilError::Unsupported(op)),
    }
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
            Opcode::LdlocS
            | Opcode::Ldloc
            | Opcode::StlocS
            | Opcode::Stloc
            | Opcode::LdlocaS
            | Opcode::Ldloca => {
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
/// discovery and predecessors, consumed by the lowering's abstract interpreter.
mod control_flow {
    use alloc::vec;
    use alloc::vec::Vec;

    use alloc::collections::BTreeSet;
    use lamella_cil::{Instruction, Opcode, Operand};

    #[test]
    fn lowers_struct_field_access() {
        use super::*;
        use lamella_token::Token;
        struct Fields;
        impl CallResolver for Fields {
            fn resolve(&self, _: &Operand) -> Option<CallInfo> {
                None
            }
            fn field_offset(&self, op: &Operand) -> Option<u32> {
                match op {
                    Operand::Token(t) if (t.0 & 0x00FF_FFFF) == 1 => Some(0),
                    Operand::Token(t) if (t.0 & 0x00FF_FFFF) == 2 => Some(4),
                    _ => None,
                }
            }
            fn value_type_size(&self, _: &Operand) -> Option<u32> {
                Some(8)
            }
        }
        let field = |row| Operand::Token(Token::new(0x04, row));
        let ty = Operand::Token(Token::new(0x02, 1));
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::new(Opcode::Initobj, ty),
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::simple(Opcode::LdcI43),
                Instruction::new(Opcode::Stfld, field(1)),
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::simple(Opcode::LdcI45),
                Instruction::new(Opcode::Stfld, field(2)),
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::new(Opcode::Ldfld, field(1)),
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::new(Opcode::Ldfld, field(2)),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::LdcI48),
                Instruction::simple(Opcode::Ceq),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let point = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 8,
        };
        let (func, _) = lower_method_typed(&body, &Fields, &[], &[point]).unwrap();
        let insts: Vec<_> = func.blocks[0].insts.iter().map(|(_, i)| i).collect();
        assert!(insts.iter().any(|i| matches!(i, Inst::InitStruct)));
        assert!(insts.iter().any(|i| matches!(i, Inst::FieldStore { .. })));
        assert!(insts.iter().any(|i| matches!(i, Inst::FieldLoad { .. })));
        assert!(crate::arm32::lower(&func).is_ok());
    }

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
    fn lowers_a_call_through_the_resolver() {
        struct TwoArgReturning;
        impl CallResolver for TwoArgReturning {
            fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
                Some(CallInfo {
                    args: 2,
                    has_result: true,
                    target: CallTarget::Internal(7),
                })
            }
        }
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI43),
                Instruction::simple(Opcode::LdcI44),
                Instruction::new(Opcode::Call, Operand::None),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_debug_with(&body, &TwoArgReturning).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let call = func.blocks[0].insts.iter().find_map(|(_, i)| match i {
            Inst::Call { callee, args } => Some((*callee, args.len())),
            _ => None,
        });
        assert_eq!(call, Some((7, 2)));
    }

    #[test]
    fn lowers_debug_writeline_to_semihosting() {
        struct DebugMock;
        impl CallResolver for DebugMock {
            fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
                Some(CallInfo {
                    args: 1,
                    has_result: false,
                    target: CallTarget::Intrinsic(Intrinsic::DebugWriteLine),
                })
            }
            fn user_string(&self, _operand: &Operand) -> Option<Box<[u8]>> {
                Some(b"Hi".to_vec().into_boxed_slice())
            }
        }
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::Ldstr, Operand::None),
                Instruction::new(Opcode::Call, Operand::None),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_debug_with(&body, &DebugMock).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let text = func.blocks[0].insts.iter().find_map(|(_, i)| match i {
            Inst::SemihostWrite { text } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(text.as_deref(), Some(&b"Hi\n\0"[..]));
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
    fn lowers_neg() {
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI45),
                Instruction::simple(Opcode::Neg),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
    }

    #[test]
    fn lowers_a_counting_loop() {
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: true,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::Stloc1),
                Instruction::new(Opcode::BrS, Operand::Target(13)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Ldloc1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc1),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Stloc1),
                Instruction::simple(Opcode::Ldloc1),
                Instruction::simple(Opcode::LdcI45),
                Instruction::new(Opcode::BleS, Operand::Target(5)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert_eq!(func.blocks.len(), 4);
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(func.value_types.len() > 8);
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn lowers_an_if_else() {
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI45),
                Instruction::simple(Opcode::LdcI43),
                Instruction::new(Opcode::BgtS, Operand::Target(5)),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(9)),
                Instruction::simple(Opcode::Ret),
                Instruction::simple(Opcode::LdcI47),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert_eq!(func.blocks.len(), 3);
        assert!(lamella_ir::verify(&func).is_ok());
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower(&func).is_ok());
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
        assert!(lower_method(&body).is_err());
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn ldc_add_ret_lowers_through_to_arm32() {
        let func = lower_method(&forty_plus_two()).unwrap();
        let bytes = crate::arm32::lower(&func).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], &[0x70, 0x47]);
    }
}
