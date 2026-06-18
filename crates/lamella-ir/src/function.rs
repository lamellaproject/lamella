//! The control-flow graph: values, blocks, terminators, and functions.

use alloc::vec::Vec;

use crate::inst::Inst;
use crate::types::MirType;

/// A virtual register: the typed result of one instruction, or a block
/// parameter. Each id is assigned once (SSA-friendly) and indexes the owning
/// function's value arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueId(pub u32);

/// A basic block, identified by a dense index into the owning function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u32);

impl ValueId {
    /// The index into the function's value arena.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl BlockId {
    /// The index into the function's block arena.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// How a block transfers control after its instructions run.
///
/// Every block ends in exactly one terminator. Branch targets carry the
/// [`ValueId`] arguments that become the target block's parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminator {
    /// Unconditional branch to `target`, passing `args` as its parameters.
    Jump {
        /// The block to branch to.
        target: BlockId,
        /// Argument values, one per `target` parameter, in order.
        args: Vec<ValueId>,
    },
    /// Branch on `cond` (an `int32`): to `if_true` when `cond` is non-zero,
    /// otherwise to `if_false`. Compare-and-branch is kept separate in MIR and
    /// fused per target during instruction selection.
    Branch {
        /// The `int32` condition value; non-zero takes `if_true`.
        cond: ValueId,
        /// The block taken when `cond` is non-zero.
        if_true: BlockId,
        /// Arguments passed to `if_true`'s parameters.
        true_args: Vec<ValueId>,
        /// The block taken when `cond` is zero.
        if_false: BlockId,
        /// Arguments passed to `if_false`'s parameters.
        false_args: Vec<ValueId>,
    },
    /// Return from the function, with a value unless the return type is `void`.
    Return(Option<ValueId>),
    /// A point control can never reach; lowers to a trap.
    Unreachable,
}

/// A basic block: parameter values, a straight-line body that defines result
/// values, and a terminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicBlock {
    /// This block's parameter values, supplied by predecessors' branch arguments.
    pub params: Vec<ValueId>,
    /// The instructions in order, each paired with the value it defines.
    pub insts: Vec<(ValueId, Inst)>,
    /// The terminator; `None` only while the block is under construction.
    pub terminator: Option<Terminator>,
}

/// A function in MIR: a typed signature and a control-flow graph over a shared
/// value arena.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    /// The parameter types, in order. The entry block's parameters take these.
    pub params: Vec<MirType>,
    /// The return type, or `None` for `void`.
    pub ret: Option<MirType>,
    /// The blocks; the entry block runs first.
    pub blocks: Vec<BasicBlock>,
    /// The entry block.
    pub entry: BlockId,
    /// The type of every value, indexed by [`ValueId`]. Both block parameters and
    /// instruction results draw their ids from here.
    pub value_types: Vec<MirType>,
}

impl Function {
    /// The declared type of `value`, or `None` if the id is out of range.
    #[must_use]
    pub fn value_type(&self, value: ValueId) -> Option<MirType> {
        self.value_types.get(value.index()).copied()
    }

    /// The block with the given id, or `None` if the id is out of range.
    #[must_use]
    pub fn block(&self, block: BlockId) -> Option<&BasicBlock> {
        self.blocks.get(block.index())
    }
}
