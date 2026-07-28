//! The decoded internal instruction stream.

use crate::ValType;
use alloc::boxed::Box;

/// How a runtime label behaves when branched to: a `Loop` label survives the branch and jumps
/// backward to its body; a `Block` label is consumed and jumps forward past its `end`. This
/// asymmetry is load-bearing: getting it backward silently miscompiles every loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelKind {
    /// A forward label: `block`, an `if`'s region, and the function's implicit body block.
    Block,
    /// A backward label: `loop`.
    Loop,
}

/// One decoded instruction. `target` fields are indices into the same function's op stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Push a runtime label recording the current operand height. For a [`LabelKind::Block`]
    /// the target is the op index just past the matching [`Op::PopLabel`]; for a
    /// [`LabelKind::Loop`] it is the first body op (this op's own index + 1).
    PushLabel {
        /// How a branch treats the label.
        kind: LabelKind,
        /// How many values a branch to this label carries (0 or 1 in this scope; always 0
        /// for a loop label).
        keep: u8,
        /// The branch destination (see above).
        target: u32,
    },
    /// Pop the innermost label: the structured `end` on the fall-through path.
    PopLabel,
    /// Pop an `i32`; jump to `target` when it is zero. The `if` false edge (internal form --
    /// there is no wasm opcode for it; `br_if` compiles to [`Op::BrIf`] instead).
    BrIfZero {
        /// The else-arm's first op, or the region's [`Op::PopLabel`] when there is no else.
        target: u32,
    },
    /// Unconditional internal jump: the then-arm's skip over an else body, landing on the
    /// region's [`Op::PopLabel`].
    Goto {
        /// The destination op index.
        target: u32,
    },
    /// `br` -- branch to the label `depth` frames down the runtime label stack.
    Br {
        /// 0 = innermost.
        depth: u32,
    },
    /// `br_if` -- pop an `i32`, branch like [`Op::Br`] when non-zero.
    BrIf {
        /// 0 = innermost.
        depth: u32,
    },
    /// `br_table` -- pop an `i32` index into `depths`, falling back to `default` past the end.
    BrTable {
        /// The table of branch depths.
        depths: Box<[u32]>,
        /// The depth taken for any out-of-range index.
        default: u32,
    },
    /// `return` from the function with its declared results.
    Return,
    /// `unreachable` -- trap unconditionally.
    Unreachable,
    /// `nop`.
    Nop,
    /// `call` a function by joint-index-space index (imports first).
    Call {
        /// The callee's function index.
        func: u32,
    },
    /// `call_indirect` through the funcref table, checking the callee against a declared type.
    CallIndirect {
        /// The expected signature, as a type-section index.
        type_index: u32,
    },
    /// `drop` the top operand.
    Drop,
    /// `select` -- pop an `i32` condition and two operands; push the first when non-zero.
    Select,
    /// `local.get` -- push a local.
    LocalGet(u32),
    /// `local.set` -- pop into a local.
    LocalSet(u32),
    /// `local.tee` -- copy the top operand into a local, leaving it on the stack.
    LocalTee(u32),
    /// `global.get` -- push a global.
    GlobalGet(u32),
    /// `global.set` -- pop into a global.
    GlobalSet(u32),
    /// A linear-memory load: `width` bytes at `[addr + offset]`, extended to `ty`.
    Load {
        /// The result type.
        ty: ValType,
        /// The access width in bytes: 1, 2, 4, or 8.
        width: u8,
        /// Sign-extend a sub-width integer load (zero-extend otherwise; meaningless at full
        /// width and for floats).
        signed: bool,
        /// The constant offset added to the popped address.
        offset: u32,
    },
    /// A linear-memory store of the low `width` bytes of a `ty` value at `[addr + offset]`.
    Store {
        /// The operand type.
        ty: ValType,
        /// The access width in bytes: 1, 2, 4, or 8.
        width: u8,
        /// The constant offset added to the popped address.
        offset: u32,
    },
    /// `memory.size` -- push the current size in pages.
    MemorySize,
    /// `memory.grow` -- pop a page delta, push the old size or -1 on refusal.
    MemoryGrow,
    /// `memory.copy` -- pop (dst, src, len), copy within linear memory.
    MemoryCopy,
    /// `memory.fill` -- pop (dst, byte, len), fill linear memory.
    MemoryFill,
    /// `i32.const` (bit pattern).
    I32Const(u32),
    /// `i64.const` (bit pattern).
    I64Const(u64),
    /// `f32.const` (raw IEEE-754 bits).
    F32Const(u32),
    /// `f64.const` (raw IEEE-754 bits).
    F64Const(u64),
    /// A pure numeric operation on the operand stack.
    Num(NumOp),
}

