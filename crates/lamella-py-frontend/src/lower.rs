//! The typed `Python -> MIR` lowering: a [`bc::CodeObject`] to a
//! [`lamella_ir::Function`].


use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use lamella_ir::{
    BasicBlock, BinOp as MBinOp, BlockId, CmpOp as MCmpOp, ConvKind, Function, Inst, MirType, PyOp,
    Terminator, ValueId,
};
use lamella_py_bytecode as bc;

/// Why a code object could not be lowered to MIR. Most variants mark a construct
/// outside the typed subset rather than a true error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerError {
    /// An op needed more operands than the abstract stack held.
    StackUnderflow,
    /// A block left values on the stack at its boundary -- the subset guarantees an
    /// empty stack there, so this signals an unexpected (out-of-subset) shape.
    StackNotEmpty,
    /// A constant-pool index was out of range.
    BadConstIndex(u32),
    /// A local slot index was out of range.
    BadLocalIndex(u32),
    /// An integer literal did not fit a 32-bit machine word (outside `[i32::MIN, u32::MAX]`);
    /// the typed lane has no bignum.
    IntLiteralTooLarge(i64),
    /// A non-integer constant in the typed path (`None`/`True`/`False`/string); not
    /// lowered in the typed integer path.
    UnsupportedConst,
    /// Arithmetic or comparison on a dynamic (non-`I32`) operand; the typed path
    /// handles integer operands only.
    DynamicOperation,
    /// A global name resolved to no user function in this module (e.g. a builtin like
    /// `print`); only intra-module calls are lowered in the typed path.
    UnresolvedGlobal(String),
    /// A name-pool index was out of range.
    BadNameIndex(u32),
    /// A function name was used as a plain operand (functions are not first-class
    /// values in the typed subset).
    CallableAsValue,
    /// `Call` was applied to something that was not a resolved function.
    CallTargetNotCallable,
    /// A call passed the wrong number of arguments for its callee.
    ArityMismatch {
        /// The callee's declared parameter count.
        expected: usize,
        /// The number of arguments the call site passed.
        found: usize,
    },
    /// A keyword argument named a parameter the callee does not have.
    UnexpectedKeyword(String),
    /// An argument was passed both positionally and by keyword, or a keyword was repeated.
    DuplicateArgument(String),
    /// A conditional branch's condition was not an `I32`.
    BadConditionType,
    /// A `return` value's type did not match the function's return type.
    ReturnTypeMismatch,
    /// Control fell off the end of the body (a function body that does not return).
    RunsOffEnd,
    /// A non-parameter local was not an integer, so it has no typed-path default to
    /// initialize it with at entry.
    UnsupportedLocalType(usize),
    /// A non-merge block was reached before its single predecessor was lowered -- an
    /// irreducible control-flow shape the structured subset does not emit.
    UnsupportedControlFlow,
}

impl core::fmt::Display for LowerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LowerError::StackUnderflow => f.write_str("operand stack underflow"),
            LowerError::StackNotEmpty => f.write_str("operand stack not empty at a block boundary"),
            LowerError::BadConstIndex(i) => write!(f, "constant index {i} out of range"),
            LowerError::BadLocalIndex(i) => write!(f, "local index {i} out of range"),
            LowerError::IntLiteralTooLarge(v) => {
                write!(f, "integer literal {v} does not fit a 32-bit machine word")
            }
            LowerError::UnsupportedConst => {
                f.write_str("non-integer constant is not lowered in the typed integer path")
            }
            LowerError::DynamicOperation => f.write_str(
                "arithmetic/comparison on a dynamic value is not supported in the typed path",
            ),
            LowerError::UnresolvedGlobal(name) => {
                write!(f, "global `{name}` is not a user function in this module")
            }
            LowerError::BadNameIndex(i) => write!(f, "name index {i} out of range"),
            LowerError::CallableAsValue => {
                f.write_str("a function name was used as a plain value")
            }
            LowerError::CallTargetNotCallable => f.write_str("call target is not a function"),
            LowerError::ArityMismatch { expected, found } => {
                write!(f, "call passed {found} argument(s) but the callee takes {expected}")
            }
            LowerError::UnexpectedKeyword(name) => {
                write!(f, "unexpected keyword argument `{name}`")
            }
            LowerError::DuplicateArgument(name) => {
                write!(f, "argument `{name}` given both positionally and by keyword, or repeated")
            }
            LowerError::BadConditionType => f.write_str("a branch condition was not an i32"),
            LowerError::ReturnTypeMismatch => {
                f.write_str("a return value's type did not match the function's return type")
            }
            LowerError::RunsOffEnd => f.write_str("control runs off the end of the function body"),
            LowerError::UnsupportedLocalType(i) => {
                write!(f, "local slot {i} has no typed-path default initializer")
            }
            LowerError::UnsupportedControlFlow => {
                f.write_str("unsupported (irreducible) control-flow shape")
            }
        }
    }
}

/// The MIR type a Python static type lowers to: an annotated `int` is a machine
/// `I32` (machine-width integers, no bignum); anything dynamic is a tagged `PyValue`.
fn mir_type(ty: bc::StaticType) -> MirType {
    match ty {
        bc::StaticType::Int => MirType::I32,
        bc::StaticType::Float => MirType::F64,
        bc::StaticType::Dynamic => MirType::PyValue,
    }
}

/// Emit one instruction of type `ty`, returning its result value.
fn emit(values: &mut Values, insts: &mut Vec<(ValueId, Inst)>, inst: Inst, ty: MirType) -> ValueId {
    let id = values.fresh(ty);
    insts.push((id, inst));
    id
}

/// Promote a value to `f64`: an F64 passes through; an I32 is widened with `IntToFloat64` (Python
/// promotes an int operand of a mixed or true-division expression to float). Any other type is out
/// of the typed float lane.
fn promote_to_f64(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    value: ValueId,
    ty: MirType,
) -> Result<ValueId, LowerError> {
    match ty {
        MirType::F64 => Ok(value),
        MirType::I32 => Ok(emit(
            values,
            insts,
            Inst::Convert {
                value,
                kind: ConvKind::IntToFloat64,
            },
            MirType::F64,
        )),
        _ => Err(LowerError::DynamicOperation),
    }
}

/// The 0/1 correction that turns truncating division into Python's floor division:
/// 1 when there is a remainder AND the operands have different signs (then floor and
/// truncation disagree by one). Grounded in the 3.14.6 binary-arithmetic semantics
/// (`//` floors toward negative infinity; `%` takes the divisor's sign;
/// `x == (x//y)*y + (x%y)`).
fn floor_adjust(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    rem: ValueId,
    lhs: ValueId,
    rhs: ValueId,
) -> ValueId {
    let zero = emit(values, insts, Inst::ConstInt {
        ty: MirType::I32,
        value: 0,
    }, MirType::I32);
    let rem_nonzero = emit(values, insts, Inst::Compare {
        op: MCmpOp::Ne,
        lhs: rem,
        rhs: zero,
    }, MirType::I32);
    let xor = emit(values, insts, Inst::Binary {
        op: MBinOp::Xor,
        lhs,
        rhs,
    }, MirType::I32);
    let signs_differ = emit(values, insts, Inst::Compare {
        op: MCmpOp::SignedLt,
        lhs: xor,
        rhs: zero,
    }, MirType::I32);
    emit(values, insts, Inst::Binary {
        op: MBinOp::And,
        lhs: rem_nonzero,
        rhs: signs_differ,
    }, MirType::I32)
}

/// Python `a // b` for typed integers: the truncating quotient minus the floor
/// correction. A zero divisor traps in hardware; `ZeroDivisionError` requires
/// the exception machinery.
fn emit_floor_div(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    lhs: ValueId,
    rhs: ValueId,
) -> ValueId {
    let q = emit(values, insts, Inst::Binary {
        op: MBinOp::DivSigned,
        lhs,
        rhs,
    }, MirType::I32);
    let r = emit(values, insts, Inst::Binary {
        op: MBinOp::RemSigned,
        lhs,
        rhs,
    }, MirType::I32);
    let adjust = floor_adjust(values, insts, r, lhs, rhs);
    emit(values, insts, Inst::Binary {
        op: MBinOp::Sub,
        lhs: q,
        rhs: adjust,
    }, MirType::I32)
}

/// Python `a % b` for typed integers: the truncating remainder plus the floor
/// correction times the divisor, so the result takes the divisor's sign.
fn emit_floor_mod(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    lhs: ValueId,
    rhs: ValueId,
) -> ValueId {
    let r = emit(values, insts, Inst::Binary {
        op: MBinOp::RemSigned,
        lhs,
        rhs,
    }, MirType::I32);
    let adjust = floor_adjust(values, insts, r, lhs, rhs);
    let adjust_b = emit(values, insts, Inst::Binary {
        op: MBinOp::Mul,
        lhs: adjust,
        rhs,
    }, MirType::I32);
    emit(values, insts, Inst::Binary {
        op: MBinOp::Add,
        lhs: r,
        rhs: adjust_b,
    }, MirType::I32)
}

fn map_cmpop(op: bc::CmpOp) -> MCmpOp {
    match op {
        bc::CmpOp::Eq => MCmpOp::Eq,
        bc::CmpOp::Ne => MCmpOp::Ne,
        bc::CmpOp::Lt => MCmpOp::SignedLt,
        bc::CmpOp::Le => MCmpOp::SignedLe,
        bc::CmpOp::Gt => MCmpOp::SignedGt,
        bc::CmpOp::Ge => MCmpOp::SignedGe,
        bc::CmpOp::Is | bc::CmpOp::IsNot => {
            unreachable!("identity comparison is not in the typed lane")
        }
    }
}

