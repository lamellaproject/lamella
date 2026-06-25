//! The typed `Python -> MIR` lowering: a [`bc::CodeObject`] to a
//! [`lamella_ir::Function`].


use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use lamella_ir::{
    BasicBlock, BinOp as MBinOp, BlockId, CmpOp as MCmpOp, Function, Inst, MirType, PyOp,
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
    /// An integer literal did not fit the typed integer (`i32`).
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
                write!(f, "integer literal {v} does not fit the typed i32")
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
        bc::StaticType::Dynamic => MirType::PyValue,
    }
}

/// Emit one instruction of type `ty`, returning its result value.
fn emit(values: &mut Values, insts: &mut Vec<(ValueId, Inst)>, inst: Inst, ty: MirType) -> ValueId {
    let id = values.fresh(ty);
    insts.push((id, inst));
    id
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
    }
}

/// Inline a built-in over typed integer arguments, returning the result. All are
/// branchless: `abs(x)` = `(x ^ (x>>31)) - (x>>31)`; `min`/`max` select via the
/// `(a ^ b) & -(a < b)` mask. (The interpreter provides the same built-ins for the
/// dynamic path; here the typed path needs no runtime call.)
fn inline_builtin(
    builtin: Builtin,
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    args: &[ValueId],
) -> Result<ValueId, LowerError> {
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
            emit(values, insts, Inst::Binary {
                op: MBinOp::Sub,
                lhs: flipped,
                rhs: mask,
            }, MirType::I32)
        }
        Builtin::Min => emit_select_extreme(values, insts, args[0], args[1], args[1]),
        Builtin::Max => emit_select_extreme(values, insts, args[0], args[1], args[0]),
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
        if ty != MirType::I32 {
            return Err(LowerError::UnsupportedLocalType(i));
        }
        let zero = values.fresh(MirType::I32);
        synth_insts.push((zero, Inst::ConstInt {
            ty: MirType::I32,
            value: 0,
        }));
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
            lower_op(co, funcs, &local_ty, &mut values, &mut insts, &mut locals, &mut stack, op)?;
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
/// module (the `Inst::Call` callee), its MIR return type, and its parameter count (for
/// the arity check). The index is module-relative; a driver that prepends functions
/// (e.g. an AOT entry shim) offsets it.
#[derive(Clone, Copy)]
struct FuncSig {
    index: u32,
    ret: MirType,
    arity: usize,
}

/// A built-in function the typed path inlines with no runtime call: `abs`, `min`,
/// `max` over integers. Other built-ins (`len`, container/string operations) dispatch
/// to the runtime and arrive with the dynamic surface.
#[derive(Clone, Copy)]
enum Builtin {
    Abs,
    Min,
    Max,
}

impl Builtin {
    fn from_name(name: &str) -> Option<Builtin> {
        match name {
            "abs" => Some(Builtin::Abs),
            "min" => Some(Builtin::Min),
            "max" => Some(Builtin::Max),
            _ => None,
        }
    }

    fn arity(self) -> usize {
        match self {
            Builtin::Abs => 1,
            Builtin::Min | Builtin::Max => 2,
        }
    }
}

/// One operand-stack slot during abstract interpretation: a typed value, or a reference
/// to a callee -- a user function or a built-in -- pushed by `LoadGlobal` and consumed
/// by `Call`. Keeping callees on the stack lets nested calls (`f(g(x))`) resolve.
enum StackEntry {
    Value(ValueId, MirType),
    Callable(FuncSig),
    Builtin(Builtin),
}

fn pop(stack: &mut Vec<StackEntry>) -> Result<StackEntry, LowerError> {
    stack.pop().ok_or(LowerError::StackUnderflow)
}

/// Pop a typed value; a callee here means a function or built-in name used as a plain
/// value, which the typed subset does not support.
fn pop_value(stack: &mut Vec<StackEntry>) -> Result<(ValueId, MirType), LowerError> {
    match pop(stack)? {
        StackEntry::Value(v, t) => Ok((v, t)),
        StackEntry::Callable(_) | StackEntry::Builtin(_) => Err(LowerError::CallableAsValue),
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_op(
    co: &bc::CodeObject,
    funcs: &BTreeMap<String, FuncSig>,
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
            let value: i64 = match c {
                bc::Const::Int(v) => {
                    i64::from(i32::try_from(*v).map_err(|_| LowerError::IntLiteralTooLarge(*v))?)
                }
                bc::Const::Bool(b) => i64::from(*b),
                bc::Const::None | bc::Const::Str(_) => return Err(LowerError::UnsupportedConst),
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
        bc::Op::Binary(b) => {
            let (rhs, rt) = pop_value(stack)?;
            let (lhs, lt) = pop_value(stack)?;
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
            };
            stack.push(StackEntry::Value(id, MirType::I32));
        }
        bc::Op::Compare(c) => {
            let (rhs, rt) = pop_value(stack)?;
            let (lhs, lt) = pop_value(stack)?;
            if lt != MirType::I32 || rt != MirType::I32 {
                return Err(LowerError::DynamicOperation);
            }
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
        bc::Op::PopTop => {
            pop(stack)?;
        }
        bc::Op::LoadGlobal(name_idx) => {
            let name = co
                .names
                .get(*name_idx as usize)
                .ok_or(LowerError::BadNameIndex(*name_idx))?;
            if let Some(sig) = funcs.get(name).copied() {
                stack.push(StackEntry::Callable(sig));
            } else if let Some(builtin) = Builtin::from_name(name) {
                stack.push(StackEntry::Builtin(builtin));
            } else {
                return Err(LowerError::UnresolvedGlobal(name.clone()));
            }
        }
        bc::Op::Call(argc) => {
            let argc = *argc as usize;
            let mut args = Vec::with_capacity(argc);
            for _ in 0..argc {
                let (value, ty) = pop_value(stack)?;
                if ty != MirType::I32 {
                    return Err(LowerError::DynamicOperation);
                }
                args.push(value);
            }
            args.reverse();
            match pop(stack)? {
                StackEntry::Callable(sig) => {
                    if argc != sig.arity {
                        return Err(LowerError::ArityMismatch {
                            expected: sig.arity,
                            found: argc,
                        });
                    }
                    let id = values.fresh(sig.ret);
                    insts.push((id, Inst::Call {
                        callee: sig.index,
                        args,
                    }));
                    stack.push(StackEntry::Value(id, sig.ret));
                }
                StackEntry::Builtin(builtin) => {
                    let id = inline_builtin(builtin, values, insts, &args)?;
                    stack.push(StackEntry::Value(id, MirType::I32));
                }
                StackEntry::Value(..) => return Err(LowerError::CallTargetNotCallable),
            }
        }
        bc::Op::Jump(_) | bc::Op::PopJumpIfFalse(_) | bc::Op::Return => {
            return Err(LowerError::StackNotEmpty);
        }
    }
    Ok(())
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
            })
        })
        .collect();
    module
        .functions
        .iter()
        .map(|co| Ok((co.name.clone(), lower_function(co, &funcs)?)))
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
}
