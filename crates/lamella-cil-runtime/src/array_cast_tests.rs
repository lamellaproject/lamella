use crate::interp::cast_matches;
use crate::module::ReflectType;
use crate::{CastElem, CastPrim, Module, Value, Vm, asm_key, run};
use alloc::{boxed::Box, format, string::String, vec, vec::Vec};
use lamella_cil::{Instruction, MethodBodyImage, Opcode, Operand};
use lamella_token::Token;

const BASES: &[(&str, bool)] = &[
    ("System.Array", false),
    ("System.ICloneable", true),
    ("System.Collections.IEnumerable", true),
    ("System.Collections.ICollection", true),
    ("System.Collections.IList", true),
    ("System.Collections.IStructuralComparable", true),
    ("System.Collections.IStructuralEquatable", true),
];

#[derive(Clone, Copy, Debug)]
enum Reflection { Absent, Complete, Partial }

const REFLECTION_TIERS: &[Reflection] = &[
    Reflection::Absent, Reflection::Complete, Reflection::Partial,
];

fn named(row: u32) -> Token { Token(0x0200_0000 | row) }
fn shape(row: u32) -> Token { Token(0x1b00_0000 | row) }

fn fixture(reflection: Reflection) -> Module {
    let mut module = Module::new();
    for (index, &(name, interface)) in BASES.iter().chain([
        &("System.IComparable", true),
        &("Sample.Array", false),
        &("Sample.IExtra", true),
    ]).enumerate() {
        let token = named(index as u32 + 1);
        let id = module.add_type(Vec::new());
        for asm in [0, 1] {
            module.bind_type_token(asm, token, id);
            module.bind_cast_elem(asm, token, CastElem::Named(asm_key(asm, token.0)));
            if index < BASES.len() { module.mark_array_base_name(asm, token); }
        }
        if !matches!(reflection, Reflection::Absent) {
            module.bind_reflect_type(asm_key(0, token.0), ReflectType {
                full_name: String::from(name), is_interface: interface, ..ReflectType::default()
            });
        }
    }
    // Casts must retain the runtime's array interfaces even when managed metadata omits them.
    // An additional interface explicitly declared by System.Array still participates normally.
    let extra = BASES.len() as u32 + 2;
    let mut interfaces = match reflection {
        Reflection::Complete => (1..BASES.len() as u32).collect::<Vec<_>>(),
        _ => vec![1, 2],
    };
    interfaces.push(extra);
    module.set_type_interfaces(0, interfaces);
    for asm in [0, 1] {
        for (row, element) in [
            (1, CastElem::Prim(CastPrim::I4)),
            (2, CastElem::Array(Box::new(CastElem::Prim(CastPrim::I4)))),
            (3, CastElem::Array(Box::new(CastElem::Prim(CastPrim::I8)))),
            (4, CastElem::VectorInterface(Box::new(CastElem::Prim(CastPrim::I4)))),
            (5, CastElem::VectorInterface(Box::new(CastElem::Prim(CastPrim::I8)))),
            (6, CastElem::MdArray { element: Box::new(CastElem::Prim(CastPrim::I4)), rank: 1 }),
            (7, CastElem::MdArray { element: Box::new(CastElem::Prim(CastPrim::I4)), rank: 2 }),
            (8, CastElem::MdArray { element: Box::new(CastElem::Prim(CastPrim::I8)), rank: 2 }),
            (9, CastElem::String),
            (10, CastElem::VectorInterface(Box::new(CastElem::Object))),
        ] {
            module.bind_cast_elem(asm, shape(row), element);
        }
    }
    module
}

fn stored(mut module: Module, mode: u8) -> Module {
    if mode == 1 { module.freeze(); }
    #[cfg(feature = "code-in-place")]
    if mode == 2 {
        let bytes = module.write_baked(None).unwrap();
        return Module::from_baked(Box::leak(bytes.into_boxed_slice())).unwrap().0;
    }
    module
}

fn storage_modes() -> &'static [u8] {
    #[cfg(feature = "code-in-place")]
    { &[0, 1, 2] }
    #[cfg(not(feature = "code-in-place"))]
    { &[0, 1] }
}

fn arrays(vm: &mut Vm) -> [Value; 3] {
    [
        Value::Object(vm.heap_mut().alloc_typed_array(vec![Value::Int32(0)], asm_key(0, shape(1).0))),
        Value::Object(vm.heap_mut().alloc_md_array_typed(vec![1], asm_key(0, shape(6).0))),
        Value::Object(vm.heap_mut().alloc_md_array_typed(vec![1, 1], asm_key(0, shape(7).0))),
    ]
}