/// Handle a built-in over typed integer arguments, returning its result value AND that
/// result's type. `abs`/`min`/`max` are branchless arithmetic (`abs(x)` = `(x ^ (x>>31)) -
/// (x>>31)`; `min`/`max` select via the `(a ^ b) & -(a < b)` mask), each an `i32`. The MMIO
/// builtins lower to a volatile `Load`/`Store`: a read yields the loaded `i32`; a write is a
/// side effect whose Python `None` result is modelled as an ignored `PyValue` placeholder (so
/// using a write's result in typed arithmetic fails loud rather than reading a bogus int). The
/// interpreter provides the same surface for the dynamic path; here the typed path needs no
/// runtime call.
fn inline_builtin(
    builtin: Builtin,
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    args: &[ValueId],
) -> Result<(ValueId, MirType), LowerError> {
    if args.len() != builtin.arity() {
        return Err(LowerError::ArityMismatch {
            expected: builtin.arity(),
            found: args.len(),
        });
    }
    Ok(match builtin {
        Builtin::Abs => {
            let x = args[0];
            let shift = emit(values, insts, Inst::ConstInt {
                ty: MirType::I32,
                value: 31,
            }, MirType::I32);
            let mask = emit(values, insts, Inst::Binary {
                op: MBinOp::ShrSigned,
                lhs: x,
                rhs: shift,
            }, MirType::I32);
            let flipped = emit(values, insts, Inst::Binary {
                op: MBinOp::Xor,
                lhs: x,
                rhs: mask,
            }, MirType::I32);
            let result = emit(values, insts, Inst::Binary {
                op: MBinOp::Sub,
                lhs: flipped,
                rhs: mask,
            }, MirType::I32);
            (result, MirType::I32)
        }
        Builtin::Min => (
            emit_select_extreme(values, insts, args[0], args[1], args[1]),
            MirType::I32,
        ),
        Builtin::Max => (
            emit_select_extreme(values, insts, args[0], args[1], args[0]),
            MirType::I32,
        ),
        Builtin::MmioRead { width } => {
            let value = emit(values, insts, Inst::Load {
                address: args[0],
                width,
                signed: false,
            }, MirType::I32);
            (value, MirType::I32)
        }
        Builtin::MmioWrite { width } => {
            let placeholder = emit(values, insts, Inst::Store {
                address: args[0],
                value: args[1],
                width,
            }, MirType::PyValue);
            (placeholder, MirType::PyValue)
        }
        Builtin::Print => {
            let placeholder =
                emit(values, insts, Inst::WriteInt { value: args[0] }, MirType::PyValue);
            (placeholder, MirType::PyValue)
        }
        Builtin::IntCast | Builtin::FloatCast | Builtin::Divmod | Builtin::Round => {
            return Err(LowerError::DynamicOperation)
        }
    })
}

fn emit_select_extreme(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    a: ValueId,
    b: ValueId,
    pick: ValueId,
) -> ValueId {
    let lt = emit(values, insts, Inst::Compare {
        op: MCmpOp::SignedLt,
        lhs: a,
        rhs: b,
    }, MirType::I32);
    let zero = emit(values, insts, Inst::ConstInt {
        ty: MirType::I32,
        value: 0,
    }, MirType::I32);
    let mask = emit(values, insts, Inst::Binary {
        op: MBinOp::Sub,
        lhs: zero,
        rhs: lt,
    }, MirType::I32);
    let axb = emit(values, insts, Inst::Binary {
        op: MBinOp::Xor,
        lhs: a,
        rhs: b,
    }, MirType::I32);
    let masked = emit(values, insts, Inst::Binary {
        op: MBinOp::And,
        lhs: axb,
        rhs: mask,
    }, MirType::I32);
    emit(values, insts, Inst::Binary {
        op: MBinOp::Xor,
        lhs: pick,
        rhs: masked,
    }, MirType::I32)
}

/// Allocates dense, single-assignment value ids and records each one's type.
struct Values {
    types: Vec<MirType>,
}

impl Values {
    fn new() -> Self {
        Values { types: Vec::new() }
    }

    fn fresh(&mut self, ty: MirType) -> ValueId {
        let id = ValueId(self.types.len() as u32);
        self.types.push(ty);
        id
    }
}

/// Per-block live-in sets over the bytecode CFG (a backward dataflow): which local
/// slots are read before being reassigned on some path out of the block. This drives
/// the minimal block-parameter set -- a merge carries a parameter only for a live-in
/// local, and the entry zero-initializes only a local that is live at the very start.
fn liveness(metas: &[BlockMeta], ops: &[bc::Op], n_locals: usize) -> Vec<Vec<bool>> {
    let n = metas.len();
    let mut use_set = vec![vec![false; n_locals]; n];
    let mut def_set = vec![vec![false; n_locals]; n];
    for (b, meta) in metas.iter().enumerate() {
        let body_end = if meta.ends_in_terminator {
            meta.end - 1
        } else {
            meta.end
        };
        for op in &ops[meta.start..body_end] {
            match op {
                bc::Op::LoadFast(i) => {
                    let s = *i as usize;
                    if s < n_locals && !def_set[b][s] {
                        use_set[b][s] = true;
                    }
                }
                bc::Op::StoreFast(i) => {
                    let s = *i as usize;
                    if s < n_locals {
                        def_set[b][s] = true;
                    }
                }
                _ => {}
            }
        }
    }
    let mut live_in = vec![vec![false; n_locals]; n];
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            for s in 0..n_locals {
                let live_out = metas[b].succs.iter().any(|succ| live_in[succ.index()][s]);
                if (use_set[b][s] || (live_out && !def_set[b][s])) && !live_in[b][s] {
                    live_in[b][s] = true;
                    changed = true;
                }
            }
        }
    }
    live_in
}

/// Lower one code object (a function or the `<module>` body) to a verified
/// [`Function`]. The caller hands the verified functions to the backend's
/// `lower_module_py`.
fn lower_function(
    co: &bc::CodeObject,
    funcs: &BTreeMap<String, FuncSig>,
    constants: &BTreeMap<String, i32>,
) -> Result<Function, LowerError> {
    let n_params = co.params.len();
    let n_locals = co.n_locals;
    let local_ty: Vec<MirType> = co.local_types.iter().map(|t| mir_type(*t)).collect();
    let ret_ty = mir_type(co.ret_ty);
    let mut values = Values::new();

    let metas = block_layout(&co.ops)?;
    if metas.is_empty() {
        return Err(LowerError::RunsOffEnd);
    }
    let n_bc = metas.len();
    let preds = compute_preds(&metas);
    let reachable = reachable_blocks(&metas);
    let live_in = liveness(&metas, &co.ops, n_locals);

    let is_merge: Vec<bool> = (0..n_bc)
        .map(|i| reachable[i] && preds[i].len() + usize::from(i == 0) >= 2)
        .collect();

    let func_params: Vec<ValueId> = (0..n_params).map(|i| values.fresh(local_ty[i])).collect();

    let mut synth_insts: Vec<(ValueId, Inst)> = Vec::new();
    let mut synth_locals: Vec<ValueId> = func_params.clone();
    for (i, &ty) in local_ty.iter().enumerate().skip(n_params) {
        if !live_in[0][i] {
            synth_locals.push(ValueId(0));
            continue;
        }
        if ty != MirType::I32 && ty != MirType::F64 {
            return Err(LowerError::UnsupportedLocalType(i));
        }
        let zero = values.fresh(ty);
        synth_insts.push((zero, Inst::ConstInt { ty, value: 0 }));
        synth_locals.push(zero);
    }
    debug_assert_eq!(synth_locals.len(), n_locals);

    let synth_id = BlockId(n_bc as u32);
    let tramp_base = n_bc + 1;
    let mut tramps: Vec<BasicBlock> = Vec::new();
    let mut blocks: Vec<BasicBlock> = Vec::with_capacity(n_bc + 1);
    let mut exit_locals: Vec<Option<Vec<ValueId>>> = vec![None; n_bc];
    let mut exit_stack: Vec<Option<Vec<(ValueId, MirType)>>> = vec![None; n_bc];

    for i in 0..n_bc {
        if !reachable[i] {
            blocks.push(unreachable_block());
            continue;
        }
        let (mut params, mut locals) = if is_merge[i] {
            let mut params = Vec::new();
            let mut locals = vec![ValueId(0); n_locals];
            for (s, slot) in locals.iter_mut().enumerate() {
                if live_in[i][s] {
                    let p = values.fresh(local_ty[s]);
                    params.push(p);
                    *slot = p;
                }
            }
            (params, locals)
        } else if i == 0 {
            (Vec::new(), synth_locals.clone())
        } else {
            let pred = preds[i][0];
            let inherited = exit_locals[pred]
                .clone()
                .ok_or(LowerError::UnsupportedControlFlow)?;
            (Vec::new(), inherited)
        };

        let mut stack: Vec<StackEntry> = if is_merge[i] {
            let incoming = preds[i]
                .iter()
                .find_map(|p| exit_stack[*p].clone())
                .unwrap_or_default();
            incoming
                .into_iter()
                .map(|(_, ty)| {
                    let p = values.fresh(ty);
                    params.push(p);
                    StackEntry::Value(p, ty)
                })
                .collect()
        } else if i == 0 {
            Vec::new()
        } else {
            exit_stack[preds[i][0]]
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|(v, ty)| StackEntry::Value(v, ty))
                .collect()
        };

        let meta = &metas[i];
        let body_end = if meta.ends_in_terminator {
            meta.end - 1
        } else {
            meta.end
        };
        let mut insts: Vec<(ValueId, Inst)> = Vec::new();
        for op in &co.ops[meta.start..body_end] {
            lower_op(co, funcs, constants, &local_ty, &mut values, &mut insts, &mut locals, &mut stack, op)?;
        }

        let terminator = if !meta.ends_in_terminator {
            let sv = stack_exit(&stack)?;
            jump_to(&is_merge, &live_in, meta.succs[0], &locals, &sv)
        } else {
            match &co.ops[meta.end - 1] {
                bc::Op::Jump(_) => {
                    let sv = stack_exit(&stack)?;
                    jump_to(&is_merge, &live_in, meta.succs[0], &locals, &sv)
                }
                bc::Op::PopJumpIfFalse(_) => {
                    let (cond, ct) = pop_value(&mut stack)?;
                    if ct != MirType::I32 {
                        return Err(LowerError::BadConditionType);
                    }
                    let sv = stack_exit(&stack)?;
                    let if_false =
                        branch_edge(&is_merge, &live_in, meta.succs[0], &locals, &sv, tramp_base, &mut tramps);
                    let if_true =
                        branch_edge(&is_merge, &live_in, meta.succs[1], &locals, &sv, tramp_base, &mut tramps);
                    Terminator::Branch {
                        cond,
                        if_true,
                        true_args: Vec::new(),
                        if_false,
                        false_args: Vec::new(),
                    }
                }
                bc::Op::Return => {
                    let (value, ty) = pop_value(&mut stack)?;
                    if ty != ret_ty {
                        return Err(LowerError::ReturnTypeMismatch);
                    }
                    Terminator::Return(Some(value))
                }
                _ => return Err(LowerError::RunsOffEnd),
            }
        };
        exit_stack[i] = Some(stack_exit(&stack)?);
        exit_locals[i] = Some(locals);
        blocks.push(BasicBlock {
            params,
            insts,
            terminator: Some(terminator),
        });
    }

    let synth_term = jump_to(&is_merge, &live_in, BlockId(0), &synth_locals, &[]);
    blocks.push(BasicBlock {
        params: func_params,
        insts: synth_insts,
        terminator: Some(synth_term),
    });
    blocks.extend(tramps);

    Ok(Function {
        params: (0..n_params).map(|i| local_ty[i]).collect(),
        ret: Some(ret_ty),
        blocks,
        entry: synth_id,
        value_types: values.types,
    })
}

