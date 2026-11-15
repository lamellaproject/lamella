use super::*;
use crate::module::{MethodParam, ReflectType};
use alloc::{boxed::Box, vec};
use lamella_token::Token;

const NAMED: u64 = 0x0200_0001;
const OWNER: u64 = 0x0200_0002;
const MEMBER: u64 = 0x0400_0001;

fn storage_modes() -> &'static [u8] {
    #[cfg(feature = "code-in-place")]
    { &[0, 1, 2] }
    #[cfg(not(feature = "code-in-place"))]
    { &[0, 1] }
}

fn stored(mut module: Module, mode: u8) -> Module {
    if mode == 1 { module.freeze(); }
    #[cfg(feature = "code-in-place")]
    if mode == 2 {
        let image = module.write_baked(None).expect("bake type records");
        return Module::from_baked(Box::leak(image.into_boxed_slice())).expect("read type records").0;
    }
    module
}

fn fixture(asm: u8) -> (Module, u64) {
    let mut module = Module::new();
    let named = module.add_type(vec![]);
    let owner = module.add_type(vec![]);
    let alias = crate::asm_key(asm, 0x0100_0007);
    module.bind_type_token(0, Token(NAMED as u32), named);
    module.bind_type_token(asm, Token(alias as u32), named);
    module.bind_type_token(0, Token(OWNER as u32), owner);
    module.bind_reflect_type(alias, ReflectType {
        full_name: String::from("Sample.Item"), ..ReflectType::default()
    });
    module.bind_reflect_type(OWNER, ReflectType {
        full_name: String::from("Sample.Owner"), base_handle: alias, ..ReflectType::default()
    });
    module.bind_member_type(MEMBER, alias);
    module.bind_method_params(MEMBER, vec![MethodParam { type_handle: alias, name: String::from("value") }]);
    module.add_assembly_type(asm, alias);
    (module, alias)
}

fn assert_named(value: Result<Option<Value>, Trap>) {
    // Compare the returned references directly, as ceq does, without an equality intrinsic.
    assert_eq!(value, Ok(Some(Value::NativeInt(NAMED as i64))));
}

#[test]
fn type_identity_box_and_instance_producers_agree_with_typeof() {
    for &mode in storage_modes() {
        for asm in [0, 1] {
            let (module, alias) = fixture(asm);
            let module = stored(module, mode);
            let mut vm = Vm::new();
            assert_named(type_from_handle(&mut vm, &module, &[Value::NativeInt(alias as i64)]));
            for value in [Value::Int32(42), Value::Struct(vec![Value::Int32(7)].into_boxed_slice())] {
                let boxed = vm.heap_mut().alloc_boxed(alias, value);
                assert_named(object_get_type(&mut vm, &module, &[Value::Object(boxed)]));
            }
            let instance = vm.heap_mut().alloc_instance(0, vec![]);
            assert_named(object_get_type(&mut vm, &module, &[Value::Object(instance)]));
            let delegate = vm.heap_mut().alloc_delegate(Value::Null, 0, Some(0));
            assert_named(object_get_type(&mut vm, &module, &[Value::Object(delegate)]));
            let other = vm.heap_mut().alloc_instance(1, vec![]);
            assert_eq!(object_get_type(&mut vm, &module, &[Value::Object(other)]),
                Ok(Some(Value::NativeInt(OWNER as i64))));
        }
    }
}

