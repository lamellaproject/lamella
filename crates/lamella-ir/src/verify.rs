//! Well-formedness checking for MIR, which doubles as the fuzzing oracle.

use alloc::vec::Vec;

use crate::function::{BlockId, Function, Terminator, ValueId};
use crate::inst::{BinOp, Inst};
use crate::types::MirType;

/// A way in which a [`Function`] is not well formed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// The entry block id is out of range.
    BadEntry(BlockId),
    /// A block does not end in a terminator.
    MissingTerminator(BlockId),
    /// A branch names a block that does not exist.
    BadBlockRef(BlockId),
    /// A value id is out of range, or used before it is defined.
    UndefinedValue(ValueId),
    /// A value is defined by more than one parameter or instruction.
    MultiplyDefined(ValueId),
    /// A branch passed the wrong number of arguments for its target's parameters.
    ArgCountMismatch {
        /// The branch target.
        target: BlockId,
        /// The number of parameters the target declares.
        expected: usize,
        /// The number of arguments the branch supplied.
        found: usize,
    },
    /// Two types that had to agree did not: a branch argument against a target
    /// parameter, the operands of a binary operation, or an instruction result.
    TypeMismatch {
        /// The type required at this position.
        expected: MirType,
        /// The type actually found.
        found: MirType,
    },
    /// A returned value, or its absence, does not match the function's return
    /// type.
    BadReturnType,
}