fn check_array_bases(kinds: &[usize]) {
    for &reflection in REFLECTION_TIERS {
        for &mode in storage_modes() {
            let module = stored(fixture(reflection), mode);
            let mut vm = Vm::new();
            for (kind, array) in arrays(&mut vm).iter().enumerate() {
                if !kinds.contains(&kind) { continue; }
                for asm in [0, 1] {
                    for (index, &(name, _)) in BASES.iter().enumerate() {
                        assert!(cast_matches(&module, asm, &vm, array, named(index as u32 + 1)),
                            "{name}, array={kind}, asm={asm}, reflection={reflection:?}, storage={mode}");
                    }
                }
            }
        }
    }
}

#[test]
fn vector_array_bases_do_not_depend_on_optional_reflected_declarations() {
    check_array_bases(&[0]);
}

#[test]
fn same_named_types_are_not_array_bases() {
    let mut failures = Vec::new();
    for &reflection in REFLECTION_TIERS {
        for &mode in storage_modes() {
            let mut module = fixture(reflection);
            for (index, &(name, interface)) in BASES.iter().enumerate() {
                let token = named(index as u32 + 1);
                let id = module.add_type(Vec::new());
                module.bind_type_token(2, token, id);
                module.bind_cast_elem(2, token, CastElem::Named(asm_key(2, token.0)));
                if !matches!(reflection, Reflection::Absent) {
                    module.bind_reflect_type(asm_key(2, token.0), ReflectType {
                        full_name: String::from(name), is_interface: interface, ..ReflectType::default()
                    });
                }
            }
            let module = stored(module, mode);
            let mut vm = Vm::new();
            for (kind, array) in arrays(&mut vm).iter().enumerate() {
                for (index, &(name, _)) in BASES.iter().enumerate() {
                    if cast_matches(&module, 2, &vm, array, named(index as u32 + 1)) {
                        failures.push(format!("{name}, array={kind}, reflection={reflection:?}, storage={mode}"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn nested_arrays_obey_the_same_named_base_rules() {
    let mut failures = Vec::new();
    for &reflection in REFLECTION_TIERS {
        for &mode in storage_modes() {
            for inner_rank in [None, Some(1), Some(2)] {
                let mut module = fixture(reflection);
                let inner = match inner_rank {
                    None => CastElem::Array(Box::new(CastElem::Prim(CastPrim::I4))),
                    Some(rank) => CastElem::MdArray { element: Box::new(CastElem::Prim(CastPrim::I4)), rank },
                };
                module.bind_cast_elem(0, shape(200), inner.clone());
                module.bind_cast_elem(0, shape(201), CastElem::MdArray { element: Box::new(inner), rank: 2 });
                let mut targets = Vec::new();
                for (index, &(name, interface)) in BASES.iter().enumerate() {
                    let token = named(index as u32 + 1);
                    let id = module.add_type(Vec::new());
                    module.bind_type_token(2, token, id);
                    module.bind_cast_elem(2, token, CastElem::Named(asm_key(2, token.0)));
                    if !matches!(reflection, Reflection::Absent) {
                        module.bind_reflect_type(asm_key(2, token.0), ReflectType {
                            full_name: String::from(name), is_interface: interface, ..ReflectType::default()
                        });
                    }
                    targets.push((asm_key(2, token.0), false));
                }
                for index in 0..BASES.len() + 3 {
                    let allowed = index < BASES.len()
                        || (index == BASES.len() + 2 && !matches!(reflection, Reflection::Absent));
                    targets.push((asm_key(1, named(index as u32 + 1).0), allowed));
                }
                // An array element can itself satisfy a generic vector interface.
                targets.extend([(asm_key(1, shape(4).0), inner_rank.is_none()),
                    (asm_key(1, shape(5).0), false), (asm_key(1, shape(10).0), false)]);
                for (index, &(handle, _)) in targets.iter().enumerate() {
                    let element = Box::new(CastElem::Named(handle));
                    module.bind_cast_elem(1, shape(300 + index as u32), CastElem::Array(element.clone()));
                    module.bind_cast_elem(1, shape(400 + index as u32), CastElem::MdArray { element, rank: 2 });
                }
                let module = stored(module, mode);
                let mut vm = Vm::new();
                let outer = [
                    Value::Object(vm.heap_mut().alloc_typed_array(vec![Value::Null], asm_key(0, shape(200).0))),
                    Value::Object(vm.heap_mut().alloc_md_array_typed(vec![1, 1], asm_key(0, shape(201).0))),
                ];
                for (kind, array) in outer.iter().enumerate() {
                    for (index, &(handle, allowed)) in targets.iter().enumerate() {
                        let token = shape(300 + kind as u32 * 100 + index as u32);
                        let actual = cast_matches(&module, 1, &vm, array, token);
                        if actual != allowed {
                            failures.push(format!("target={handle:x}, allowed={allowed}, outer={kind}, inner={inner_rank:?}, reflection={reflection:?}, storage={mode}"));
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn rectangular_array_bases_do_not_depend_on_optional_reflected_declarations() {
    check_array_bases(&[1, 2]);
}

fn check_resolved_array_bases(reflection: Reflection, named_payload: bool) {
    let mut failures = Vec::new();
    for &mode in storage_modes() {
        let mut module = fixture(reflection);
        let mut sites = Vec::new();
        for (index, &(name, _)) in BASES.iter().enumerate() {
            let token = shape(100 + index as u32);
            let id = module.type_id_of(0, named(index as u32 + 1)).unwrap();
            module.bind_type_token(1, token, id);
            if named_payload {
                // A generic body's synthetic operand carries the caller's unmarked TypeRef.
                let reference = Token(0x0100_0000 | (index as u32 + 1));
                module.bind_cast_elem(1, token, CastElem::Named(asm_key(1, reference.0)));
            }
            for opcode in [Opcode::Isinst, Opcode::Castclass] {
                let method = module.add_method_image(1, MethodBodyImage {
                    max_stack: 1, init_locals: false, local_var_sig: None,
                    code: vec![
                        Instruction::simple(Opcode::Ldarg0),
                        Instruction::new(opcode, Operand::Token(token)),
                        Instruction::simple(Opcode::Ret),
                    ].into_boxed_slice(),
                    handlers: Box::default(),
                }, 0);
                sites.push((name, opcode, method));
            }
        }
        let module = stored(module, mode);
        let mut vm = Vm::new();
        for (kind, array) in arrays(&mut vm).iter().enumerate() {
            for &(name, opcode, method) in &sites {
                let result = run(&module, &mut vm, method, vec![array.clone()]);
                if result != Ok(Some(array.clone())) {
                    failures.push(format!(
                        "{name}, {opcode:?}, array={kind}, reflection={reflection:?}, storage={mode}: {result:?}"
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn generic_array_bases_without_reflection() {
    check_resolved_array_bases(Reflection::Absent, true);
}

#[test]
fn generic_array_bases_with_complete_reflection() {
    check_resolved_array_bases(Reflection::Complete, true);
}

#[test]
fn generic_array_bases_with_partial_reflection() {
    check_resolved_array_bases(Reflection::Partial, true);
}

#[test]
fn resolved_array_bases_without_cast_records_or_reflection() {
    check_resolved_array_bases(Reflection::Absent, false);
}

#[test]
fn resolved_array_bases_without_cast_records_with_complete_reflection() {
    check_resolved_array_bases(Reflection::Complete, false);
}

#[test]
fn resolved_array_bases_without_cast_records_with_partial_reflection() {
    check_resolved_array_bases(Reflection::Partial, false);
}

#[test]
fn array_bases_preserve_unrelated_type_rank_and_element_guards() {
    for &reflection in REFLECTION_TIERS {
        for &mode in storage_modes() {
            let module = stored(fixture(reflection), mode);
            let mut vm = Vm::new();
            for (kind, array) in arrays(&mut vm).iter().enumerate() {
                for asm in [0, 1] {
                    for token in [named(BASES.len() as u32 + 1), named(BASES.len() as u32 + 2)] {
                        assert!(!cast_matches(&module, asm, &vm, array, token));
                    }
                    assert_eq!(cast_matches(&module, asm, &vm, array, named(BASES.len() as u32 + 3)),
                        !matches!(reflection, Reflection::Absent));
                    for (row, allowed) in [
                        (2, kind == 0), (3, false), (4, kind == 0), (5, false),
                        (6, kind == 1), (7, kind == 2), (8, false), (9, false), (10, false),
                    ] {
                        assert_eq!(cast_matches(&module, asm, &vm, array, shape(row)), allowed,
                            "array={kind}, target={row}, asm={asm}, reflection={reflection:?}, storage={mode}");
                    }
                }
            }
            let strings = Value::Object(vm.heap_mut().alloc_typed_array(vec![Value::Null], asm_key(0, shape(9).0)));
            assert!(cast_matches(&module, 1, &vm, &strings, shape(10)), "reference-element covariance");
            assert!(!cast_matches(&module, 1, &vm, &strings, shape(4)), "string is not int");
        }
    }
}