#[cfg(feature = "reflection")]
#[test]
fn type_identity_member_types_intern_named_and_structural_tokens_in_either_order() {
    for &mode in storage_modes() {
        for asm in [0, 1] {
            for reverse in [false, true] {
                let (mut module, alias) = fixture(asm);
                for (row, element) in [CastElem::Prim(crate::CastPrim::I4), CastElem::String].into_iter().enumerate() {
                    let first = Token(0x1b00_0001 + row as u32);
                    let second = Token(0x1b00_0011 + row as u32);
                    let shape = CastElem::Array(Box::new(element));
                    module.bind_cast_elem(0, first, shape.clone());
                    module.bind_cast_elem(asm, second, shape);
                    module.bind_member_type(MEMBER + 1 + row as u64, crate::asm_key(asm, second.0));
                }
                let module = stored(module, mode);
                let mut vm = Vm::new();
                assert_named(member_get_type(&mut vm, &module, &[Value::NativeInt(MEMBER as i64)]));
                assert_named(type_from_handle(&mut vm, &module, &[Value::NativeInt(alias as i64)]));
                let mut distinct = Vec::new();
                for row in 0..2 {
                    let member_args = [Value::NativeInt((MEMBER + 1 + row) as i64)];
                    let type_args = [Value::NativeInt(0x1b00_0001 + row as i64)];
                    let (first, second) = if reverse {
                        let first = member_get_type(&mut vm, &module, &member_args);
                        (first, type_from_handle(&mut vm, &module, &type_args))
                    } else {
                        let first = type_from_handle(&mut vm, &module, &type_args);
                        (first, member_get_type(&mut vm, &module, &member_args))
                    };
                    assert_eq!(first, second, "mode={mode}, asm={asm}, reverse={reverse}");
                    assert!(!distinct.contains(&first), "different array elements retain distinct types");
                    distinct.push(first);
                }
                assert_eq!(member_get_type(&mut vm, &module, &[Value::NativeInt(99)]), Ok(Some(Value::Null)));
            }
        }
    }
}

#[test]
fn type_identity_base_type_uses_the_same_reference() {
    for &mode in storage_modes() {
        for asm in [0, 1] {
            let (module, _) = fixture(asm);
            let module = stored(module, mode);
            let mut vm = Vm::new();
            assert_named(type_get_base_type(&mut vm, &module, &[Value::NativeInt(OWNER as i64)]));
        }
    }
}

#[cfg(feature = "reflection")]
#[test]
fn type_identity_assembly_name_lookup_uses_the_same_reference() {
    for &mode in storage_modes() {
        for asm in [0, 1] {
            let (module, _) = fixture(asm);
            let module = stored(module, mode);
            let mut vm = Vm::new();
            let name = vm.heap_mut().alloc_text("Sample.Item");
            assert_named(assembly_get_type(&mut vm, &module, &[Value::NativeInt(0), Value::Object(name)]));
        }
    }
}

#[cfg(feature = "reflection")]
#[test]
fn type_identity_assembly_enumeration_uses_the_same_reference() {
    for &mode in storage_modes() {
        for asm in [0, 1] {
            let (module, _) = fixture(asm);
            let module = stored(module, mode);
            let mut vm = Vm::new();
            let value = assembly_get_types(&mut vm, &module, &[Value::NativeInt((u64::from(asm) << 32) as i64)]).unwrap();
            let Some(Value::Object(array)) = value else { panic!("type array") };
            assert_eq!(vm.heap().array_get(array, 0), Some(Value::NativeInt(NAMED as i64)));
        }
    }
}

#[cfg(feature = "reflection")]
#[test]
fn type_identity_parameter_types_use_the_same_reference() {
    for &mode in storage_modes() {
        for asm in [0, 1] {
            let (module, _) = fixture(asm);
            let module = stored(module, mode);
            let mut vm = Vm::new();
            assert_named(method_parameter_type(&mut vm, &module, &[Value::NativeInt(MEMBER as i64), Value::Int32(0)]));
            for index in [-1, 1] {
                assert_eq!(method_parameter_type(&mut vm, &module, &[Value::NativeInt(MEMBER as i64), Value::Int32(index)]), Ok(Some(Value::Null)));
            }
        }
    }
}

#[test]
fn type_identity_attribute_values_and_arrays_use_the_same_reference() {
    for &mode in storage_modes() {
        for asm in [0, 1] {
            let (module, alias) = fixture(asm);
            let module = stored(module, mode);
            let mut vm = Vm::new();
            let expected = type_from_handle(&mut vm, &module, &[Value::NativeInt(NAMED as i64)]).unwrap().unwrap();
            assert_eq!(materialize_attr_value(&mut vm, &module, &AttrValue::Type(alias)), Ok(expected.clone()));
            let array = AttrValue::Array(vec![AttrValue::Type(alias), AttrValue::Null].into_boxed_slice());
            let Value::Object(array) = materialize_attr_value(&mut vm, &module, &array).unwrap() else {
                panic!("attribute Type array");
            };
            assert_eq!(vm.heap().array_get(array, 0), Some(expected));
            assert_eq!(vm.heap().array_get(array, 1), Some(Value::Null));
        }
    }
}
