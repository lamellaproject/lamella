use crate::intrinsics::md_array_set;
use crate::{CastElem, CastPrim, Module, ObjectRef, Trap, Value, Vm, asm_key};
use alloc::{boxed::Box, vec, vec::Vec};
use lamella_token::Token;

fn shape(module: &mut Module, row: u32, element: CastElem, rank: u32) -> u64 {
    let token = Token(0x0a00_0000 | row);
    module.bind_cast_elem(
        1,
        token,
        CastElem::MdArray {
            element: Box::new(element),
            rank,
        },
    );
    asm_key(1, token.0)
}

fn put(
    vm: &mut Vm,
    module: &Module,
    array: ObjectRef,
    value: Value,
) -> Result<Option<Value>, Trap> {
    md_array_set(
        vm,
        module,
        &[
            Value::Object(array),
            Value::Int32(0),
            Value::Int32(0),
            value,
        ],
    )
}

#[test]
fn rectangular_set_rejects_incompatible_references_without_mutating_storage() {
    for frozen in [false, true] {
        let mut module = Module::new();
        let strings = shape(&mut module, 1, CastElem::String, 2);
        let base = module.add_type(vec![]);
        let derived = module.add_type(vec![]);
        let other = module.add_type(vec![]);
        let interface = module.add_type(vec![]);
        module.set_type_base(derived, Some(base));
        module.set_type_interfaces(derived, vec![interface]);
        module.bind_type_token(0, Token(0x0200_0001), base);
        module.bind_type_token(0, Token(0x0200_0002), interface);
        let bases = shape(&mut module, 2, CastElem::Named(asm_key(0, 0x0200_0001)), 2);
        let interfaces = shape(&mut module, 3, CastElem::Named(asm_key(0, 0x0200_0002)), 2);
        let vectors = shape(
            &mut module,
            4,
            CastElem::Array(Box::new(CastElem::String)),
            2,
        );
        let matrices = shape(
            &mut module,
            5,
            CastElem::MdArray {
                element: Box::new(CastElem::String),
                rank: 2,
            },
            2,
        );
        let cubes = shape(&mut module, 6, CastElem::String, 3);
        module.bind_cast_elem(0, Token(0x0100_0001), CastElem::String);
        module.bind_cast_elem(0, Token(0x0100_0002), CastElem::Object);
        if frozen {
            module.freeze();
        }
        let mut vm = Vm::new();
        let text = Value::Object(vm.heap_mut().alloc_string(&[111, 107]).unwrap());
        let boxed = Value::Object(vm.heap_mut().alloc_boxed(0x0100_0003, Value::Int32(42)));
        let child = Value::Object(vm.heap_mut().alloc_instance(derived, vec![]));
        let unrelated = Value::Object(vm.heap_mut().alloc_instance(other, vec![]));
        let string_vector = Value::Object(
            vm.heap_mut()
                .alloc_typed_array(vec![], asm_key(0, 0x0100_0001)),
        );
        let object_vector = Value::Object(
            vm.heap_mut()
                .alloc_typed_array(vec![], asm_key(0, 0x0100_0002)),
        );
        let string_matrix = Value::Object(vm.heap_mut().alloc_md_array_typed(vec![1, 1], strings));
        let string_cube = Value::Object(vm.heap_mut().alloc_md_array_typed(vec![1, 1, 1], cubes));
        for (name, target, accepted, rejected) in [
            ("string", strings, text.clone(), boxed.clone()),
            ("base class", bases, child.clone(), unrelated.clone()),
            ("interface", interfaces, child, unrelated),
            (
                "vector element",
                vectors,
                string_vector.clone(),
                object_vector,
            ),
            (
                "matrix element rank",
                matrices,
                string_matrix.clone(),
                string_cube,
            ),
            (
                "vector versus matrix",
                vectors,
                string_vector,
                string_matrix,
            ),
            ("array versus scalar", matrices, Value::Null, text),
        ] {
            let array = vm.heap_mut().alloc_md_array_typed(vec![1, 1], target);
            assert_eq!(
                put(&mut vm, &module, array, accepted.clone()),
                Ok(None),
                "{name}, frozen={frozen}"
            );
            assert_eq!(
                put(&mut vm, &module, array, rejected),
                Err(Trap::ArrayTypeMismatch),
                "{name}, frozen={frozen}"
            );
            assert_eq!(
                vm.heap().md_array_get(array, &[0, 0]),
                Some(accepted),
                "{name}, frozen={frozen}"
            );
        }
    }
}

#[test]
fn rectangular_set_preserves_valid_stores_and_bounds_checks() {
    for frozen in [false, true] {
        let mut module = Module::new();
        let strings = shape(&mut module, 1, CastElem::String, 2);
        let objects = shape(&mut module, 2, CastElem::Object, 2);
        let integers = shape(&mut module, 3, CastElem::Prim(CastPrim::I4), 2);
        if frozen {
            module.freeze();
        }
        let mut vm = Vm::new();
        let text = Value::Object(vm.heap_mut().alloc_string(&[111, 107]).unwrap());
        let boxed = Value::Object(vm.heap_mut().alloc_boxed(0x0100_0003, Value::Int32(42)));
        let instance = Value::Object(vm.heap_mut().alloc_instance(123, Vec::new()));
        for (target, values) in [
            (strings, vec![text.clone(), Value::Null]),
            (objects, vec![text, boxed, instance, Value::Null]),
            (integers, vec![Value::Int32(42), Value::Int32(-7)]),
        ] {
            let array = vm.heap_mut().alloc_md_array_typed(vec![1, 1], target);
            for value in values {
                assert_eq!(put(&mut vm, &module, array, value.clone()), Ok(None));
                assert_eq!(vm.heap().md_array_get(array, &[0, 0]), Some(value));
            }
            assert_eq!(
                md_array_set(
                    &mut vm,
                    &module,
                    &[
                        Value::Object(array),
                        Value::Int32(1),
                        Value::Int32(0),
                        Value::Null,
                    ]
                ),
                Err(Trap::IndexOutOfRange(1))
            );
        }
        assert_eq!(
            md_array_set(
                &mut vm,
                &module,
                &[Value::Null, Value::Int32(0), Value::Int32(0), Value::Null,]
            ),
            Err(Trap::NullReference)
        );
    }
}