/// The pure numeric operations, one variant per wasm mnemonic. Grouped exactly as the spec
/// numbers them; the executor gives each its section-5 semantics (masked shifts, guarded
/// division, exact truncation checks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::missing_docs_in_private_items)]
pub enum NumOp {
    /// `i32.eqz`
    I32Eqz,
    /// `i32.eq`
    I32Eq,
    /// `i32.ne`
    I32Ne,
    /// `i32.lt_s`
    I32LtS,
    /// `i32.lt_u`
    I32LtU,
    /// `i32.gt_s`
    I32GtS,
    /// `i32.gt_u`
    I32GtU,
    /// `i32.le_s`
    I32LeS,
    /// `i32.le_u`
    I32LeU,
    /// `i32.ge_s`
    I32GeS,
    /// `i32.ge_u`
    I32GeU,
    /// `i64.eqz`
    I64Eqz,
    /// `i64.eq`
    I64Eq,
    /// `i64.ne`
    I64Ne,
    /// `i64.lt_s`
    I64LtS,
    /// `i64.lt_u`
    I64LtU,
    /// `i64.gt_s`
    I64GtS,
    /// `i64.gt_u`
    I64GtU,
    /// `i64.le_s`
    I64LeS,
    /// `i64.le_u`
    I64LeU,
    /// `i64.ge_s`
    I64GeS,
    /// `i64.ge_u`
    I64GeU,
    /// `f32.eq`
    F32Eq,
    /// `f32.ne`
    F32Ne,
    /// `f32.lt`
    F32Lt,
    /// `f32.gt`
    F32Gt,
    /// `f32.le`
    F32Le,
    /// `f32.ge`
    F32Ge,
    /// `f64.eq`
    F64Eq,
    /// `f64.ne`
    F64Ne,
    /// `f64.lt`
    F64Lt,
    /// `f64.gt`
    F64Gt,
    /// `f64.le`
    F64Le,
    /// `f64.ge`
    F64Ge,
    /// `i32.clz`
    I32Clz,
    /// `i32.ctz`
    I32Ctz,
    /// `i32.popcnt`
    I32Popcnt,
    /// `i32.add`
    I32Add,
    /// `i32.sub`
    I32Sub,
    /// `i32.mul`
    I32Mul,
    /// `i32.div_s`
    I32DivS,
    /// `i32.div_u`
    I32DivU,
    /// `i32.rem_s`
    I32RemS,
    /// `i32.rem_u`
    I32RemU,
    /// `i32.and`
    I32And,
    /// `i32.or`
    I32Or,
    /// `i32.xor`
    I32Xor,
    /// `i32.shl`
    I32Shl,
    /// `i32.shr_s`
    I32ShrS,
    /// `i32.shr_u`
    I32ShrU,
    /// `i32.rotl`
    I32Rotl,
    /// `i32.rotr`
    I32Rotr,
    /// `i64.clz`
    I64Clz,
    /// `i64.ctz`
    I64Ctz,
    /// `i64.popcnt`
    I64Popcnt,
    /// `i64.add`
    I64Add,
    /// `i64.sub`
    I64Sub,
    /// `i64.mul`
    I64Mul,
    /// `i64.div_s`
    I64DivS,
    /// `i64.div_u`
    I64DivU,
    /// `i64.rem_s`
    I64RemS,
    /// `i64.rem_u`
    I64RemU,
    /// `i64.and`
    I64And,
    /// `i64.or`
    I64Or,
    /// `i64.xor`
    I64Xor,
    /// `i64.shl`
    I64Shl,
    /// `i64.shr_s`
    I64ShrS,
    /// `i64.shr_u`
    I64ShrU,
    /// `i64.rotl`
    I64Rotl,
    /// `i64.rotr`
    I64Rotr,
    /// `f32.abs`
    F32Abs,
    /// `f32.neg`
    F32Neg,
    /// `f32.ceil`
    F32Ceil,
    /// `f32.floor`
    F32Floor,
    /// `f32.trunc`
    F32Trunc,
    /// `f32.nearest`
    F32Nearest,
    /// `f32.sqrt`
    F32Sqrt,
    /// `f32.add`
    F32Add,
    /// `f32.sub`
    F32Sub,
    /// `f32.mul`
    F32Mul,
    /// `f32.div`
    F32Div,
    /// `f32.min`
    F32Min,
    /// `f32.max`
    F32Max,
    /// `f32.copysign`
    F32Copysign,
    /// `f64.abs`
    F64Abs,
    /// `f64.neg`
    F64Neg,
    /// `f64.ceil`
    F64Ceil,
    /// `f64.floor`
    F64Floor,
    /// `f64.trunc`
    F64Trunc,
    /// `f64.nearest`
    F64Nearest,
    /// `f64.sqrt`
    F64Sqrt,
    /// `f64.add`
    F64Add,
    /// `f64.sub`
    F64Sub,
    /// `f64.mul`
    F64Mul,
    /// `f64.div`
    F64Div,
    /// `f64.min`
    F64Min,
    /// `f64.max`
    F64Max,
    /// `f64.copysign`
    F64Copysign,
    /// `i32.wrap_i64`
    I32WrapI64,
    /// `i32.trunc_f32_s`
    I32TruncF32S,
    /// `i32.trunc_f32_u`
    I32TruncF32U,
    /// `i32.trunc_f64_s`
    I32TruncF64S,
    /// `i32.trunc_f64_u`
    I32TruncF64U,
    /// `i64.extend_i32_s`
    I64ExtendI32S,
    /// `i64.extend_i32_u`
    I64ExtendI32U,
    /// `i64.trunc_f32_s`
    I64TruncF32S,
    /// `i64.trunc_f32_u`
    I64TruncF32U,
    /// `i64.trunc_f64_s`
    I64TruncF64S,
    /// `i64.trunc_f64_u`
    I64TruncF64U,
    /// `f32.convert_i32_s`
    F32ConvertI32S,
    /// `f32.convert_i32_u`
    F32ConvertI32U,
    /// `f32.convert_i64_s`
    F32ConvertI64S,
    /// `f32.convert_i64_u`
    F32ConvertI64U,
    /// `f32.demote_f64`
    F32DemoteF64,
    /// `f64.convert_i32_s`
    F64ConvertI32S,
    /// `f64.convert_i32_u`
    F64ConvertI32U,
    /// `f64.convert_i64_s`
    F64ConvertI64S,
    /// `f64.convert_i64_u`
    F64ConvertI64U,
    /// `f64.promote_f32`
    F64PromoteF32,
    /// `i32.reinterpret_f32`
    I32ReinterpretF32,
    /// `i64.reinterpret_f64`
    I64ReinterpretF64,
    /// `f32.reinterpret_i32`
    F32ReinterpretI32,
    /// `f64.reinterpret_i64`
    F64ReinterpretI64,
    /// `i32.extend8_s` (sign-extension operators)
    I32Extend8S,
    /// `i32.extend16_s`
    I32Extend16S,
    /// `i64.extend8_s`
    I64Extend8S,
    /// `i64.extend16_s`
    I64Extend16S,
    /// `i64.extend32_s`
    I64Extend32S,
    /// `i32.trunc_sat_f32_s` (non-trapping float-to-int)
    I32TruncSatF32S,
    /// `i32.trunc_sat_f32_u`
    I32TruncSatF32U,
    /// `i32.trunc_sat_f64_s`
    I32TruncSatF64S,
    /// `i32.trunc_sat_f64_u`
    I32TruncSatF64U,
    /// `i64.trunc_sat_f32_s`
    I64TruncSatF32S,
    /// `i64.trunc_sat_f32_u`
    I64TruncSatF32U,
    /// `i64.trunc_sat_f64_s`
    I64TruncSatF64S,
    /// `i64.trunc_sat_f64_u`
    I64TruncSatF64U,
}
