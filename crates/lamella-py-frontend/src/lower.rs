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
/// outside the first-light typed subset rather than a true error.
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
    /// An integer literal did not fit first light's `i32` typed integer.
    IntLiteralTooLarge(i64),
    /// A non-integer constant in the typed path (`None`/`True`/`False`/string); not
    /// yet lowered for first light.
    UnsupportedConst,
    /// A binary operator outside first light's typed integer set (`//`, `%`): floor
    /// division and modulo need sign-correct lowering, deferred.
    UnsupportedBinOp(bc::BinOp),
    /// Arithmetic or comparison on a dynamic (non-`I32`) operand; first light's
    /// dynamic surface is attribute access only.
    DynamicOperation,
    /// An opcode deferred for the first-light parity slice (`Call`, `LoadGlobal`).
    Deferred(&'static str),
    /// A conditional branch's condition was not an `I32`.
    BadConditionType,
    /// A `return` value's type did not match the function's return type.
    ReturnTypeMismatch,
    /// Control fell off the end of the body (a function body that does not return).
    RunsOffEnd,
    /// A non-parameter local was not an integer, so it has no first-light default to
    /// initialize it with at entry.
    UnsupportedLocalType(usize),
}

impl core::fmt::Display for LowerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LowerError::StackUnderflow => f.write_str("operand stack underflow"),
            LowerError::StackNotEmpty => f.write_str("operand stack not empty at a block boundary"),
            LowerError::BadConstIndex(i) => write!(f, "constant index {i} out of range"),
            LowerError::BadLocalIndex(i) => write!(f, "local index {i} out of range"),
            LowerError::IntLiteralTooLarge(v) => {
                write!(f, "integer literal {v} does not fit first light's i32")
            }
            LowerError::UnsupportedConst => {
                f.write_str("non-integer constant is not lowered for first light")
            }
            LowerError::UnsupportedBinOp(op) => {
                write!(f, "binary operator {op:?} is not lowered for first light")
            }
            LowerError::DynamicOperation => {
                f.write_str("arithmetic/comparison on a dynamic value is out of the first-light subset")
            }
            LowerError::Deferred(op) => write!(f, "{op} is deferred past the first-light parity slice"),
            LowerError::BadConditionType => f.write_str("a branch condition was not an i32"),
            LowerError::ReturnTypeMismatch => {
                f.write_str("a return value's type did not match the function's return type")
            }
            LowerError::RunsOffEnd => f.write_str("control runs off the end of the function body"),
            LowerError::UnsupportedLocalType(i) => {
                write!(f, "local slot {i} has no first-light default initializer")
            }
        }
    }
}

/// The MIR type a first-light Python type lowers to: an annotated `int` is a
/// machine `I32` (bignum overflow deferred); anything dynamic is a tagged `PyValue`.
fn mir_type(ty: bc::StaticType) -> MirType {
    match ty {
        bc::StaticType::Int => MirType::I32,
        bc::StaticType::Dynamic => MirType::PyValue,
    }
}