/// Checks that `func` is well formed, returning every problem found. An empty
/// result (`Ok`) means the function is sound to lower.
pub fn verify(func: &Function) -> Result<(), Vec<VerifyError>> {
    let mut errors = Vec::new();

    if func.entry.index() >= func.blocks.len() {
        errors.push(VerifyError::BadEntry(func.entry));
    }

    let mut defined: Vec<bool> = Vec::new();
    defined.resize(func.value_types.len(), false);
    for block in &func.blocks {
        for &param in &block.params {
            define(&mut defined, param, &mut errors);
        }
        for (result, _) in &block.insts {
            define(&mut defined, *result, &mut errors);
        }
    }

    for (index, block) in func.blocks.iter().enumerate() {
        let block_id = BlockId(index as u32);
        for (result, inst) in &block.insts {
            check_inst(func, &defined, *result, inst, &mut errors);
        }
        match &block.terminator {
            None => errors.push(VerifyError::MissingTerminator(block_id)),
            Some(terminator) => check_terminator(func, &defined, terminator, &mut errors),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn define(defined: &mut [bool], value: ValueId, errors: &mut Vec<VerifyError>) {
    match defined.get_mut(value.index()) {
        None => errors.push(VerifyError::UndefinedValue(value)),
        Some(slot) => {
            if *slot {
                errors.push(VerifyError::MultiplyDefined(value));
            }
            *slot = true;
        }
    }
}

fn use_value(
    func: &Function,
    defined: &[bool],
    value: ValueId,
    errors: &mut Vec<VerifyError>,
) -> Option<MirType> {
    if !defined.get(value.index()).copied().unwrap_or(false) {
        errors.push(VerifyError::UndefinedValue(value));
    }
    func.value_type(value)
}

fn expect(expected: MirType, found: MirType, errors: &mut Vec<VerifyError>) {
    if expected != found {
        errors.push(VerifyError::TypeMismatch { expected, found });
    }
}

fn check_inst(
    func: &Function,
    defined: &[bool],
    result: ValueId,
    inst: &Inst,
    errors: &mut Vec<VerifyError>,
) {
    let result_ty = func.value_type(result);
    match inst {
        Inst::ConstInt { ty, .. } => {
            if let Some(rt) = result_ty {
                expect(*ty, rt, errors);
            }
        }
        Inst::Binary { op, lhs, rhs } => {
            let a = use_value(func, defined, *lhs, errors);
            let b = use_value(func, defined, *rhs, errors);
            let is_shift = matches!(op, BinOp::Shl | BinOp::ShrSigned | BinOp::ShrUnsigned);
            if !is_shift {
                if let (Some(a), Some(b)) = (a, b) {
                    expect(a, b, errors);
                }
            }
            if let (Some(a), Some(r)) = (a, result_ty) {
                expect(a, r, errors);
            }
        }
        Inst::Compare { lhs, rhs, .. } => {
            let a = use_value(func, defined, *lhs, errors);
            let b = use_value(func, defined, *rhs, errors);
            if let (Some(a), Some(b)) = (a, b) {
                expect(a, b, errors);
            }
            if let Some(r) = result_ty {
                expect(MirType::I32, r, errors);
            }
        }
        Inst::Call { args, .. }
        | Inst::CallVirtual { args, .. }
        | Inst::CallInterface { args, .. }
        | Inst::CastClassScan { args, .. } => {
            for &arg in args {
                use_value(func, defined, arg, errors);
            }
        }
        Inst::CallIndirect { target, args } => {
            use_value(func, defined, *target, errors);
            for &arg in args {
                use_value(func, defined, arg, errors);
            }
        }
        Inst::CallNative { args, .. } => {
            for &arg in args {
                use_value(func, defined, arg, errors);
            }
        }
        Inst::FuncAddr { .. } => {
            if let Some(r) = result_ty {
                expect(MirType::I32, r, errors);
            }
        }
        Inst::PyIntrinsic { op, args, .. } => {
            for &arg in args {
                use_value(func, defined, arg, errors);
            }
            if let (Some(expected), Some(r)) = (op.result_type(), result_ty) {
                expect(expected, r, errors);
            }
        }
        Inst::Store { address, value, .. } => {
            use_value(func, defined, *address, errors);
            use_value(func, defined, *value, errors);
        }
        Inst::Load { address, .. } => {
            use_value(func, defined, *address, errors);
        }
        Inst::CopyBlock { dst, src, size } => {
            use_value(func, defined, *dst, errors);
            use_value(func, defined, *src, errors);
            use_value(func, defined, *size, errors);
        }
        Inst::FillBlock { dst, value, size } => {
            use_value(func, defined, *dst, errors);
            use_value(func, defined, *value, errors);
            use_value(func, defined, *size, errors);
        }
        Inst::Convert { value, kind } => {
            use_value(func, defined, *value, errors);
            if let Some(r) = result_ty {
                expect(kind.result_type(), r, errors);
            }
        }
        Inst::Widen { value, .. } => {
            use_value(func, defined, *value, errors);
            if let Some(r) = result_ty {
                expect(MirType::I64, r, errors);
            }
        }
        Inst::Truncate { value } => {
            use_value(func, defined, *value, errors);
            if let Some(r) = result_ty {
                expect(MirType::I32, r, errors);
            }
        }
        Inst::InitStruct => {
        }
        Inst::FieldLoad { base, .. } => {
            use_value(func, defined, *base, errors);
        }
        Inst::FieldStore { base, value, .. } => {
            use_value(func, defined, *base, errors);
            use_value(func, defined, *value, errors);
        }
        Inst::FieldAddr { base, .. } => {
            use_value(func, defined, *base, errors);
        }
        Inst::LoadTypeDesc { object } => {
            use_value(func, defined, *object, errors);
        }
        Inst::TypeDescAddr { .. } => {
        }
        Inst::CopyStruct { src } => {
            use_value(func, defined, *src, errors);
        }
        Inst::SemihostWrite { .. } => {
        }
        Inst::WriteInt { value } => {
            use_value(func, defined, *value, errors);
        }
        Inst::StringLiteral { .. } => {
        }
        Inst::StringEquals { lhs, rhs } => {
            use_value(func, defined, *lhs, errors);
            use_value(func, defined, *rhs, errors);
            if let Some(r) = result_ty {
                expect(MirType::I32, r, errors);
            }
        }
        Inst::StringConcat { lhs, rhs } => {
            use_value(func, defined, *lhs, errors);
            use_value(func, defined, *rhs, errors);
            if let Some(r) = result_ty {
                expect(MirType::ObjectRef, r, errors);
            }
        }
        Inst::IntToString { value } => {
            use_value(func, defined, *value, errors);
            if let Some(r) = result_ty {
                expect(MirType::ObjectRef, r, errors);
            }
        }
        Inst::Alloc { .. } => {
            if let Some(r) = result_ty {
                expect(MirType::ObjectRef, r, errors);
            }
        }
        Inst::AllocArray { length, .. } => {
            use_value(func, defined, *length, errors);
            if let Some(r) = result_ty {
                expect(MirType::ObjectRef, r, errors);
            }
        }
        Inst::ArrayLoad { array, index, .. } => {
            use_value(func, defined, *array, errors);
            use_value(func, defined, *index, errors);
        }
        Inst::ArrayStore {
            array,
            index,
            value,
            ..
        } => {
            use_value(func, defined, *array, errors);
            use_value(func, defined, *index, errors);
            use_value(func, defined, *value, errors);
        }
        Inst::AllocArray2D { dim0, dim1, .. } => {
            use_value(func, defined, *dim0, errors);
            use_value(func, defined, *dim1, errors);
            if let Some(r) = result_ty {
                expect(MirType::ObjectRef, r, errors);
            }
        }
        Inst::Array2DLoad {
            array,
            index0,
            index1,
            ..
        } => {
            use_value(func, defined, *array, errors);
            use_value(func, defined, *index0, errors);
            use_value(func, defined, *index1, errors);
        }
        Inst::Array2DStore {
            array,
            index0,
            index1,
            value,
            ..
        } => {
            use_value(func, defined, *array, errors);
            use_value(func, defined, *index0, errors);
            use_value(func, defined, *index1, errors);
            use_value(func, defined, *value, errors);
        }
        Inst::StaticLoad { .. } => {
        }
        Inst::StaticStore { value, .. } => {
            use_value(func, defined, *value, errors);
        }
    }
}

fn check_terminator(
    func: &Function,
    defined: &[bool],
    terminator: &Terminator,
    errors: &mut Vec<VerifyError>,
) {
    match terminator {
        Terminator::Jump { target, args } => check_branch(func, defined, *target, args, errors),
        Terminator::Branch {
            cond,
            if_true,
            true_args,
            if_false,
            false_args,
        } => {
            if let Some(cond_ty) = use_value(func, defined, *cond, errors) {
                if !cond_ty.is_integer() {
                    expect(MirType::I32, cond_ty, errors);
                }
            }
            check_branch(func, defined, *if_true, true_args, errors);
            check_branch(func, defined, *if_false, false_args, errors);
        }
        Terminator::Return(value) => {
            let found = match value {
                Some(v) => {
                    use_value(func, defined, *v, errors);
                    func.value_type(*v)
                }
                None => None,
            };
            if found != func.ret {
                errors.push(VerifyError::BadReturnType);
            }
        }
        Terminator::Unreachable => {}
    }
}

fn check_branch(
    func: &Function,
    defined: &[bool],
    target: BlockId,
    args: &[ValueId],
    errors: &mut Vec<VerifyError>,
) {
    let Some(block) = func.block(target) else {
        errors.push(VerifyError::BadBlockRef(target));
        return;
    };
    if args.len() != block.params.len() {
        errors.push(VerifyError::ArgCountMismatch {
            target,
            expected: block.params.len(),
            found: args.len(),
        });
    }
    for (i, &arg) in args.iter().enumerate() {
        let arg_ty = use_value(func, defined, arg, errors);
        if let Some(&param) = block.params.get(i) {
            if let (Some(a), Some(p)) = (arg_ty, func.value_type(param)) {
                expect(p, a, errors);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::function::BasicBlock;
    use crate::inst::BinOp;

    /// `fn(i32, i32) -> i32 { return arg0 + arg1 }`.
    fn add_function() -> Function {
        Function {
            params: vec![MirType::I32, MirType::I32],
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
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
        }
    }

    #[test]
    fn verify_never_panics_on_malformed_functions() {
        let cases = [
            Function {
                params: Vec::new(),
                ret: None,
                value_types: Vec::new(),
                entry: BlockId(9),
                blocks: Vec::new(),
            },
            Function {
                params: Vec::new(),
                ret: None,
                value_types: Vec::new(),
                entry: BlockId(0),
                blocks: vec![BasicBlock {
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: None,
                }],
            },
            Function {
                params: Vec::new(),
                ret: None,
                value_types: Vec::new(),
                entry: BlockId(0),
                blocks: vec![BasicBlock {
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Some(Terminator::Jump {
                        target: BlockId(9),
                        args: vec![ValueId(7)],
                    }),
                }],
            },
            Function {
                params: Vec::new(),
                ret: Some(MirType::I32),
                value_types: vec![MirType::I32],
                entry: BlockId(0),
                blocks: vec![BasicBlock {
                    params: Vec::new(),
                    insts: vec![(
                        ValueId(0),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(50),
                            rhs: ValueId(99),
                        },
                    )],
                    terminator: Some(Terminator::Return(Some(ValueId(0)))),
                }],
            },
        ];
        for case in &cases {
            let _ = verify(case);
        }
    }

    #[test]
    fn well_formed_add_verifies() {
        assert_eq!(verify(&add_function()), Ok(()));
    }

    #[test]
    fn py_intrinsic_getattr_yields_a_py_value() {
        let mut f = Function {
            params: vec![MirType::PyValue],
            ret: Some(MirType::PyValue),
            value_types: vec![MirType::PyValue, MirType::PyValue],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::PyIntrinsic {
                        op: crate::inst::PyOp::Getattr { name: 3 },
                        args: vec![ValueId(0)],
                        cache: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert_eq!(verify(&f), Ok(()));
        f.value_types[1] = MirType::I32;
        f.ret = Some(MirType::I32);
        assert!(
            verify(&f)
                .unwrap_err()
                .iter()
                .any(|e| matches!(e, VerifyError::TypeMismatch { .. }))
        );
    }

    #[test]
    fn alloc_must_produce_an_object_ref() {
        let mut f = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: crate::types::TypeHandle(1),
                        payload_size: 8,
                        ref_offsets: vec![4u32].into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        assert_eq!(verify(&f), Ok(()));
        f.value_types[0] = MirType::I32;
        f.ret = Some(MirType::I32);
        assert!(verify(&f).is_err());
    }

    #[test]
    fn missing_terminator_is_caught() {
        let mut f = add_function();
        f.blocks[0].terminator = None;
        assert_eq!(
            verify(&f),
            Err(vec![VerifyError::MissingTerminator(BlockId(0))])
        );
    }

    #[test]
    fn undefined_operand_is_caught() {
        let mut f = add_function();
        f.blocks[0].insts[0].1 = Inst::Binary {
            op: BinOp::Add,
            lhs: ValueId(9),
            rhs: ValueId(1),
        };
        assert!(
            verify(&f)
                .unwrap_err()
                .contains(&VerifyError::UndefinedValue(ValueId(9)))
        );
    }

    #[test]
    fn return_type_mismatch_is_caught() {
        let mut f = add_function();
        f.ret = Some(MirType::I64);
        assert!(
            verify(&f)
                .unwrap_err()
                .contains(&VerifyError::BadReturnType)
        );
    }

    #[test]
    fn argument_type_mismatch_is_caught() {
        let mut f = add_function();
        f.value_types = vec![MirType::I32, MirType::I32, MirType::I32, MirType::I64];
        f.blocks.push(BasicBlock {
            params: vec![ValueId(3)],
            insts: Vec::new(),
            terminator: Some(Terminator::Unreachable),
        });
        f.blocks[0].terminator = Some(Terminator::Jump {
            target: BlockId(1),
            args: vec![ValueId(2)],
        });
        let errors = verify(&f).unwrap_err();
        assert!(errors.iter().any(|e| matches!(
            e,
            VerifyError::TypeMismatch {
                expected: MirType::I64,
                found: MirType::I32
            }
        )));
    }

    #[test]
    fn shift_count_may_be_narrower_than_the_value() {
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I64),
            value_types: vec![MirType::I64, MirType::I32, MirType::I64],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I64,
                            value: 1,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 5,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Binary {
                            op: BinOp::Shl,
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        assert_eq!(verify(&func), Ok(()));
        let mut bad = func;
        bad.blocks[0].insts[2].1 = Inst::Binary {
            op: BinOp::Add,
            lhs: ValueId(0),
            rhs: ValueId(1),
        };
        assert!(verify(&bad).is_err());
    }
}
