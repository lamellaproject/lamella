//! Captured generic receivers preserve the storage a value-type instantiation updates.

use super::*;
use crate::symbols::TypeParameterConstraints;
use lamella_syntax::parser::parse_expression;

fn parameter_binder(constraints: TypeParameterConstraints) -> (Binder, TypeSymbol) {
    let mut binder = Binder::new();
    binder.enter_type_parameter_names_with(&["T".into()], &[constraints]);
    binder.enter_scope();
    let ty = TypeSymbol::Named(vec!["T".into()].into_boxed_slice());
    binder.declare_local("box", ty.clone());
    binder.declare_local(
        "boxes",
        TypeSymbol::Array {
            element: Box::new(ty.clone()),
            rank: 1,
        },
    );
    (binder, ty)
}

fn assert_location(receiver: &BoundExpr, captures: &[BoundExpr], original: &BoundExpr) {
    assert_eq!(captures.len(), 1);
    let BoundExprKind::Ref {
        operand,
        out: false,
    } = &captures[0].kind
    else {
        panic!("the capture must store the receiver's location: {captures:?}");
    };
    assert_eq!(operand, &Box::new(original.clone()));
    let BoundExprKind::Dereference { operand } = &receiver.kind else {
        panic!("the captured receiver must read through its location: {receiver:?}");
    };
    assert!(matches!(operand.kind, BoundExprKind::Temp(0)));
    assert_eq!(operand.ty, TypeSymbol::ByRef(Box::new(original.ty.clone())));
}

#[test]
fn generic_receiver_capture_keeps_local_and_array_storage() {
    for constraints in [
        TypeParameterConstraints::default(),
        TypeParameterConstraints {
            value_type: true,
            ..Default::default()
        },
    ] {
        let (mut binder, _) = parameter_binder(constraints);
        for text in ["box", "boxes[0]"] {
            let original = binder.bind_expression(&parse_expression(text).expr);
            assert!(
                binder.diagnostics().is_empty(),
                "{:?}",
                binder.diagnostics()
            );
            let mut captures = Vec::new();
            let captured = binder.spill_named_receiver(original.clone(), &mut captures);
            assert_location(&captured, &captures, &original);
        }
    }
}

#[test]
fn generic_property_capture_keeps_the_receiver_location() {
    let (mut binder, ty) = parameter_binder(TypeParameterConstraints::default());
    let original = binder.bind_expression(&parse_expression("box").expr);
    let property = BoundExpr {
        ty: binder.nullable_of(&TypeSymbol::Special(SpecialType::Boolean)),
        kind: BoundExprKind::PropertyAccess {
            receiver: Box::new(original.clone()),
            declaring_type: ty.clone(),
            setter_declaring_type: ty,
            getter_instantiation: None,
            setter_instantiation: None,
            name: "P".into(),
        },
    };
    let mut captures = Vec::new();
    let captured = binder.capture_compound_target(property, &mut captures);
    let BoundExprKind::PropertyAccess { receiver, .. } = &captured.kind else {
        panic!("property target lost: {captured:?}");
    };
    assert_location(receiver, &captures, &original);
}

#[test]
fn generic_receiver_capture_copies_rvalues_and_known_references() {
    let (binder, ty) = parameter_binder(TypeParameterConstraints::default());
    let original = BoundExpr {
        ty: ty.clone(),
        kind: BoundExprKind::DefaultValue(ty),
    };
    let mut captures = Vec::new();
    let captured = binder.spill_named_receiver(original.clone(), &mut captures);
    assert!(matches!(captured.kind, BoundExprKind::Temp(0)));
    assert_eq!(captures, [original]);

    let (mut binder, _) = parameter_binder(TypeParameterConstraints {
        reference_type: true,
        ..Default::default()
    });
    let original = binder.bind_expression(&parse_expression("box").expr);
    let mut captures = Vec::new();
    let captured = binder.spill_named_receiver(original.clone(), &mut captures);
    assert!(matches!(captured.kind, BoundExprKind::Temp(0)));
    assert_eq!(captures, [original]);
}