fn map_binop(op: bc::BinOp) -> Result<MBinOp, LowerError> {
    match op {
        bc::BinOp::Add => Ok(MBinOp::Add),
        bc::BinOp::Sub => Ok(MBinOp::Sub),
        bc::BinOp::Mul => Ok(MBinOp::Mul),
        bc::BinOp::FloorDiv | bc::BinOp::Mod => Err(LowerError::UnsupportedBinOp(op)),
    }
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

/// Lower one code object (a function or the `<module>` body) to a verified
/// [`Function`]. The caller hands the verified functions to the backend's
/// `lower_module_py`.
pub fn lower_function(co: &bc::CodeObject) -> Result<Function, LowerError> {
    let n_params = co.params.len();
    let local_ty: Vec<MirType> = co.local_types.iter().map(|t| mir_type(*t)).collect();
    let ret_ty = mir_type(co.ret_ty);

    let mut values = Values::new();
    let entry_params: Vec<ValueId> = (0..n_params).map(|i| values.fresh(local_ty[i])).collect();

    let blocks_meta = block_layout(&co.ops)?;
    let reachable = reachable_blocks(&blocks_meta);

    let n_blocks = blocks_meta.len();
    let mut blocks: Vec<BasicBlock> = Vec::with_capacity(n_blocks);
    let mut trampolines: Vec<BasicBlock> = Vec::new();
    for (bi, meta) in blocks_meta.iter().enumerate() {
        if !reachable[bi] {
            blocks.push(BasicBlock {
                params: Vec::new(),
                insts: Vec::new(),
                terminator: Some(Terminator::Unreachable),
            });
            continue;
        }
        let block = lower_block(
            co,
            &local_ty,
            ret_ty,
            &mut values,
            &entry_params,
            meta,
            bi == 0,
            n_blocks,
            &mut trampolines,
        )?;
        blocks.push(block);
    }
    blocks.extend(trampolines);

    Ok(Function {
        params: (0..n_params).map(|i| local_ty[i]).collect(),
        ret: Some(ret_ty),
        blocks,
        entry: BlockId(0),
        value_types: values.types,
    })
}

/// One basic block's op range and how it leaves.
struct BlockMeta {
    /// The first op index (a leader).
    start: usize,
    /// One past the last op index.
    end: usize,
    /// The successor block ids (for reachability).
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
            bc::Op::PopJumpIfFalse(t) => {
                (vec![block_id(*t as usize)?, block_id(end)?], true)
            }
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

#[allow(clippy::too_many_arguments)]
fn lower_block(
    co: &bc::CodeObject,
    local_ty: &[MirType],
    ret_ty: MirType,
    values: &mut Values,
    entry_params: &[ValueId],
    meta: &BlockMeta,
    is_entry: bool,
    n_blocks: usize,
    trampolines: &mut Vec<BasicBlock>,
) -> Result<BasicBlock, LowerError> {
    let n_locals = co.n_locals;
    let n_params = co.params.len();
    let mut insts: Vec<(ValueId, Inst)> = Vec::new();
    let mut locals: Vec<ValueId> = vec![ValueId(0); n_locals];
    let params: Vec<ValueId>;

    if is_entry {
        params = entry_params.to_vec();
        locals[..n_params].copy_from_slice(entry_params);
        for (i, &ty) in local_ty.iter().enumerate().skip(n_params) {
            if ty != MirType::I32 {
                return Err(LowerError::UnsupportedLocalType(i));
            }
            let zero = values.fresh(MirType::I32);
            insts.push((zero, Inst::ConstInt {
                ty: MirType::I32,
                value: 0,
            }));
            locals[i] = zero;
        }
    } else {
        let p: Vec<ValueId> = local_ty.iter().map(|&ty| values.fresh(ty)).collect();
        locals.clone_from(&p);
        params = p;
    }

    let body_end = if meta.ends_in_terminator {
        meta.end - 1
    } else {
        meta.end
    };
    let mut stack: Vec<(ValueId, MirType)> = Vec::new();
    for op in &co.ops[meta.start..body_end] {
        lower_op(co, local_ty, values, &mut insts, &mut locals, &mut stack, op)?;
    }

    let terminator =
        build_terminator(co, ret_ty, meta, &mut stack, &locals, n_blocks, trampolines)?;
    if !stack.is_empty() {
        return Err(LowerError::StackNotEmpty);
    }
    Ok(BasicBlock {
        params,
        insts,
        terminator: Some(terminator),
    })
}

fn pop(stack: &mut Vec<(ValueId, MirType)>) -> Result<(ValueId, MirType), LowerError> {
    stack.pop().ok_or(LowerError::StackUnderflow)
}

fn lower_op(
    co: &bc::CodeObject,
    local_ty: &[MirType],
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    locals: &mut [ValueId],
    stack: &mut Vec<(ValueId, MirType)>,
    op: &bc::Op,
) -> Result<(), LowerError> {
    match op {
        bc::Op::LoadConst(k) => {
            let c = co
                .consts
                .get(*k as usize)
                .ok_or(LowerError::BadConstIndex(*k))?;
            match c {
                bc::Const::Int(v) => {
                    let fits = i32::try_from(*v).map_err(|_| LowerError::IntLiteralTooLarge(*v))?;
                    let id = values.fresh(MirType::I32);
                    insts.push((id, Inst::ConstInt {
                        ty: MirType::I32,
                        value: i64::from(fits),
                    }));
                    stack.push((id, MirType::I32));
                }
                bc::Const::None | bc::Const::Bool(_) | bc::Const::Str(_) => {
                    return Err(LowerError::UnsupportedConst);
                }
            }
        }
        bc::Op::LoadFast(i) => {
            let slot = *i as usize;
            let value = *locals.get(slot).ok_or(LowerError::BadLocalIndex(*i))?;
            stack.push((value, local_ty[slot]));
        }
        bc::Op::StoreFast(i) => {
            let slot = *i as usize;
            if slot >= locals.len() {
                return Err(LowerError::BadLocalIndex(*i));
            }
            let (value, _ty) = pop(stack)?;
            locals[slot] = value;
        }
        bc::Op::Binary(b) => {
            let (rhs, rt) = pop(stack)?;
            let (lhs, lt) = pop(stack)?;
            if lt != MirType::I32 || rt != MirType::I32 {
                return Err(LowerError::DynamicOperation);
            }
            let id = values.fresh(MirType::I32);
            insts.push((id, Inst::Binary {
                op: map_binop(*b)?,
                lhs,
                rhs,
            }));
            stack.push((id, MirType::I32));
        }
        bc::Op::Compare(c) => {
            let (rhs, rt) = pop(stack)?;
            let (lhs, lt) = pop(stack)?;
            if lt != MirType::I32 || rt != MirType::I32 {
                return Err(LowerError::DynamicOperation);
            }
            let id = values.fresh(MirType::I32);
            insts.push((id, Inst::Compare {
                op: map_cmpop(*c),
                lhs,
                rhs,
            }));
            stack.push((id, MirType::I32));
        }
        bc::Op::LoadAttr { name, cache } => {
            let (obj, _ot) = pop(stack)?;
            let name_id = values.fresh(MirType::I32);
            insts.push((name_id, Inst::ConstInt {
                ty: MirType::I32,
                value: i64::from(*name),
            }));
            let id = values.fresh(MirType::PyValue);
            insts.push((id, Inst::PyIntrinsic {
                op: PyOp::Getattr,
                args: vec![obj, name_id],
                cache: cache + 1,
            }));
            stack.push((id, MirType::PyValue));
        }
        bc::Op::PopTop => {
            pop(stack)?;
        }
        bc::Op::LoadGlobal(_) => return Err(LowerError::Deferred("LoadGlobal")),
        bc::Op::Call(_) => return Err(LowerError::Deferred("Call")),
        bc::Op::Jump(_) | bc::Op::PopJumpIfFalse(_) | bc::Op::Return => {
            return Err(LowerError::StackNotEmpty);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_terminator(
    co: &bc::CodeObject,
    ret_ty: MirType,
    meta: &BlockMeta,
    stack: &mut Vec<(ValueId, MirType)>,
    locals: &[ValueId],
    n_blocks: usize,
    trampolines: &mut Vec<BasicBlock>,
) -> Result<Terminator, LowerError> {
    if !meta.ends_in_terminator {
        let target = meta.succs.first().copied().ok_or(LowerError::RunsOffEnd)?;
        return Ok(Terminator::Jump {
            target,
            args: locals.to_vec(),
        });
    }
    match &co.ops[meta.end - 1] {
        bc::Op::Jump(_) => {
            let target = meta.succs[0];
            Ok(Terminator::Jump {
                target,
                args: locals.to_vec(),
            })
        }
        bc::Op::PopJumpIfFalse(_) => {
            let (cond, ct) = pop(stack)?;
            if ct != MirType::I32 {
                return Err(LowerError::BadConditionType);
            }
            let if_false = trampoline(n_blocks, trampolines, meta.succs[0], locals);
            let if_true = trampoline(n_blocks, trampolines, meta.succs[1], locals);
            Ok(Terminator::Branch {
                cond,
                if_true,
                true_args: Vec::new(),
                if_false,
                false_args: Vec::new(),
            })
        }
        bc::Op::Return => {
            let (value, ty) = pop(stack)?;
            if ty != ret_ty {
                return Err(LowerError::ReturnTypeMismatch);
            }
            Ok(Terminator::Return(Some(value)))
        }
        _ => Err(LowerError::RunsOffEnd),
    }
}

/// Append a parameter-less trampoline block that jumps to `target`, passing the
/// branching block's `locals`, and return its block id. Used to route a `Branch`
/// (which the backend requires to carry no arguments) into a parameterized target.
fn trampoline(
    n_blocks: usize,
    trampolines: &mut Vec<BasicBlock>,
    target: BlockId,
    locals: &[ValueId],
) -> BlockId {
    let id = BlockId((n_blocks + trampolines.len()) as u32);
    trampolines.push(BasicBlock {
        params: Vec::new(),
        insts: Vec::new(),
        terminator: Some(Terminator::Jump {
            target,
            args: locals.to_vec(),
        }),
    });
    id
}

/// Lower every function of a compiled module, returning each `(name, Function)`.
/// The `<module>` body is not lowered for first light (the parity harness drives
/// the call boundary).
pub fn lower_module(module: &bc::Module) -> Result<Vec<(String, Function)>, LowerError> {
    module
        .functions
        .iter()
        .map(|co| Ok((co.name.clone(), lower_function(co)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_str;
    use alloc::format;

    fn lower_named(source: &str, name: &str) -> Function {
        let module = compile_str("test", source).expect("compiles");
        let co = module
            .functions
            .iter()
            .find(|f| f.name == name)
            .expect("function present");
        let func = lower_function(co).expect("lowers");
        assert_eq!(
            lamella_ir::verify(&func),
            Ok(()),
            "lowered function must verify: {}",
            describe(&func)
        );
        func
    }

    fn describe(func: &Function) -> String {
        format!("{} blocks, {} values", func.blocks.len(), func.value_types.len())
    }

    fn count_insts(func: &Function, pred: impl Fn(&Inst) -> bool) -> usize {
        func.blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .filter(|(_, inst)| pred(inst))
            .count()
    }

    #[test]
    fn typed_fib_lowers_and_verifies() {
        let src = "\
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
        let func = lower_named(src, "fib");
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
    fn dynamic_getattr_lowers_to_a_py_intrinsic() {
        let func = lower_named("def get_x(obj):\n    return obj.x\n", "get_x");
        assert_eq!(func.params, vec![MirType::PyValue]);
        assert_eq!(func.ret, Some(MirType::PyValue));
        let getattrs = count_insts(&func, |i| {
            matches!(i, Inst::PyIntrinsic { op: PyOp::Getattr, .. })
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
    fn deferred_opcodes_are_rejected() {
        let module = compile_str("test", "def f(n: int) -> int:\n    return n\nf(1)\n")
            .expect("compiles");
        assert!(matches!(
            lower_function(&module.body),
            Err(LowerError::Deferred(_))
        ));
    }
}