fn unreachable_block() -> BasicBlock {
    BasicBlock {
        params: Vec::new(),
        insts: Vec::new(),
        terminator: Some(Terminator::Unreachable),
    }
}

/// The arguments a branch into `target` carries: when `target` is a merge, its live-in
/// locals (in slot order) followed by the threaded operand-stack values (bottom to
/// top) -- matching the order the merge declares those parameters. A non-merge target
/// has no parameters, so no arguments: it reuses the predecessor's values directly.
fn merge_args(
    is_merge: &[bool],
    live_in: &[Vec<bool>],
    target: BlockId,
    locals: &[ValueId],
    stack: &[(ValueId, MirType)],
) -> Vec<ValueId> {
    if !is_merge.get(target.index()).copied().unwrap_or(false) {
        return Vec::new();
    }
    let live = &live_in[target.index()];
    let mut args: Vec<ValueId> = (0..live.len()).filter(|&s| live[s]).map(|s| locals[s]).collect();
    args.extend(stack.iter().map(|(v, _)| *v));
    args
}

/// The operand stack as `(ValueId, type)` pairs, for threading to successors. A
/// callable left on the stack at a block boundary (a function used across a
/// mid-expression branch) is not supported in the typed subset.
fn stack_exit(stack: &[StackEntry]) -> Result<Vec<(ValueId, MirType)>, LowerError> {
    stack
        .iter()
        .map(|e| match e {
            StackEntry::Value(v, t) => Ok((*v, *t)),
            StackEntry::Callable(_) | StackEntry::Builtin(_) => Err(LowerError::CallableAsValue),
            StackEntry::Tuple(_) => Err(LowerError::DynamicOperation),
        })
        .collect()
}

/// A `Jump` to `target`, passing its live-in locals plus the threaded stack when
/// `target` is a merge; a non-merge target takes none.
fn jump_to(
    is_merge: &[bool],
    live_in: &[Vec<bool>],
    target: BlockId,
    locals: &[ValueId],
    stack: &[(ValueId, MirType)],
) -> Terminator {
    Terminator::Jump {
        target,
        args: merge_args(is_merge, live_in, target, locals, stack),
    }
}

/// Resolve one edge of a `Branch`. A `Branch` may carry no arguments, so an edge into
/// a merge block (which expects its live-in locals plus the threaded stack) is routed
/// through a parameter-less trampoline that jumps there with them; an edge into a
/// non-merge block is direct.
fn branch_edge(
    is_merge: &[bool],
    live_in: &[Vec<bool>],
    target: BlockId,
    locals: &[ValueId],
    stack: &[(ValueId, MirType)],
    tramp_base: usize,
    tramps: &mut Vec<BasicBlock>,
) -> BlockId {
    if is_merge.get(target.index()).copied().unwrap_or(false) {
        let id = BlockId((tramp_base + tramps.len()) as u32);
        tramps.push(BasicBlock {
            params: Vec::new(),
            insts: Vec::new(),
            terminator: Some(Terminator::Jump {
                target,
                args: merge_args(is_merge, live_in, target, locals, stack),
            }),
        });
        id
    } else {
        target
    }
}

/// One basic block's op range and how it leaves.
struct BlockMeta {
    /// The first op index (a leader).
    start: usize,
    /// One past the last op index.
    end: usize,
    /// The successor block ids (for predecessor and reachability analysis).
    succs: Vec<BlockId>,
    /// Whether the block's last op is a control-flow op (else it falls through).
    ends_in_terminator: bool,
}

/// Split the op stream into basic blocks at leaders and record each block's
/// successors. A leader is op 0, any jump target, and the op after any jump or
/// return.
fn block_layout(ops: &[bc::Op]) -> Result<Vec<BlockMeta>, LowerError> {
    if ops.is_empty() {
        return Ok(Vec::new());
    }
    let mut leaders: Vec<usize> = vec![0];
    for (i, op) in ops.iter().enumerate() {
        match op {
            bc::Op::Jump(t) | bc::Op::PopJumpIfFalse(t) => {
                leaders.push(*t as usize);
                if i + 1 < ops.len() {
                    leaders.push(i + 1);
                }
            }
            bc::Op::Return if i + 1 < ops.len() => leaders.push(i + 1),
            _ => {}
        }
    }
    leaders.sort_unstable();
    leaders.dedup();

    let block_of: BTreeMap<usize, BlockId> = leaders
        .iter()
        .enumerate()
        .map(|(i, &op)| (op, BlockId(i as u32)))
        .collect();
    let block_id = |op: usize| -> Result<BlockId, LowerError> {
        block_of.get(&op).copied().ok_or(LowerError::RunsOffEnd)
    };

    let mut metas = Vec::with_capacity(leaders.len());
    for (i, &start) in leaders.iter().enumerate() {
        let end = leaders.get(i + 1).copied().unwrap_or(ops.len());
        let last = &ops[end - 1];
        let (succs, ends_in_terminator) = match last {
            bc::Op::Jump(t) => (vec![block_id(*t as usize)?], true),
            bc::Op::PopJumpIfFalse(t) => (vec![block_id(*t as usize)?, block_id(end)?], true),
            bc::Op::Return => (Vec::new(), true),
            _ => (vec![block_id(end)?], false),
        };
        metas.push(BlockMeta {
            start,
            end,
            succs,
            ends_in_terminator,
        });
    }
    Ok(metas)
}

/// The predecessor block indices of each block, inverted from the successor lists.
fn compute_preds(metas: &[BlockMeta]) -> Vec<Vec<usize>> {
    let mut preds = vec![Vec::new(); metas.len()];
    for (j, meta) in metas.iter().enumerate() {
        for succ in &meta.succs {
            preds[succ.index()].push(j);
        }
    }
    preds
}

/// Mark blocks reachable from the entry (block 0) by following successors.
fn reachable_blocks(metas: &[BlockMeta]) -> Vec<bool> {
    let mut reachable = vec![false; metas.len()];
    if metas.is_empty() {
        return reachable;
    }
    let mut work = vec![BlockId(0)];
    reachable[0] = true;
    while let Some(b) = work.pop() {
        for &s in &metas[b.index()].succs {
            if !reachable[s.index()] {
                reachable[s.index()] = true;
                work.push(s);
            }
        }
    }
    reachable
}

/// A callee's call signature, for resolving `LoadGlobal`/`Call`: its index in the
/// module (the `Inst::Call` callee), its MIR return type, its parameter count (for the
/// arity check), and its parameter NAMES (to static-bind a keyword call to positional).
/// The index is module-relative; a driver that prepends functions (e.g. an AOT entry
/// shim) offsets it.
#[derive(Clone)]
struct FuncSig {
    index: u32,
    ret: MirType,
    arity: usize,
    param_names: Vec<String>,
    param_types: Vec<MirType>,
}

/// A built-in the typed path handles with no runtime call: `abs`/`min`/`max` over integers
/// (inlined to branchless arithmetic), the MMIO primitives (lowered to a volatile `Store`/`Load`),
/// and `print` of one int (lowered to a semihosting `WriteInt`). Other built-ins (`len`,
/// container/string operations) dispatch to the runtime and arrive with the dynamic surface.
///
/// The MMIO builtins are the AOT half of the language-neutral MMIO contract: `mmio_read{8,16,32}
/// (addr) -> int` and `mmio_write{8,16,32}(addr, value)`, lowering to the SAME volatile ldr/str
/// a C / Rust / C#-AOT BSP reaches (the "type gradient" -- no `py_*` runtime call). Names are
/// provisional pending the cross-lane contract (python-runtime wires the matching interp builtin).
#[derive(Clone, Copy)]
enum Builtin {
    Abs,
    Min,
    Max,
    /// `mmio_read{8,16,32}(addr) -> int` -- a volatile load of `width` bytes (zero-extended to i32).
    MmioRead { width: u32 },
    /// `mmio_write{8,16,32}(addr, value)` -- a volatile store of `width` bytes; returns None.
    MmioWrite { width: u32 },
    /// `print(x)` for one int argument -- a semihosting `WriteInt` (signed decimal + newline);
    /// returns None. Only the single-positional-int form; `print()`, multi-arg, and keyworded
    /// `print` fall to the dynamic path (they need spacing/`sep`/`end` the primitive does not do).
    Print,
    /// `int(x)` on a numeric argument -- `int(int)` is identity, `int(float)` a `Float64ToInt`
    /// convert (truncates toward zero == Python `int()`). A non-numeric arg (`int("5")`, base) is
    /// dynamic. Handled in the `Call` op (it needs the argument's type), not `inline_builtin`.
    IntCast,
    /// `float(x)` on a numeric argument -- `float(float)` is identity, `float(int)` an `IntToFloat64`
    /// convert. A non-numeric arg (`float("3.14")`) is dynamic. Handled in the `Call` op.
    FloatCast,
    /// `divmod(a, b)` on two ints -- returns the tuple `(a // b, a % b)`. Lowered in the `Call` op to
    /// a threaded `StackEntry::Tuple` of the floor-quotient and floor-remainder (the same ops `//`
    /// and `%` emit), so `q, r = divmod(a, b)` elides the heap tuple. Float args are dynamic.
    Divmod,
    /// `round(x)` (one arg) -- returns an int. `round(int)` is identity; `round(float)` is
    /// round-half-to-even (`lamella_rint`) then `Float64ToInt`. `round(x, ndigits)` (two args, a float
    /// result) is dynamic. Handled in the `Call` op (it needs the argument's type).
    Round,
}

