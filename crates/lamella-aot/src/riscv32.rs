//! The RV32IM (RISC-V) target code generator.

use alloc::vec::Vec;

use lamella_asm_riscv32::{BranchCond, Encoder, Label, Reg};
use lamella_ir::{BinOp, CmpOp, Function, Inst, MirType, Terminator, TypeHandle, ValueId};

/// Why a function could not be lowered to RV32IM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowerError {
    /// The function did not pass IR verification.
    NotWellFormed,
    /// An instruction or shape this backend does not lower yet.
    Unsupported,
    /// The all-spilled frame's slot offsets exceed the 12-bit lw/sw immediate (a function past
    /// ~500 values).
    TooManyValues,
    /// A control-flow shape this backend does not handle.
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
/// arguments are not lowered by this backend yet).
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
    lower_module_inner(funcs, None)
}

/// As [`lower_module`], but with the garbage-collected allocator threaded: `Alloc` lowers to a
/// `lamella_gc_alloc(payload_size [a0], &TypeDesc [a1]) -> payload* [a0]` call at `alloc_addr`, and
/// each function's emitted TypeDescs follow its code (addressed PC-relatively via `la`).
pub fn lower_module_gc(funcs: &[Function], alloc_addr: u32) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(funcs, Some(alloc_addr))
}

fn lower_module_inner(funcs: &[Function], alloc_addr: Option<u32>) -> Result<Vec<u8>, LowerError> {
    lower_module_to_image(funcs, alloc_addr, false).map(|(bytes, _, _)| bytes)
}

/// A lowered module: the code bytes, each function's entry offset, and the call relocations as
/// `(auipc offset, callee index)` pairs (empty unless lowering for a relocatable object).
type LoweredModule = (Vec<u8>, Vec<u32>, Vec<(u32, u32)>);

/// Lowers a module and also reports each function's byte offset in the image (its entry point) --
/// the basis for the symbol table when emitting a relocatable object.
fn lower_module_to_image(
    funcs: &[Function],
    alloc_addr: Option<u32>,
    relocate: bool,
) -> Result<LoweredModule, LowerError> {
    for func in funcs {
        if lamella_ir::verify(func).is_err() {
            return Err(LowerError::NotWellFormed);
        }
    }
    let mut enc = Encoder::new();
    let func_labels: Vec<Label> = (0..funcs.len()).map(|_| enc.new_label()).collect();
    let mut offsets: Vec<u32> = Vec::with_capacity(funcs.len());
    let mut call_relocs: Vec<(u32, u32)> = Vec::new();
    for (index, func) in funcs.iter().enumerate() {
        enc.bind_label(func_labels[index]);
        offsets.push(enc.position());
        lower_function(
            &mut enc,
            func,
            &func_labels,
            alloc_addr,
            &mut call_relocs,
            relocate,
        )?;
    }
    let bytes = enc
        .finish()
        .map(|assembled| assembled.bytes)
        .map_err(|_| LowerError::CodeTooLarge)?;
    Ok((bytes, offsets, call_relocs))
}

/// Lowers a module into an ELF32 relocatable object: each function becomes a global `STT_FUNC`
/// symbol (named by `names[i]`) at its entry offset, and every call becomes an `R_RISCV_CALL_PLT`
/// relocation to the callee's symbol -- so a linker (ours or another) resolves them and can see the
/// call graph for `--gc-sections`. `names` must have one entry per function in `funcs`.
pub fn lower_object(funcs: &[Function], names: &[&str]) -> Result<Vec<u8>, LowerError> {
    let (text, offsets, call_relocs) = lower_module_to_image(funcs, None, true)?;
    let symbols: Vec<lamella_elf::Symbol> = (0..funcs.len())
        .map(|i| {
            let end = offsets.get(i + 1).copied().unwrap_or(text.len() as u32);
            lamella_elf::Symbol {
                name: names[i],
                value: offsets[i],
                size: end - offsets[i],
                binding: lamella_elf::Binding::Global,
                kind: lamella_elf::SymbolType::Func,
                section: lamella_elf::SymbolSection::Text,
            }
        })
        .collect();
    let relocations: Vec<lamella_elf::Relocation> = call_relocs
        .iter()
        .map(|&(offset, callee)| lamella_elf::Relocation {
            offset,
            symbol: callee,
            kind: lamella_elf::riscv::R_RISCV_CALL_PLT,
            addend: 0,
        })
        .collect();
    Ok(lamella_elf::write_relocatable_object(
        lamella_elf::Machine::RiscV,
        &text,
        &symbols,
        &relocations,
    ))
}