impl Builtin {
    fn from_name(name: &str) -> Option<Builtin> {
        match name {
            "abs" => Some(Builtin::Abs),
            "min" => Some(Builtin::Min),
            "max" => Some(Builtin::Max),
            "mmio_read8" => Some(Builtin::MmioRead { width: 1 }),
            "mmio_read16" => Some(Builtin::MmioRead { width: 2 }),
            "mmio_read32" => Some(Builtin::MmioRead { width: 4 }),
            "mmio_write8" => Some(Builtin::MmioWrite { width: 1 }),
            "mmio_write16" => Some(Builtin::MmioWrite { width: 2 }),
            "mmio_write32" => Some(Builtin::MmioWrite { width: 4 }),
            "print" => Some(Builtin::Print),
            "int" => Some(Builtin::IntCast),
            "float" => Some(Builtin::FloatCast),
            "divmod" => Some(Builtin::Divmod),
            "round" => Some(Builtin::Round),
            _ => None,
        }
    }

    fn arity(self) -> usize {
        match self {
            Builtin::Abs
            | Builtin::MmioRead { .. }
            | Builtin::Print
            | Builtin::IntCast
            | Builtin::FloatCast
            | Builtin::Round => 1,
            Builtin::Min | Builtin::Max | Builtin::MmioWrite { .. } | Builtin::Divmod => 2,
        }
    }
}

/// One operand-stack slot during abstract interpretation: a typed value; a reference to a
/// callee -- a user function or a built-in -- pushed by `LoadGlobal` and consumed by `Call`
/// (keeping callees on the stack lets nested calls `f(g(x))` resolve); or a threaded `Tuple`
/// of typed values, pushed by `BuildTuple` and consumed by `UnpackSequence`, which lets the
/// `a, b = <exprs>` idiom elide its heap tuple (any other consumer pops it as a plain value
/// via `pop_value` and falls to the dynamic path -- a real tuple object).
enum StackEntry {
    Value(ValueId, MirType),
    Callable(FuncSig),
    Builtin(Builtin),
    Tuple(Vec<(ValueId, MirType)>),
}

fn pop(stack: &mut Vec<StackEntry>) -> Result<StackEntry, LowerError> {
    stack.pop().ok_or(LowerError::StackUnderflow)
}