/// Lowers one function into `enc`: a prologue that allocates a frame and saves the callee-saved
/// registers it uses (plus `ra` if it calls), the incoming arguments moved from a0-a7 into the
/// entry block's parameters, the block bodies, and -- at each return -- a value moved to a0 then
/// the saved registers restored and `ret`.
fn lower_function(
    enc: &mut Encoder,
    func: &Function,
    func_labels: &[Label],
    alloc_addr: Option<u32>,
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
) -> Result<(), LowerError> {
    let pool = allocatable();
    let value_count = func.value_types.len();
    let allocates = func.blocks.iter().any(|b| {
        b.insts
            .iter()
            .any(|(_, i)| matches!(i, Inst::Alloc { .. } | Inst::AllocArray { .. }))
    });
    if value_count > pool.len() || allocates {
        return lower_function_spilled(enc, func, func_labels, alloc_addr, relocs, relocate);
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
            lower_inst(
                enc,
                &reg,
                &func.value_types,
                func_labels,
                *result,
                inst,
                relocs,
                relocate,
            )?;
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

/// Emits a call to function `callee`: in object mode (`relocate`) an `auipc`+`jalr` pair whose
/// target is left for a `R_RISCV_CALL_PLT` relocation (the site recorded in `relocs`); otherwise a
/// resolved `jal` to the callee's intra-module label.
fn emit_call(
    enc: &mut Encoder,
    func_labels: &[Label],
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
    callee: u32,
) -> Result<(), LowerError> {
    if relocate {
        relocs.push((enc.position(), callee));
        enc.auipc(Reg::RA, 0);
        enc.jalr(Reg::RA, Reg::RA, 0);
    } else {
        let label = *func_labels
            .get(callee as usize)
            .ok_or(LowerError::ControlFlowUnsupported)?;
        enc.jal(Reg::RA, label);
    }
    Ok(())
}

/// Lowers one value-defining instruction into its assigned register.
#[allow(clippy::too_many_arguments)]
fn lower_inst(
    enc: &mut Encoder,
    reg: &impl Fn(ValueId) -> Reg,
    value_types: &[MirType],
    func_labels: &[Label],
    result: ValueId,
    inst: &Inst,
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
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
            emit_call(enc, func_labels, relocs, relocate, *callee)?;
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

/// Lowers a function whose value count exceeds the 12 callee-saved registers into an ALL-SPILLED
/// frame: every value gets a 4-byte stack slot, each instruction loads its operands into the
/// `t0`-`t2` scratch registers, computes, and stores the result back. Nothing live sits in a
/// register across a call, so the caller's values survive with no callee-saved bookkeeping (only
/// `ra` is saved). This lifts the register-only path's value-count cap. The frame's slot offsets
/// must fit the 12-bit `lw`/`sw` immediate, so a function past ~500 values is rejected (deferred).
/// Block parameters move slot-to-slot through `t0`: every value has a distinct slot, so the
/// sequential move is sound (the register path's no-alias assumption).
fn lower_function_spilled(
    enc: &mut Encoder,
    func: &Function,
    func_labels: &[Label],
    alloc_addr: Option<u32>,
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
) -> Result<(), LowerError> {
    let value_count = func.value_types.len();
    let has_calls = func.blocks.iter().any(|b| {
        b.insts.iter().any(|(_, i)| {
            matches!(
                i,
                Inst::Call { .. } | Inst::Alloc { .. } | Inst::AllocArray { .. }
            )
        })
    });
    let ra_off = (value_count * 4) as i32;
    let frame = ((value_count + has_calls as usize) * 4).div_ceil(16) * 16;
    if frame > 2047 {
        return Err(LowerError::TooManyValues);
    }
    let slot = |v: ValueId| (v.index() * 4) as i32;
    let mut type_descs: Vec<(Label, Vec<u32>)> = Vec::new();
    let mut type_desc_labels: Vec<(TypeHandle, Label)> = Vec::new();

    if frame > 0 {
        enc.addi(Reg::SP, Reg::SP, -(frame as i32));
    }
    if has_calls {
        enc.sw(Reg::RA, Reg::SP, ra_off);
    }
    let entry = &func.blocks[func.entry.index()];
    for (i, &param) in entry.params.iter().enumerate() {
        let arg = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
        enc.sw(arg, Reg::SP, slot(param));
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
            lower_inst_spilled(
                enc,
                &slot,
                &func.value_types,
                func_labels,
                *result,
                inst,
                alloc_addr,
                &mut type_descs,
                &mut type_desc_labels,
                relocs,
                relocate,
            )?;
        }
        match &block.terminator {
            Some(Terminator::Return(value)) => {
                if let Some(v) = value {
                    enc.lw(Reg::A0, Reg::SP, slot(*v));
                }
                if has_calls {
                    enc.lw(Reg::RA, Reg::SP, ra_off);
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
                    enc.lw(Reg::T0, Reg::SP, slot(*a));
                    enc.sw(Reg::T0, Reg::SP, slot(*p));
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
                        enc.lw(Reg::T0, Reg::SP, slot(lhs));
                        enc.lw(Reg::T1, Reg::SP, slot(rhs));
                        let (cond, a, b) = branch_for(op, Reg::T0, Reg::T1);
                        enc.branch(cond, a, b, true_label);
                    }
                    None => {
                        enc.lw(Reg::T0, Reg::SP, slot(*cond));
                        enc.branch(BranchCond::Ne, Reg::T0, Reg::ZERO, true_label);
                    }
                }
                enc.j(false_label);
            }
            Some(Terminator::Unreachable) => enc.ebreak(),
            None => return Err(LowerError::ControlFlowUnsupported),
        }
    }
    for (label, words) in &type_descs {
        enc.bind_label(*label);
        for &w in words {
            enc.emit_word(w);
        }
    }
    Ok(())
}

/// Lowers one instruction in the all-spilled frame: load operands from their slots into `t0`-`t2`,
/// compute, store the result back to its slot. Mirrors [`lower_inst`] but slot-based; `t6` stays
/// the array-addressing scratch and a0-a7 carry call arguments.
#[allow(clippy::too_many_arguments)]
fn lower_inst_spilled(
    enc: &mut Encoder,
    slot: &impl Fn(ValueId) -> i32,
    value_types: &[MirType],
    func_labels: &[Label],
    result: ValueId,
    inst: &Inst,
    alloc_addr: Option<u32>,
    type_descs: &mut Vec<(Label, Vec<u32>)>,
    type_desc_labels: &mut Vec<(TypeHandle, Label)>,
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
) -> Result<(), LowerError> {
    let (t0, t1, t2) = (Reg::T0, Reg::T1, Reg::T2);
    match inst {
        Inst::ConstInt { value, .. } => {
            enc.li(t0, *value as i32);
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::Binary { op, lhs, rhs } => {
            enc.lw(t0, Reg::SP, slot(*lhs));
            enc.lw(t1, Reg::SP, slot(*rhs));
            match op {
                BinOp::Add => enc.add(t0, t0, t1),
                BinOp::Sub => enc.sub(t0, t0, t1),
                BinOp::And => enc.and(t0, t0, t1),
                BinOp::Or => enc.or(t0, t0, t1),
                BinOp::Xor => enc.xor(t0, t0, t1),
                BinOp::Mul => enc.mul(t0, t0, t1),
                BinOp::DivSigned => enc.div(t0, t0, t1),
                BinOp::DivUnsigned => enc.divu(t0, t0, t1),
                BinOp::RemSigned => enc.rem(t0, t0, t1),
                BinOp::RemUnsigned => enc.remu(t0, t0, t1),
                BinOp::Shl => enc.sll(t0, t0, t1),
                BinOp::ShrSigned => enc.sra(t0, t0, t1),
                BinOp::ShrUnsigned => enc.srl(t0, t0, t1),
            }
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::Compare { op, lhs, rhs } => {
            enc.lw(t0, Reg::SP, slot(*lhs));
            enc.lw(t1, Reg::SP, slot(*rhs));
            materialize_compare(enc, t2, t0, t1, *op);
            enc.sw(t2, Reg::SP, slot(result));
        }
        Inst::Load { address } => {
            enc.lw(t0, Reg::SP, slot(*address));
            enc.lw(t1, t0, 0);
            enc.sw(t1, Reg::SP, slot(result));
        }
        Inst::Store { address, value } => {
            enc.lw(t0, Reg::SP, slot(*address));
            enc.lw(t1, Reg::SP, slot(*value));
            enc.sw(t1, t0, 0);
        }
        Inst::FieldLoad { base, offset } => {
            if !is_pointer(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            enc.lw(t0, Reg::SP, slot(*base));
            enc.lw(t1, t0, field_offset(*offset)?);
            enc.sw(t1, Reg::SP, slot(result));
        }
        Inst::FieldStore {
            base,
            offset,
            value,
        } => {
            if !is_pointer(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            enc.lw(t0, Reg::SP, slot(*base));
            enc.lw(t1, Reg::SP, slot(*value));
            enc.sw(t1, t0, field_offset(*offset)?);
        }
        Inst::FieldAddr { base, offset } => {
            if !is_pointer(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            enc.lw(t0, Reg::SP, slot(*base));
            enc.addi(t1, t0, field_offset(*offset)?);
            enc.sw(t1, Reg::SP, slot(result));
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
            enc.lw(t0, Reg::SP, slot(*array));
            enc.lw(t1, Reg::SP, slot(*index));
            emit_element_address(enc, t0, t1);
            enc.lw(t2, scratch(), 4);
            enc.sw(t2, Reg::SP, slot(result));
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
            enc.lw(t0, Reg::SP, slot(*array));
            enc.lw(t1, Reg::SP, slot(*index));
            enc.lw(t2, Reg::SP, slot(*value));
            emit_element_address(enc, t0, t1);
            enc.sw(t2, scratch(), 4);
        }
        Inst::Alloc {
            handle,
            payload_size,
            ref_offsets,
        } => {
            let alloc = alloc_addr.ok_or(LowerError::Unsupported)?;
            let desc_label = match type_desc_labels.iter().find(|(h, _)| h == handle) {
                Some((_, l)) => *l,
                None => {
                    let l = enc.new_label();
                    let mut words = Vec::with_capacity(2 + ref_offsets.len());
                    words.push(*payload_size);
                    words.push(ref_offsets.len() as u32);
                    words.extend_from_slice(ref_offsets);
                    type_descs.push((l, words));
                    type_desc_labels.push((*handle, l));
                    l
                }
            };
            enc.li(Reg::A0, *payload_size as i32);
            enc.la(Reg::A1, desc_label);
            enc.li(t0, alloc as i32);
            enc.jalr(Reg::RA, t0, 0);
            let ok = enc.new_label();
            enc.branch(BranchCond::Ne, Reg::A0, Reg::ZERO, ok);
            enc.ebreak();
            enc.bind_label(ok);
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::AllocArray {
            handle,
            length,
            element_size,
        } => {
            let alloc = alloc_addr.ok_or(LowerError::Unsupported)?;
            let desc_label = match type_desc_labels.iter().find(|(h, _)| h == handle) {
                Some((_, l)) => *l,
                None => {
                    let l = enc.new_label();
                    type_descs.push((l, alloc::vec![*element_size, 0]));
                    type_desc_labels.push((*handle, l));
                    l
                }
            };
            enc.lw(t0, Reg::SP, slot(*length));
            enc.li(t1, *element_size as i32);
            enc.mul(t0, t0, t1);
            enc.addi(Reg::A0, t0, 4);
            enc.la(Reg::A1, desc_label);
            enc.li(t0, alloc as i32);
            enc.jalr(Reg::RA, t0, 0);
            let ok = enc.new_label();
            enc.branch(BranchCond::Ne, Reg::A0, Reg::ZERO, ok);
            enc.ebreak();
            enc.bind_label(ok);
            enc.lw(t0, Reg::SP, slot(*length));
            enc.sw(t0, Reg::A0, 0);
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::Call { callee, args } => {
            for (i, &arg) in args.iter().enumerate() {
                let target = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(target, Reg::SP, slot(arg));
            }
            emit_call(enc, func_labels, relocs, relocate, *callee)?;
            enc.sw(Reg::A0, Reg::SP, slot(result));
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
/// the only field base the register-only path can dereference. A value type spans several words
/// and needs the stack slots that path does not hold yet.
fn is_pointer(value_types: &[MirType], value: ValueId) -> bool {
    matches!(
        value_types.get(value.index()),
        Some(MirType::ObjectRef | MirType::ManagedPtr)
    )
}

/// Converts a field/element byte offset to the signed 12-bit immediate RISC-V `lw`/`sw`/`addi`
/// take, or `Unsupported` if it does not fit. This backend does not materialize a large offset into
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