/// Pop a typed value; a callee here means a function or built-in name used as a plain
/// value, which the typed subset does not support. A `Tuple` entry that was not consumed by
/// an `UnpackSequence` is a real (heap) tuple value -- dynamic in the typed lane.
fn pop_value(stack: &mut Vec<StackEntry>) -> Result<(ValueId, MirType), LowerError> {
    match pop(stack)? {
        StackEntry::Value(v, t) => Ok((v, t)),
        StackEntry::Callable(_) | StackEntry::Builtin(_) => Err(LowerError::CallableAsValue),
        StackEntry::Tuple(_) => Err(LowerError::DynamicOperation),
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_op(
    co: &bc::CodeObject,
    funcs: &BTreeMap<String, FuncSig>,
    constants: &BTreeMap<String, i32>,
    local_ty: &[MirType],
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    locals: &mut [ValueId],
    stack: &mut Vec<StackEntry>,
    op: &bc::Op,
) -> Result<(), LowerError> {
    match op {
        bc::Op::LoadConst(k) => {
            let c = co
                .consts
                .get(*k as usize)
                .ok_or(LowerError::BadConstIndex(*k))?;
            if let bc::Const::Float(bits) = c {
                let id = values.fresh(MirType::F64);
                insts.push((id, Inst::ConstInt {
                    ty: MirType::F64,
                    value: *bits as i64,
                }));
                stack.push(StackEntry::Value(id, MirType::F64));
                return Ok(());
            }
            let value: i64 = match c {
                bc::Const::Int(v) => {
                    if let Ok(n) = i32::try_from(*v) {
                        i64::from(n)
                    } else if let Ok(u) = u32::try_from(*v) {
                        i64::from(u as i32)
                    } else {
                        return Err(LowerError::IntLiteralTooLarge(*v));
                    }
                }
                bc::Const::Bool(b) => i64::from(*b),
                bc::Const::None
                | bc::Const::Str(_)
                | bc::Const::KwNames(_)
                | bc::Const::ArgKinds(_)
                | bc::Const::Imaginary(_)
                | bc::Const::BigInt(_)
                | bc::Const::Bytes(_) => {
                    return Err(LowerError::UnsupportedConst);
                }
                bc::Const::Float(_) => unreachable!("a float constant is materialized above"),
            };
            let id = values.fresh(MirType::I32);
            insts.push((id, Inst::ConstInt {
                ty: MirType::I32,
                value,
            }));
            stack.push(StackEntry::Value(id, MirType::I32));
        }
        bc::Op::LoadFast(i) => {
            let slot = *i as usize;
            let value = *locals.get(slot).ok_or(LowerError::BadLocalIndex(*i))?;
            stack.push(StackEntry::Value(value, local_ty[slot]));
        }
        bc::Op::StoreFast(i) => {
            let slot = *i as usize;
            if slot >= locals.len() {
                return Err(LowerError::BadLocalIndex(*i));
            }
            let (value, _ty) = pop_value(stack)?;
            locals[slot] = value;
        }
        bc::Op::Binary(b) | bc::Op::InplaceBinOp(b) => {
            let (rhs, rt) = pop_value(stack)?;
            let (lhs, lt) = pop_value(stack)?;
            if lt == MirType::F64 || rt == MirType::F64 || matches!(b, bc::BinOp::TrueDiv) {
                let lhs = promote_to_f64(values, insts, lhs, lt)?;
                let rhs = promote_to_f64(values, insts, rhs, rt)?;
                let op = match b {
                    bc::BinOp::Add => MBinOp::Add,
                    bc::BinOp::Sub => MBinOp::Sub,
                    bc::BinOp::Mul => MBinOp::Mul,
                    bc::BinOp::TrueDiv => MBinOp::DivSigned,
                    _ => return Err(LowerError::DynamicOperation),
                };
                let id = emit(values, insts, Inst::Binary { op, lhs, rhs }, MirType::F64);
                stack.push(StackEntry::Value(id, MirType::F64));
                return Ok(());
            }
            if lt != MirType::I32 || rt != MirType::I32 {
                return Err(LowerError::DynamicOperation);
            }
            let id = match b {
                bc::BinOp::Add => emit(values, insts, Inst::Binary {
                    op: MBinOp::Add,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::Sub => emit(values, insts, Inst::Binary {
                    op: MBinOp::Sub,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::Mul => emit(values, insts, Inst::Binary {
                    op: MBinOp::Mul,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::FloorDiv => emit_floor_div(values, insts, lhs, rhs),
                bc::BinOp::Mod => emit_floor_mod(values, insts, lhs, rhs),
                bc::BinOp::BitAnd => emit(values, insts, Inst::Binary {
                    op: MBinOp::And,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::BitOr => emit(values, insts, Inst::Binary {
                    op: MBinOp::Or,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::BitXor => emit(values, insts, Inst::Binary {
                    op: MBinOp::Xor,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::LShift => emit(values, insts, Inst::Binary {
                    op: MBinOp::Shl,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::RShift => emit(values, insts, Inst::Binary {
                    op: MBinOp::ShrSigned,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::TrueDiv => return Err(LowerError::DynamicOperation),
                bc::BinOp::Pow => return Err(LowerError::DynamicOperation),
                bc::BinOp::MatMul => return Err(LowerError::DynamicOperation),
            };
            stack.push(StackEntry::Value(id, MirType::I32));
        }
        bc::Op::Compare(c) => {
            if matches!(c, bc::CmpOp::Is | bc::CmpOp::IsNot) {
                return Err(LowerError::DynamicOperation);
            }
            let (rhs, rt) = pop_value(stack)?;
            let (lhs, lt) = pop_value(stack)?;
            let (lhs, rhs) = if lt == MirType::F64 || rt == MirType::F64 {
                (
                    promote_to_f64(values, insts, lhs, lt)?,
                    promote_to_f64(values, insts, rhs, rt)?,
                )
            } else if lt == MirType::I32 && rt == MirType::I32 {
                (lhs, rhs)
            } else {
                return Err(LowerError::DynamicOperation);
            };
            let id = values.fresh(MirType::I32);
            insts.push((id, Inst::Compare {
                op: map_cmpop(*c),
                lhs,
                rhs,
            }));
            stack.push(StackEntry::Value(id, MirType::I32));
        }
        bc::Op::Unary(u) => {
            let (operand, ty) = pop_value(stack)?;
            if ty == MirType::F64 {
                let id = match u {
                    bc::UnaryOp::Pos => operand,
                    bc::UnaryOp::Neg => {
                        let zero = emit(values, insts, Inst::ConstInt {
                            ty: MirType::F64,
                            value: 0,
                        }, MirType::F64);
                        emit(values, insts, Inst::Binary {
                            op: MBinOp::Sub,
                            lhs: zero,
                            rhs: operand,
                        }, MirType::F64)
                    }
                    bc::UnaryOp::Invert => return Err(LowerError::DynamicOperation),
                };
                stack.push(StackEntry::Value(id, MirType::F64));
                return Ok(());
            }
            if ty != MirType::I32 {
                return Err(LowerError::DynamicOperation);
            }
            let id = match u {
                bc::UnaryOp::Pos => operand,
                bc::UnaryOp::Neg => {
                    let zero = emit(values, insts, Inst::ConstInt {
                        ty: MirType::I32,
                        value: 0,
                    }, MirType::I32);
                    emit(values, insts, Inst::Binary {
                        op: MBinOp::Sub,
                        lhs: zero,
                        rhs: operand,
                    }, MirType::I32)
                }
                bc::UnaryOp::Invert => {
                    let ones = emit(values, insts, Inst::ConstInt {
                        ty: MirType::I32,
                        value: -1,
                    }, MirType::I32);
                    emit(values, insts, Inst::Binary {
                        op: MBinOp::Xor,
                        lhs: operand,
                        rhs: ones,
                    }, MirType::I32)
                }
            };
            stack.push(StackEntry::Value(id, MirType::I32));
        }
        bc::Op::LoadAttr { name, cache } => {
            let (obj, _ot) = pop_value(stack)?;
            let id = values.fresh(MirType::PyValue);
            insts.push((id, Inst::PyIntrinsic {
                op: PyOp::Getattr { name: *name },
                args: vec![obj],
                cache: *cache,
            }));
            stack.push(StackEntry::Value(id, MirType::PyValue));
        }
        bc::Op::Subscript { cache } => {
            let (index, _it) = pop_value(stack)?;
            let (container, _ct) = pop_value(stack)?;
            let id = values.fresh(MirType::PyValue);
            insts.push((id, Inst::PyIntrinsic {
                op: PyOp::Getitem,
                args: vec![container, index],
                cache: *cache,
            }));
            stack.push(StackEntry::Value(id, MirType::PyValue));
        }
        bc::Op::BuildSlice => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::BuildList(_) | bc::Op::BuildDict(_) => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::BuildTuple(n) => {
            let n = *n as usize;
            let mut elems = Vec::with_capacity(n);
            for _ in 0..n {
                elems.push(pop_value(stack)?);
            }
            elems.reverse();
            stack.push(StackEntry::Tuple(elems));
        }
        bc::Op::UnpackSequence(n) => {
            let n = *n as usize;
            match pop(stack)? {
                StackEntry::Tuple(elems) if elems.len() == n => {
                    for (v, t) in elems.into_iter().rev() {
                        stack.push(StackEntry::Value(v, t));
                    }
                }
                _ => return Err(LowerError::DynamicOperation),
            }
        }
        bc::Op::GetIter | bc::Op::ForIter(_) => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::Setitem => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::Contains { .. } => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::Raise(_)
        | bc::Op::MatchExc
        | bc::Op::LoadExc
        | bc::Op::PopExcept
        | bc::Op::Reraise
        | bc::Op::DeleteItem
        | bc::Op::DeleteAttr { .. }
        | bc::Op::DeleteFast(_) => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::MakeFunction { .. }
        | bc::Op::Yield
        | bc::Op::YieldFrom
        | bc::Op::CallEx { .. }
        | bc::Op::BuildClass
        | bc::Op::SetAttr { .. }
        | bc::Op::ListAppend
        | bc::Op::SetAdd
        | bc::Op::DictInsert
        | bc::Op::LoadSuper(_)
        | bc::Op::BuildSet(_)
        | bc::Op::UnpackEx { .. }
        | bc::Op::LoadDeref(_)
        | bc::Op::StoreDeref(_)
        | bc::Op::LoadClosure(_)
        | bc::Op::SetupClassNamespace
        | bc::Op::StoreName(_)
        | bc::Op::LoadName(_)
        | bc::Op::ImportName(_)
        | bc::Op::ImportFrom(_)
        | bc::Op::ImportStar
        | bc::Op::StoreGlobal(_) => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::PopTop => {
            pop(stack)?;
        }
        bc::Op::LoadGlobal(name_idx) => {
            let name = co
                .names
                .get(*name_idx as usize)
                .ok_or(LowerError::BadNameIndex(*name_idx))?;
            if let Some(sig) = funcs.get(name).cloned() {
                stack.push(StackEntry::Callable(sig));
            } else if let Some(builtin) = Builtin::from_name(name) {
                stack.push(StackEntry::Builtin(builtin));
            } else if let Some(&value) = constants.get(name) {
                let id = emit(
                    values,
                    insts,
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: i64::from(value),
                    },
                    MirType::I32,
                );
                stack.push(StackEntry::Value(id, MirType::I32));
            } else {
                return Err(LowerError::UnresolvedGlobal(name.clone()));
            }
        }
        bc::Op::Call(argc) => {
            let argc = *argc as usize;
            let mut typed_args = Vec::with_capacity(argc);
            for _ in 0..argc {
                typed_args.push(pop_value(stack)?);
            }
            typed_args.reverse();
            match pop(stack)? {
                StackEntry::Callable(sig) => {
                    if argc != sig.arity {
                        return Err(LowerError::ArityMismatch {
                            expected: sig.arity,
                            found: argc,
                        });
                    }
                    let mut args = Vec::with_capacity(argc);
                    for ((value, ty), &pty) in typed_args.into_iter().zip(&sig.param_types) {
                        if ty != pty {
                            return Err(LowerError::DynamicOperation);
                        }
                        args.push(value);
                    }
                    let id = values.fresh(sig.ret);
                    insts.push((id, Inst::Call {
                        callee: sig.index,
                        args,
                    }));
                    stack.push(StackEntry::Value(id, sig.ret));
                }
                StackEntry::Builtin(Builtin::Divmod) => {
                    if argc != 2 {
                        return Err(LowerError::ArityMismatch {
                            expected: 2,
                            found: argc,
                        });
                    }
                    let (lhs, lt) = typed_args[0];
                    let (rhs, rt) = typed_args[1];
                    if lt != MirType::I32 || rt != MirType::I32 {
                        return Err(LowerError::DynamicOperation);
                    }
                    let trunc_q = emit(values, insts, Inst::Binary {
                        op: MBinOp::DivSigned,
                        lhs,
                        rhs,
                    }, MirType::I32);
                    let trunc_r = emit(values, insts, Inst::Binary {
                        op: MBinOp::RemSigned,
                        lhs,
                        rhs,
                    }, MirType::I32);
                    let adjust = floor_adjust(values, insts, trunc_r, lhs, rhs);
                    let q = emit(values, insts, Inst::Binary {
                        op: MBinOp::Sub,
                        lhs: trunc_q,
                        rhs: adjust,
                    }, MirType::I32);
                    let adjust_b = emit(values, insts, Inst::Binary {
                        op: MBinOp::Mul,
                        lhs: adjust,
                        rhs,
                    }, MirType::I32);
                    let r = emit(values, insts, Inst::Binary {
                        op: MBinOp::Add,
                        lhs: trunc_r,
                        rhs: adjust_b,
                    }, MirType::I32);
                    stack.push(StackEntry::Tuple(vec![(q, MirType::I32), (r, MirType::I32)]));
                }
                StackEntry::Builtin(Builtin::Abs) => {
                    if argc != 1 {
                        return Err(LowerError::ArityMismatch {
                            expected: 1,
                            found: argc,
                        });
                    }
                    let (value, ty) = typed_args[0];
                    match ty {
                        MirType::I32 => {
                            let (id, rty) = inline_builtin(Builtin::Abs, values, insts, &[value])?;
                            stack.push(StackEntry::Value(id, rty));
                        }
                        MirType::F64 => {
                            let id = emit(values, insts, Inst::PInvoke {
                                import: "lamella_fabs".into(),
                                args: vec![value],
                            }, MirType::F64);
                            stack.push(StackEntry::Value(id, MirType::F64));
                        }
                        _ => return Err(LowerError::DynamicOperation),
                    }
                }
                StackEntry::Builtin(Builtin::Round) => {
                    if argc != 1 {
                        return Err(LowerError::DynamicOperation);
                    }
                    let (value, ty) = typed_args[0];
                    let id = match ty {
                        MirType::I32 => value,
                        MirType::F64 => {
                            let rounded = emit(values, insts, Inst::PInvoke {
                                import: "lamella_rint".into(),
                                args: vec![value],
                            }, MirType::F64);
                            emit(values, insts, Inst::Convert {
                                value: rounded,
                                kind: ConvKind::Float64ToInt,
                            }, MirType::I32)
                        }
                        _ => return Err(LowerError::DynamicOperation),
                    };
                    stack.push(StackEntry::Value(id, MirType::I32));
                }
                StackEntry::Builtin(cast @ (Builtin::IntCast | Builtin::FloatCast)) => {
                    if argc != 1 {
                        return Err(LowerError::ArityMismatch {
                            expected: 1,
                            found: argc,
                        });
                    }
                    let (value, ty) = typed_args[0];
                    let (id, rty) = match (cast, ty) {
                        (Builtin::IntCast, MirType::I32) | (Builtin::FloatCast, MirType::F64) => {
                            (value, ty)
                        }
                        (Builtin::IntCast, MirType::F64) => (
                            emit(values, insts, Inst::Convert {
                                value,
                                kind: ConvKind::Float64ToInt,
                            }, MirType::I32),
                            MirType::I32,
                        ),
                        (Builtin::FloatCast, MirType::I32) => (
                            emit(values, insts, Inst::Convert {
                                value,
                                kind: ConvKind::IntToFloat64,
                            }, MirType::F64),
                            MirType::F64,
                        ),
                        _ => return Err(LowerError::DynamicOperation),
                    };
                    stack.push(StackEntry::Value(id, rty));
                }
                StackEntry::Builtin(builtin) => {
                    let mut args = Vec::with_capacity(argc);
                    for (value, ty) in typed_args {
                        if ty != MirType::I32 {
                            return Err(LowerError::DynamicOperation);
                        }
                        args.push(value);
                    }
                    let (id, ty) = inline_builtin(builtin, values, insts, &args)?;
                    stack.push(StackEntry::Value(id, ty));
                }
                StackEntry::Value(..) | StackEntry::Tuple(_) => {
                    return Err(LowerError::CallTargetNotCallable);
                }
            }
        }
        bc::Op::CallKw { argc, kwnames } => {
            let argc = *argc as usize;
            let names: &[String] = match co.consts.get(*kwnames as usize) {
                Some(bc::Const::KwNames(names)) => names,
                _ => return Err(LowerError::BadConstIndex(*kwnames)),
            };
            let k = names.len();
            let mut kw_values = Vec::with_capacity(k);
            for _ in 0..k {
                let (value, ty) = pop_value(stack)?;
                if ty != MirType::I32 {
                    return Err(LowerError::DynamicOperation);
                }
                kw_values.push(value);
            }
            kw_values.reverse();
            let mut pos_values = Vec::with_capacity(argc);
            for _ in 0..argc {
                let (value, ty) = pop_value(stack)?;
                if ty != MirType::I32 {
                    return Err(LowerError::DynamicOperation);
                }
                pos_values.push(value);
            }
            pos_values.reverse();
            let sig = match pop(stack)? {
                StackEntry::Callable(sig) => sig,
                StackEntry::Builtin(_) | StackEntry::Value(..) | StackEntry::Tuple(_) => {
                    return Err(LowerError::CallTargetNotCallable);
                }
            };
            if argc + k != sig.arity {
                return Err(LowerError::ArityMismatch {
                    expected: sig.arity,
                    found: argc + k,
                });
            }
            let mut bound: Vec<Option<ValueId>> = vec![None; sig.arity];
            for (i, value) in pos_values.into_iter().enumerate() {
                bound[i] = Some(value);
            }
            for (name, value) in names.iter().zip(kw_values) {
                let slot = sig
                    .param_names
                    .iter()
                    .position(|p| p == name)
                    .ok_or_else(|| LowerError::UnexpectedKeyword(name.clone()))?;
                if bound[slot].is_some() {
                    return Err(LowerError::DuplicateArgument(name.clone()));
                }
                bound[slot] = Some(value);
            }
            let args: Vec<ValueId> =
                bound.into_iter().map(|b| b.expect("every param bound")).collect();
            let id = values.fresh(sig.ret);
            insts.push((id, Inst::Call {
                callee: sig.index,
                args,
            }));
            stack.push(StackEntry::Value(id, sig.ret));
        }
        bc::Op::Jump(_) | bc::Op::PopJumpIfFalse(_) | bc::Op::Return => {
            return Err(LowerError::StackNotEmpty);
        }
    }
    Ok(())
}

/// The module-level integer constants a typed function may reference by name: a `NAME = <int
/// literal>` in the module body's LEADING straight-line prefix (so it is unconditionally executed
/// -- always bound, matching the interpreter), assigned exactly once (never reassigned), and
/// fitting a 32-bit machine word. Anything else (computed, reassigned, conditional, or out of
/// range) stays an unresolved global in the typed lane -- conservative, never a wrong or divergent
/// value. Lets a typed AOT driver name a register address (`GPIO = 0x5000_0000`).
fn module_constants(body: &bc::CodeObject) -> BTreeMap<String, i32> {
    let mut store_count = vec![0u32; body.n_locals];
    for op in &body.ops {
        if let bc::Op::StoreFast(s) = op {
            if let Some(c) = store_count.get_mut(*s as usize) {
                *c += 1;
            }
        }
    }
    let mut constants = BTreeMap::new();
    for (i, pair) in body.ops.windows(2).enumerate() {
        if matches!(
            body.ops[i],
            bc::Op::Jump(_) | bc::Op::PopJumpIfFalse(_) | bc::Op::ForIter(_) | bc::Op::Return
        ) {
            break;
        }
        if let [bc::Op::LoadConst(k), bc::Op::StoreFast(s)] = pair {
            let slot = *s as usize;
            if store_count.get(slot).copied() != Some(1) {
                continue;
            }
            let Some(bc::Const::Int(v)) = body.consts.get(*k as usize) else {
                continue;
            };
            let bits = match i32::try_from(*v) {
                Ok(n) => n,
                Err(_) => match u32::try_from(*v) {
                    Ok(u) => u as i32,
                    Err(_) => continue,
                },
            };
            if let Some(name) = body.local_names.get(slot) {
                constants.insert(name.clone(), bits);
            }
        }
    }
    constants
}

/// Lower every function of a compiled module, returning each `(name, Function)`.
/// The `<module>` body is not lowered in the typed path: the parity harness drives
/// the call boundary.
pub fn lower_module(module: &bc::Module) -> Result<Vec<(String, Function)>, LowerError> {
    let funcs: BTreeMap<String, FuncSig> = module
        .functions
        .iter()
        .enumerate()
        .map(|(i, co)| {
            (co.name.clone(), FuncSig {
                index: i as u32,
                ret: mir_type(co.ret_ty),
                arity: co.params.len(),
                param_names: co.params.iter().map(|p| p.name.clone()).collect(),
                param_types: co.params.iter().map(|p| mir_type(p.ty)).collect(),
            })
        })
        .collect();
    let constants = module_constants(&module.body);
    module
        .functions
        .iter()
        .map(|co| Ok((co.name.clone(), lower_function(co, &funcs, &constants)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_str;
    use alloc::format;

    fn lower_named(source: &str, name: &str) -> Function {
        let module = compile_str("test", source).expect("compiles");
        let lowered = lower_module(&module).expect("lowers");
        let func = lowered
            .into_iter()
            .find(|(n, _)| n == name)
            .expect("function present")
            .1;
        assert_eq!(
            lamella_ir::verify(&func),
            Ok(()),
            "lowered function must verify: {}",
            describe(&func)
        );
        func
    }

    /// Lower a whole module, returning every `(name, Function)` for multi-function and
    /// call-resolution tests.
    fn lower_all(source: &str) -> Vec<(String, Function)> {
        let module = compile_str("test", source).expect("compiles");
        let lowered = lower_module(&module).expect("lowers");
        for (name, func) in &lowered {
            assert_eq!(
                lamella_ir::verify(func),
                Ok(()),
                "function `{name}` must verify: {}",
                describe(func)
            );
        }
        lowered
    }

    fn describe(func: &Function) -> String {
        format!(
            "{} blocks, {} values",
            func.blocks.len(),
            func.value_types.len()
        )
    }

    fn count_insts(func: &Function, pred: impl Fn(&Inst) -> bool) -> usize {
        func.blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .filter(|(_, inst)| pred(inst))
            .count()
    }

    const FIB: &str = "\
def fib(n: int) -> int:
    a: int = 0
    b: int = 1
    i: int = 0
    while i < n:
        t: int = a + b
        a = b
        b = t
        i = i + 1
    return a
";

    #[test]
    fn typed_fib_lowers_and_verifies() {
        let func = lower_named(FIB, "fib");
        assert_eq!(func.params, vec![MirType::I32]);
        assert_eq!(func.ret, Some(MirType::I32));
        let has_branch = func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Branch { .. })));
        let has_jump = func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Jump { .. })));
        assert!(has_branch && has_jump);
        assert!(count_insts(&func, |i| matches!(i, Inst::Binary { .. })) >= 2);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Compare { .. })), 1);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn typed_float_arithmetic_lowers_and_verifies() {
        let func = lower_named(
            "\
def poly(x: float, y: float) -> float:
    return x * x + 2.0 * y - 1.0
",
            "poly",
        );
        assert_eq!(func.params, vec![MirType::F64, MirType::F64]);
        assert_eq!(func.ret, Some(MirType::F64));
        assert!(count_insts(&func, |i| matches!(i, Inst::Binary { .. })) >= 3);
        assert!(
            count_insts(&func, |i| matches!(i, Inst::ConstInt {
                ty: MirType::F64,
                ..
            })) >= 2
        );
    }

    #[test]
    fn true_division_promotes_int_operands_to_f64() {
        let func = lower_named(
            "\
def half(n: int) -> float:
    return n / 2
",
            "half",
        );
        assert_eq!(func.ret, Some(MirType::F64));
        assert_eq!(
            count_insts(&func, |i| matches!(i, Inst::Convert {
                kind: ConvKind::IntToFloat64,
                ..
            })),
            2
        );
    }

    #[test]
    fn float_unary_negation_lowers() {
        let func = lower_named(
            "\
def flip(x: float) -> float:
    return -x
",
            "flip",
        );
        assert_eq!(func.ret, Some(MirType::F64));
        assert!(count_insts(&func, |i| matches!(i, Inst::Binary {
            op: MBinOp::Sub,
            ..
        })) >= 1);
    }

    #[test]
    fn floor_division_and_modulo_lower_with_a_sign_correction() {
        let div = lower_named("def f(a: int, b: int) -> int:\n    return a // b\n", "f");
        assert_eq!(
            count_insts(&div, |i| matches!(i, Inst::Binary {
                op: MBinOp::DivSigned,
                ..
            })),
            1
        );
        assert!(count_insts(&div, |i| matches!(i, Inst::Binary {
            op: MBinOp::RemSigned,
            ..
        })) >= 1);
        assert_eq!(count_insts(&div, |i| matches!(i, Inst::Compare { .. })), 2);

        let modulo = lower_named("def g(a: int, b: int) -> int:\n    return a % b\n", "g");
        assert!(count_insts(&modulo, |i| matches!(i, Inst::Binary {
            op: MBinOp::RemSigned,
            ..
        })) >= 1);
        assert_eq!(count_insts(&modulo, |i| matches!(i, Inst::Compare { .. })), 2);
    }

    #[test]
    fn minimal_ssa_keeps_the_value_count_small() {
        let func = lower_named(FIB, "fib");
        assert!(
            func.value_types.len() < 15,
            "fib lowered to {} values, expected a minimal-SSA + liveness reduction",
            func.value_types.len()
        );
        let param_blocks = func.blocks.iter().filter(|b| !b.params.is_empty()).count();
        assert_eq!(param_blocks, 2);
        let max_params = func.blocks.iter().map(|b| b.params.len()).max().unwrap();
        assert_eq!(max_params, 4);
    }

    #[test]
    fn dynamic_getattr_lowers_to_a_py_intrinsic() {
        let func = lower_named("def get_x(obj):\n    return obj.x\n", "get_x");
        assert_eq!(func.params, vec![MirType::PyValue]);
        assert_eq!(func.ret, Some(MirType::PyValue));
        let getattrs = count_insts(&func, |i| {
            matches!(i, Inst::PyIntrinsic { op: PyOp::Getattr { name: _ }, .. })
        });
        assert_eq!(getattrs, 1);
    }

    #[test]
    fn straight_line_typed_function() {
        let func = lower_named("def inc(n: int) -> int:\n    return n + 1\n", "inc");
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Return(Some(_))))));
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Unreachable))));
    }

    #[test]
    fn if_else_lowers_and_verifies() {
        let src = "\
def sign(n: int) -> int:
    if n < 0:
        return 0
    else:
        return 1
";
        let func = lower_named(src, "sign");
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Branch { .. }))));
    }

    #[test]
    fn loop_first_with_a_body_local_verifies() {
        let func = lower_named(
            "def f(n: int) -> int:\n    while n > 0:\n        x: int = n\n        n = n - x\n    return n\n",
            "f",
        );
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Branch { .. }))));
    }

    #[test]
    fn a_call_to_a_helper_lowers_to_inst_call() {
        let funcs = lower_all(
            "def inc(x: int) -> int:\n    return x + 1\n\
             def main() -> int:\n    return inc(41)\n",
        );
        let main = funcs.iter().find(|(n, _)| n == "main").unwrap().1.clone();
        let inc_index = funcs.iter().position(|(n, _)| n == "inc").unwrap() as u32;
        let calls: Vec<(u32, usize)> = main
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .filter_map(|(_, i)| match i {
                Inst::Call { callee, args } => Some((*callee, args.len())),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec![(inc_index, 1)]);
    }

    #[test]
    fn direct_recursion_resolves_to_the_callee_itself() {
        let func = lower_named(
            "def fib(n: int) -> int:\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\n",
            "fib",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 2);
    }

    #[test]
    fn nested_calls_resolve_inside_out() {
        let func = lower_named(
            "def g(x: int) -> int:\n    return x * 2\n\
             def f(x: int) -> int:\n    return x + 1\n\
             def main(n: int) -> int:\n    return f(g(n))\n",
            "main",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 2);
    }

    #[test]
    fn a_multi_argument_call_carries_every_argument() {
        let func = lower_named(
            "def add3(a: int, b: int, c: int) -> int:\n    return a + b + c\n\
             def main() -> int:\n    return add3(1, 2, 3)\n",
            "main",
        );
        let argc = func
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .find_map(|(_, i)| match i {
                Inst::Call { args, .. } => Some(args.len()),
                _ => None,
            });
        assert_eq!(argc, Some(3));
    }

    #[test]
    fn an_arity_mismatch_is_rejected() {
        let module = compile_str(
            "test",
            "def one(x: int) -> int:\n    return x\n\
             def main() -> int:\n    return one(1, 2)\n",
        )
        .expect("compiles");
        assert_eq!(
            lower_module(&module),
            Err(LowerError::ArityMismatch {
                expected: 1,
                found: 2,
            })
        );
    }

    #[test]
    fn an_unknown_global_is_rejected() {
        let module = compile_str("test", "def main() -> int:\n    return nope(1)\n")
            .expect("compiles");
        assert!(matches!(
            lower_module(&module),
            Err(LowerError::UnresolvedGlobal(_))
        ));
    }

    #[test]
    fn a_function_used_as_a_value_is_rejected() {
        let module = compile_str(
            "test",
            "def inc(x: int) -> int:\n    return x\n\
             def main() -> int:\n    return inc + 1\n",
        )
        .expect("compiles");
        assert_eq!(lower_module(&module), Err(LowerError::CallableAsValue));
    }

    #[test]
    fn bitwise_operators_lower_to_their_mir_ops() {
        for (src_op, mir_op) in [
            ("&", MBinOp::And),
            ("|", MBinOp::Or),
            ("^", MBinOp::Xor),
            ("<<", MBinOp::Shl),
            (">>", MBinOp::ShrSigned),
        ] {
            let src = format!("def f(a: int, b: int) -> int:\n    return a {src_op} b\n");
            let func = lower_named(&src, "f");
            assert_eq!(
                count_insts(&func, |i| matches!(i, Inst::Binary { op, .. } if *op == mir_op)),
                1,
                "operator `{src_op}` should lower to one {mir_op:?}"
            );
        }
    }

    #[test]
    fn bitwise_precedence_follows_python() {
        let module =
            compile_str("test", "def f() -> int:\n    return 1 | 2 & 3\n").expect("compiles");
        let binops: Vec<bc::BinOp> = module.functions[0]
            .ops
            .iter()
            .filter_map(|o| match o {
                bc::Op::Binary(b) => Some(*b),
                _ => None,
            })
            .collect();
        assert_eq!(binops, vec![bc::BinOp::BitAnd, bc::BinOp::BitOr]);
    }

    #[test]
    fn unary_operators_lower_for_typed_ints() {
        let neg = lower_named("def f(x: int) -> int:\n    return -x\n", "f");
        assert_eq!(
            count_insts(&neg, |i| matches!(i, Inst::Binary {
                op: MBinOp::Sub,
                ..
            })),
            1
        );
        let inv = lower_named("def f(x: int) -> int:\n    return ~x\n", "f");
        assert_eq!(
            count_insts(&inv, |i| matches!(i, Inst::Binary {
                op: MBinOp::Xor,
                ..
            })),
            1
        );
        let pos = lower_named("def f(x: int) -> int:\n    return +x\n", "f");
        assert_eq!(count_insts(&pos, |i| matches!(i, Inst::Binary { .. })), 0);
    }

    #[test]
    fn unary_on_a_literal_folds_but_on_a_variable_emits_an_op() {
        let var = compile_str("test", "def f(x: int) -> int:\n    return ~x\n").expect("compiles");
        assert!(var.functions[0]
            .ops
            .iter()
            .any(|o| matches!(o, bc::Op::Unary(bc::UnaryOp::Invert))));
        let lit = compile_str("test", "def g() -> int:\n    return ~3\n").expect("compiles");
        assert!(lit.functions[0].consts.contains(&bc::Const::Int(!3)));
        assert!(!lit.functions[0]
            .ops
            .iter()
            .any(|o| matches!(o, bc::Op::Unary(_))));
    }

    #[test]
    fn a_nested_boolean_threads_the_stack_and_verifies() {
        let func = lower_named(
            "def f(a: int, b: int) -> int:\n    return 10 + (a and b)\n",
            "f",
        );
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Branch { .. }))));
    }

    #[test]
    fn builtins_inline_without_a_call() {
        let abs = lower_named("def f(x: int) -> int:\n    return abs(x)\n", "f");
        assert_eq!(count_insts(&abs, |i| matches!(i, Inst::Call { .. })), 0);
        assert!(count_insts(&abs, |i| matches!(i, Inst::Binary {
            op: MBinOp::ShrSigned,
            ..
        })) >= 1);
        let mx = lower_named("def f(a: int, b: int) -> int:\n    return max(a, b)\n", "f");
        assert_eq!(count_insts(&mx, |i| matches!(i, Inst::Call { .. })), 0);
    }

    #[test]
    fn subscript_lowers_to_a_getitem_intrinsic() {
        let func = lower_named("def f(s, i):\n    return s[i]\n", "f");
        assert_eq!(
            count_insts(&func, |i| matches!(
                i,
                Inst::PyIntrinsic {
                    op: PyOp::Getitem,
                    ..
                }
            )),
            1
        );
    }

    #[test]
    fn a_builtin_arity_mismatch_is_rejected() {
        let module = compile_str("test", "def f(x: int) -> int:\n    return abs(x, x)\n")
            .expect("compiles");
        assert!(matches!(
            lower_module(&module),
            Err(LowerError::ArityMismatch { .. })
        ));
    }

    #[test]
    fn mmio_write_and_read_lower_to_volatile_store_and_load() {
        let func = lower_named(
            "def poke(addr: int, val: int) -> int:\n    mmio_write32(addr, val)\n    return mmio_read32(addr)\n",
            "poke",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Store { width: 4, .. })), 1);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Load { width: 4, .. })), 1);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 0);
    }

    #[test]
    fn mmio_widths_map_to_store_and_load_byte_widths() {
        for (bytes, read, write) in [
            (1u32, "mmio_read8", "mmio_write8"),
            (2, "mmio_read16", "mmio_write16"),
            (4, "mmio_read32", "mmio_write32"),
        ] {
            let src =
                format!("def f(a: int, v: int) -> int:\n    {write}(a, v)\n    return {read}(a)\n");
            let func = lower_named(&src, "f");
            assert_eq!(
                count_insts(&func, |i| matches!(i, Inst::Store { width, .. } if *width == bytes)),
                1,
                "{write} should store {bytes} byte(s)"
            );
            assert_eq!(
                count_insts(&func, |i| matches!(i, Inst::Load { width, .. } if *width == bytes)),
                1,
                "{read} should load {bytes} byte(s)"
            );
        }
    }

    #[test]
    fn an_mmio_read_result_is_a_usable_typed_int() {
        let func = lower_named(
            "def f(a: int) -> int:\n    x = mmio_read32(a)\n    return x + 1\n",
            "f",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Load { width: 4, .. })), 1);
        assert_eq!(
            count_insts(&func, |i| matches!(i, Inst::Binary {
                op: MBinOp::Add,
                ..
            })),
            1
        );
    }

    #[test]
    fn an_mmio_write_result_cannot_be_used_as_an_int() {
        let module = compile_str(
            "test",
            "def f(a: int) -> int:\n    return mmio_write32(a, 1) + 1\n",
        )
        .expect("compiles");
        assert_eq!(lower_module(&module), Err(LowerError::DynamicOperation));
    }

    #[test]
    fn a_u32_peripheral_address_lowers_as_a_32_bit_word() {
        let direct = lower_named(
            "def read_systick() -> int:\n    return mmio_read32(0xE000E010)\n",
            "read_systick",
        );
        assert_eq!(count_insts(&direct, |i| matches!(i, Inst::Load { width: 4, .. })), 1);
        let want = 0xE000_E010_u32 as i32 as i64;
        assert_eq!(
            count_insts(&direct, |i| matches!(i, Inst::ConstInt { value, .. } if *value == want)),
            1
        );

        let via_local = lower_named(
            "def enable() -> int:\n    ctrl = 0xE000E010\n    mmio_write32(ctrl, 0xDEADBEEF)\n    return 0\n",
            "enable",
        );
        assert_eq!(count_insts(&via_local, |i| matches!(i, Inst::Store { width: 4, .. })), 1);
    }

    #[test]
    fn an_int_literal_beyond_32_bits_is_still_rejected() {
        let module =
            compile_str("test", "def f() -> int:\n    return 4294967296\n").expect("compiles");
        assert_eq!(
            lower_module(&module),
            Err(LowerError::IntLiteralTooLarge(4294967296))
        );
    }

    #[test]
    fn a_keyword_call_static_binds_to_parameter_order() {
        let funcs = lower_all(
            "def sub(a: int, b: int) -> int:\n    return a - b\n\
             def main() -> int:\n    return sub(b=3, a=10)\n",
        );
        let main = funcs.iter().find(|(n, _)| n == "main").unwrap().1.clone();
        let sub_index = funcs.iter().position(|(n, _)| n == "sub").unwrap() as u32;
        let const_of = |id: ValueId| -> Option<i64> {
            main.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .find_map(|(vid, inst)| match inst {
                    Inst::ConstInt { value, .. } if *vid == id => Some(*value),
                    _ => None,
                })
        };
        let args = main
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .find_map(|(_, i)| match i {
                Inst::Call { callee, args } if *callee == sub_index => Some(args.clone()),
                _ => None,
            })
            .expect("a Call to sub");
        assert_eq!(args.len(), 2);
        assert_eq!(const_of(args[0]), Some(10));
        assert_eq!(const_of(args[1]), Some(3));
    }

    #[test]
    fn a_keyword_call_rejects_unknown_and_duplicate() {
        let unknown = compile_str(
            "test",
            "def f(a: int) -> int:\n    return a\n\
             def main() -> int:\n    return f(z=1)\n",
        )
        .expect("compiles");
        assert_eq!(
            lower_module(&unknown),
            Err(LowerError::UnexpectedKeyword(String::from("z")))
        );

        let dup = compile_str(
            "test",
            "def g(a: int, b: int) -> int:\n    return a\n\
             def main() -> int:\n    return g(1, a=2)\n",
        )
        .expect("compiles");
        assert_eq!(
            lower_module(&dup),
            Err(LowerError::DuplicateArgument(String::from("a")))
        );
    }

    #[test]
    fn a_module_level_int_constant_is_usable_in_a_typed_function() {
        let funcs = lower_all(
            "SYSTICK = 0xE000E010\n\
             def read() -> int:\n    return mmio_read32(SYSTICK)\n",
        );
        let read = funcs.iter().find(|(n, _)| n == "read").unwrap().1.clone();
        let want = 0xE000_E010_u32 as i32 as i64;
        assert_eq!(
            count_insts(&read, |i| matches!(i, Inst::ConstInt { value, .. } if *value == want)),
            1
        );
        assert_eq!(count_insts(&read, |i| matches!(i, Inst::Load { width: 4, .. })), 1);
    }

    #[test]
    fn a_reassigned_module_name_is_not_a_constant() {
        let module =
            compile_str("test", "x = 5\nx = 6\ndef f() -> int:\n    return x\n").expect("compiles");
        assert!(matches!(
            lower_module(&module),
            Err(LowerError::UnresolvedGlobal(_))
        ));
    }

    #[test]
    fn a_parallel_assignment_lowers_fully_typed_with_no_heap_tuple() {
        let func = lower_named(
            "def f() -> int:\n    a, b = 1, 2\n    a, b = b, a\n    return a * 10 + b\n",
            "f",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 0);
    }

    #[test]
    fn a_swap_is_pure_value_threading() {
        let func = lower_named(
            "def f(a: int, b: int) -> int:\n    a, b = b, a\n    return a - b\n",
            "f",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Binary { .. })), 1);
    }

    #[test]
    fn fib_via_parallel_assignment_lowers_and_verifies() {
        let func = lower_named(
            "def fib(n: int) -> int:\n    a, b = 0, 1\n    i: int = 0\n    while i < n:\n        a, b = b, a + b\n        i = i + 1\n    return a\n",
            "fib",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Branch { .. }))));
    }

    #[test]
    fn a_returned_tuple_falls_to_the_dynamic_path() {
        let module = compile_str("test", "def f() -> int:\n    t = (1, 2)\n    return t[0]\n")
            .expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn a_tuple_arity_mismatch_falls_to_the_dynamic_path() {
        let module = compile_str("test", "def f() -> int:\n    a, b, c = 1, 2\n    return a\n")
            .expect("compiles");
        assert_eq!(lower_module(&module), Err(LowerError::DynamicOperation));
    }

    #[test]
    fn print_of_an_int_lowers_to_a_semihosting_write() {
        let func = lower_named(
            "def f(x: int) -> int:\n    print(x)\n    print(x + 1)\n    return 0\n",
            "f",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::WriteInt { .. })), 2);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 0);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn print_of_the_wrong_arity_or_a_keyword_falls_to_the_dynamic_path() {
        let two =
            compile_str("test", "def f(a: int, b: int) -> int:\n    print(a, b)\n    return 0\n")
                .expect("compiles");
        assert!(lower_module(&two).is_err());
        let kw = compile_str("test", "def f(a: int) -> int:\n    print(a, end=\"\")\n    return 0\n")
            .expect("compiles");
        assert!(lower_module(&kw).is_err());
    }

    #[test]
    fn int_and_float_conversions_lower_to_converts() {
        let to_int = lower_named("def f(x: float) -> int:\n    return int(x)\n", "f");
        assert_eq!(
            count_insts(&to_int, |i| matches!(i, Inst::Convert {
                kind: ConvKind::Float64ToInt,
                ..
            })),
            1
        );
        let to_float = lower_named("def f(n: int) -> float:\n    return float(n)\n", "f");
        assert_eq!(
            count_insts(&to_float, |i| matches!(i, Inst::Convert {
                kind: ConvKind::IntToFloat64,
                ..
            })),
            1
        );
    }

    #[test]
    fn a_same_type_conversion_is_identity() {
        let ii = lower_named("def f(n: int) -> int:\n    return int(n)\n", "f");
        assert_eq!(count_insts(&ii, |i| matches!(i, Inst::Convert { .. })), 0);
        let ff = lower_named("def f(x: float) -> float:\n    return float(x)\n", "f");
        assert_eq!(count_insts(&ff, |i| matches!(i, Inst::Convert { .. })), 0);
    }

    #[test]
    fn a_conversion_of_a_non_numeric_falls_to_the_dynamic_path() {
        let module =
            compile_str("test", "def f() -> int:\n    return int(\"5\")\n").expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn divmod_lowers_to_a_threaded_tuple_with_no_heap_tuple_or_call() {
        let func = lower_named(
            "def f(a: int, b: int) -> int:\n    q, r = divmod(a, b)\n    return q * 100 + r\n",
            "f",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 0);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
        assert_eq!(
            count_insts(&func, |i| matches!(i, Inst::Binary {
                op: MBinOp::DivSigned,
                ..
            })),
            1
        );
        assert_eq!(
            count_insts(&func, |i| matches!(i, Inst::Binary {
                op: MBinOp::RemSigned,
                ..
            })),
            1
        );
    }

    #[test]
    fn divmod_on_floats_falls_to_the_dynamic_path() {
        let module = compile_str("test", "def f(a: float, b: float) -> int:\n    q, r = divmod(a, b)\n    return int(q)\n")
            .expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn a_divmod_result_not_unpacked_falls_to_the_dynamic_path() {
        let module = compile_str("test", "def f(a: int, b: int) -> int:\n    t = divmod(a, b)\n    return t[0]\n")
            .expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn abs_of_a_float_lowers_to_a_fabs_pinvoke() {
        let ff = lower_named("def f(x: float) -> float:\n    return abs(x)\n", "f");
        assert_eq!(
            count_insts(&ff, |i| matches!(i, Inst::PInvoke { import, .. } if &**import == "lamella_fabs")),
            1
        );
        assert_eq!(count_insts(&ff, |i| matches!(i, Inst::Call { .. })), 0);
        let fi = lower_named("def f(x: int) -> int:\n    return abs(x)\n", "f");
        assert_eq!(count_insts(&fi, |i| matches!(i, Inst::PInvoke { .. })), 0);
        assert!(count_insts(&fi, |i| matches!(i, Inst::Binary {
            op: MBinOp::ShrSigned,
            ..
        })) >= 1);
    }

    #[test]
    fn round_of_a_float_lowers_to_rint_then_to_int() {
        let rf = lower_named("def f(x: float) -> int:\n    return round(x)\n", "f");
        assert_eq!(
            count_insts(&rf, |i| matches!(i, Inst::PInvoke { import, .. } if &**import == "lamella_rint")),
            1
        );
        assert_eq!(
            count_insts(&rf, |i| matches!(i, Inst::Convert {
                kind: ConvKind::Float64ToInt,
                ..
            })),
            1
        );
        let ri = lower_named("def f(x: int) -> int:\n    return round(x)\n", "f");
        assert_eq!(count_insts(&ri, |i| matches!(i, Inst::PInvoke { .. })), 0);
        assert_eq!(count_insts(&ri, |i| matches!(i, Inst::Convert { .. })), 0);
    }
}
