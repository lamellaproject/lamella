use super::*;
use lamella_cil_runtime::{CastElem, CastPrim, Vm, intrinsics, run};
use lamella_pe::ImageBuilder;

fn assembly(image: ImageBuilder, entry: Token) -> Assembly<'static> {
    Assembly::read(Box::leak(
        image.finish(entry, entry.0 == 0).into_boxed_slice(),
    ))
    .unwrap()
}

fn fixture(name: &str, is_value_type: bool, lazy: bool) -> Program {
    let mut corlib = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let object = corlib.add_type("System", "Object", Token(0), 1);
    let value_type = corlib.add_type("System", "ValueType", object, 0x81);
    if name != "Object" {
        corlib.add_type(
            "System",
            name,
            if is_value_type { value_type } else { object },
            1,
        );
    }
    let corlib = assembly(corlib, Token(0));
    let mut caller = ImageBuilder::new("caller.dll", "caller");
    let named = caller.type_ref("System", name);
    caller.add_type("", "Program", Token(0), 1);
    // A type handle is the only use: no newarr or corlib body classifies its defining token.
    let mut code = alloc::vec![0x1a, 0xd0];
    code.extend_from_slice(&named.0.to_le_bytes());
    code.push(0x2a);
    let entry = caller.add_method("Main", &[0, 0, 0x18], &code, 0x16, 0, &[]);
    let caller = assembly(caller, entry);
    if lazy {
        load_program_lazy_corlib_unfrozen(&corlib, &caller).unwrap()
    } else {
        load_with_corlib_unfrozen(&corlib, &caller).unwrap()
    }
}

fn check_array(
    module: &Module,
    entry: MethodId,
    zero: &Value,
    bytes: Option<usize>,
    shape: &CastElem,
) {
    let mut vm = Vm::new();
    let raw = run(module, &mut vm, entry, Vec::new()).unwrap().unwrap();
    let reflected = intrinsics::type_from_handle(&mut vm, module, &[raw.clone()])
        .unwrap()
        .unwrap();
    assert_ne!(
        raw, reflected,
        "the TypeRef must resolve to its defining handle"
    );
    let array =
        intrinsics::array_create_instance(&mut vm, module, &[reflected.clone(), Value::Int32(2)])
            .unwrap()
            .unwrap();
    let Value::Object(array) = array else {
        panic!("CreateInstance must return an array")
    };
    assert_eq!(vm.heap().array_get(array, 0), Some(zero.clone()));
    assert_eq!(vm.heap().array_get(array, 1), Some(zero.clone()));
    assert_eq!(vm.heap().buffer_byte_length(array), bytes);
    let element = vm.heap().array_element_type(array).unwrap();
    assert_eq!(Some(Value::NativeInt(element as i64)), Some(reflected));
    // The allocated array must retain its exact element shape for later casts and stores.
    assert_eq!(module.cast_elem(element), Some(shape.clone()));
}

fn check_tiers(name: &str, zero: Value, bytes: Option<usize>, shape: CastElem) {
    for lazy in [false, true] {
        let mut program = fixture(name, bytes.is_some(), lazy);
        check_array(&program.module, program.entry, &zero, bytes, &shape);
        program.module.freeze();
        check_array(&program.module, program.entry, &zero, bytes, &shape);
        #[cfg(feature = "code-in-place")]
        {
            let baked = program.module.write_baked(Some(program.entry)).unwrap();
            let (module, entry) = Module::from_baked(Box::leak(baked.into_boxed_slice())).unwrap();
            check_array(&module, entry.unwrap(), &zero, bytes, &shape);
        }
    }
}

#[test]
fn canonical_primitive_type_handles_create_zeroed_arrays_on_both_corlib_tiers() {
    for (name, zero, bytes, kind) in [
        ("Byte", Value::Int32(0), 2, CastPrim::U1),
        ("Int32", Value::Int32(0), 8, CastPrim::I4),
        ("UInt32", Value::Int32(0), 8, CastPrim::U4),
        ("Int64", Value::Int64(0), 16, CastPrim::I8),
    ] {
        check_tiers(name, zero, Some(bytes), CastElem::Prim(kind));
    }
}

#[test]
fn canonical_reference_type_handles_keep_array_element_shapes_on_both_corlib_tiers() {
    check_tiers("String", Value::Null, None, CastElem::String);
    check_tiers("Object", Value::Null, None, CastElem::Object);
}

#[test]
fn corlib_ldtoken_array_shapes_keep_reflected_array_casts_exact() {
    use lamella_pe::{TypeSig, type_signature};

    let mut failures = Vec::new();
    for lazy in [false, true] {
        for rank in [0, 1, 2] {
            let element = |primitive| if rank == 0 {
                TypeSig::SzArray(Box::new(primitive))
            } else {
                TypeSig::Array { element: Box::new(primitive), rank }
            };
            let mut corlib = ImageBuilder::new("mscorlib.dll", "mscorlib");
            let object = corlib.add_type("System", "Object", Token(0), 1);
            corlib.add_type("", "Factory", object, 1);
            let shape = corlib.type_spec(&type_signature(&element(TypeSig::Int32)));
            let mut code = alloc::vec![0x1a, 0xd0];
            code.extend_from_slice(&shape.0.to_le_bytes());
            code.push(0x2a);
            // No newarr or type test in this corlib classifies the ldtoken operand.
            corlib.add_method("Handle", &[0, 0, 0x18], &code, 0x16, 0, &[]);
            let corlib = assembly(corlib, Token(0));

            let mut caller = ImageBuilder::new("caller.dll", "caller");
            let factory = caller.type_ref("", "Factory");
            let handle = caller.member_ref(factory, "Handle", &[0, 0, 0x18]);
            caller.add_type("", "Program", Token(0), 1);
            let mut code = alloc::vec![0x1a, 0x28];
            code.extend_from_slice(&handle.0.to_le_bytes());
            code.push(0x2a);
            let entry = caller.add_method("Main", &[0, 0, 0x18], &code, 0x16, 0, &[]);
            let mut tests = Vec::new();
            for (primitive, allowed) in [(TypeSig::Int32, true), (TypeSig::Int64, false)] {
                let target = caller.type_spec(&type_signature(&TypeSig::SzArray(Box::new(element(primitive)))));
                let mut code = alloc::vec![0x1e, 0x02, 0x75];
                code.extend_from_slice(&target.0.to_le_bytes());
                code.push(0x2a);
                let method = caller.add_method(if allowed { "Exact" } else { "WrongWidth" },
                    &[0, 1, 0x1c, 0x1c], &code, 0x16, 0, &[]);
                tests.push((method, allowed));
            }
            let caller = assembly(caller, entry);
            let mut program = if lazy {
                load_program_lazy_corlib_unfrozen(&corlib, &caller).unwrap()
            } else {
                load_with_corlib_unfrozen(&corlib, &caller).unwrap()
            };
            let check = |module: &Module, entry, phase, failures: &mut Vec<String>| {
                let mut vm = Vm::new();
                let raw = run(module, &mut vm, entry, Vec::new()).unwrap().unwrap();
                let reflected = intrinsics::type_from_handle(&mut vm, module, &[raw]).unwrap().unwrap();
                let array = intrinsics::array_create_instance(&mut vm, module, &[reflected, Value::Int32(1)])
                    .unwrap().unwrap();
                for &(token, allowed) in &tests {
                    let method = module.resolve(1, token).unwrap();
                    let result = run(module, &mut vm, method, alloc::vec![array.clone()]);
                    let expected = if allowed { array.clone() } else { Value::Null };
                    if result != Ok(Some(expected)) {
                        failures.push(alloc::format!("lazy={lazy}, rank={rank}, {phase}, allowed={allowed}: {result:?}"));
                    }
                }
            };
            check(&program.module, program.entry, "live", &mut failures);
            program.module.freeze();
            check(&program.module, program.entry, "frozen", &mut failures);
            #[cfg(feature = "code-in-place")]
            {
                let bytes = program.module.write_baked(Some(program.entry)).unwrap();
                let (module, entry) = Module::from_baked(Box::leak(bytes.into_boxed_slice())).unwrap();
                check(&module, entry.unwrap(), "baked", &mut failures);
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn named_fixture(kind: &str, field_type: u8, lazy: bool) -> Program {
    let mut corlib = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let object = corlib.add_type("System", "Object", Token(0), 1);
    let value_type = corlib.add_type("System", "ValueType", object, 0x81);
    let enum_type = corlib.add_type("System", "Enum", value_type, 0x81);
    let base = match kind {
        "Struct" => value_type,
        "Enum" => enum_type,
        _ => object,
    };
    corlib.add_type("User", "Item", base, 1);
    if kind != "Class" {
        corlib.add_field(
            if kind == "Enum" { "value__" } else { "Value" },
            &[0x06, field_type],
            if kind == "Enum" { 0x606 } else { 6 },
        );
    }
    let corlib = assembly(corlib, Token(0));
    let mut caller = ImageBuilder::new("caller.dll", "caller");
    let named = caller.type_ref("User", "Item");
    caller.add_type("", "Program", Token(0), 1);
    let mut code = alloc::vec![0x1a, 0xd0];
    code.extend_from_slice(&named.0.to_le_bytes());
    code.push(0x2a);
    let entry = caller.add_method("Main", &[0, 0, 0x18], &code, 0x16, 0, &[]);
    let caller = assembly(caller, entry);
    if lazy {
        load_program_lazy_corlib_unfrozen(&corlib, &caller).unwrap()
    } else {
        load_with_corlib_unfrozen(&corlib, &caller).unwrap()
    }
}

fn check_named_array_without_extra_record(module: &Module, entry: MethodId, zero: &Value) {
    let mut vm = Vm::new();
    let raw = run(module, &mut vm, entry, Vec::new()).unwrap().unwrap();
    let reflected = intrinsics::type_from_handle(&mut vm, module, &[raw.clone()])
        .unwrap()
        .unwrap();
    assert_ne!(raw, reflected);
    let Value::NativeInt(handle) = reflected else {
        panic!("Type handle")
    };
    assert!(module.type_id_by_handle(handle as u64).is_some());
    assert_eq!(
        module.cast_elem(handle as u64),
        None,
        "a named type already has an identity; typeof alone must not add a cast record"
    );
    let Some(Value::Object(array)) =
        intrinsics::array_create_instance(&mut vm, module, &[reflected, Value::Int32(2)]).unwrap()
    else {
        panic!("array")
    };
    assert_eq!(vm.heap().array_get(array, 0), Some(zero.clone()));
    assert_eq!(vm.heap().array_get(array, 1), Some(zero.clone()));
    assert_eq!(vm.heap().array_element_type(array), Some(handle as u64));
}

fn check_named_tiers(kind: &str, field_type: u8, zero: Value) {
    for lazy in [false, true] {
        let mut program = named_fixture(kind, field_type, lazy);
        check_named_array_without_extra_record(&program.module, program.entry, &zero);
        program.module.freeze();
        check_named_array_without_extra_record(&program.module, program.entry, &zero);
        #[cfg(feature = "code-in-place")]
        {
            let image = program.module.write_baked(Some(program.entry)).unwrap();
            let (module, entry) = Module::from_baked(Box::leak(image.into_boxed_slice())).unwrap();
            check_named_array_without_extra_record(&module, entry.unwrap(), &zero);
        }
    }
}

#[test]
fn reflected_named_arrays_do_not_need_per_typedef_cast_records() {
    check_named_tiers("Class", 0, Value::Null);
    check_named_tiers(
        "Struct",
        0x08,
        Value::Struct(alloc::vec![Value::Int32(0)].into_boxed_slice()),
    );
}

#[test]
fn reflected_enum_arrays_keep_the_underlying_zero_without_extra_cast_records() {
    check_named_tiers("Enum", 0x08, Value::Int32(0));
    check_named_tiers("Enum", 0x0a, Value::Int64(0));
    check_named_tiers("Enum", 0x0b, Value::Int64(0));
}

const ARRAY_BASE_NAMES: &[(&str, &str)] = &[
    ("System", "Array"),
    ("System", "ICloneable"),
    ("System.Collections", "IList"),
    ("System.Collections", "ICollection"),
    ("System.Collections", "IEnumerable"),
    ("System.Collections", "IStructuralComparable"),
    ("System.Collections", "IStructuralEquatable"),
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum ArrayBaseOrigin { Local, Library, Corlib, MissingCorlib, MissingLibrary }

fn array_base_origin_fixture(
    namespace: &str, name: &str, origin: ArrayBaseOrigin, lazy: bool, generic: bool, opcode: u8,
) -> Program {
    use lamella_pe::{TypeSig, generic_method_signature, method_spec_signature, type_signature};

    let mut core = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let object = core.add_type("System", "Object", Token(0), 1);
    let interface = name != "Array";
    let flags = if interface { 0xa1 } else { 1 };
    if matches!(origin, ArrayBaseOrigin::Local | ArrayBaseOrigin::Corlib) {
        core.add_type(namespace, name, if interface { Token(0) } else { object }, flags);
    }
    if name != "Array" { core.add_type("System", "Array", object, 1); }
    let core = assembly(core, Token(0));
    let mut library = ImageBuilder::new("compat.dll", "compat");
    let object = library.type_ref("System", "Object");
    library.add_type(namespace, name, if interface { Token(0) } else { object }, flags);
    let library = assembly(library, Token(0));

    let mut caller = ImageBuilder::new("caller.dll", "caller");
    let target = if origin == ArrayBaseOrigin::Local {
        let object = caller.type_ref("System", "Object");
        caller.add_type(namespace, name, if interface { Token(0) } else { object }, flags)
    } else {
        if matches!(origin, ArrayBaseOrigin::Library | ArrayBaseOrigin::MissingLibrary) {
            caller.set_type_assembly(&alloc::format!("{namespace}.{name}"), "compat");
        }
        caller.type_ref(namespace, name)
    };
    caller.add_type("", "Program", Token(0), 1);
    let body = |token: Token, opcode: u8| {
        let mut code = alloc::vec![0x1e, 0x02, opcode];
        code.extend_from_slice(&token.0.to_le_bytes());
        code.push(0x2a);
        code
    };
    let entry = if generic {
        let parameter = caller.type_spec(&type_signature(&TypeSig::MVar(0)));
        let check = caller.add_method("Check",
            &generic_method_signature(false, 1, &[TypeSig::Object], &TypeSig::Object),
            &body(parameter, opcode), 0x16, 0, &[]);
        caller.add_generic_param(check, 0, 0, "T");
        let closed = caller.method_spec(check, &method_spec_signature(&[TypeSig::Class(target)]));
        caller.add_method("Main", &[0, 1, 0x1c, 0x1c], &body(closed, 0x28), 0x16, 0, &[])
    } else {
        caller.add_method("Main", &[0, 1, 0x1c, 0x1c], &body(target, opcode), 0x16, 0, &[])
    };
    let caller = assembly(caller, entry);
    let libraries = if origin == ArrayBaseOrigin::Library { alloc::vec![library] } else { Vec::new() };
    if lazy {
        load_program_lazy_corlib_with_libraries_unfrozen(&core, &libraries, &caller).unwrap()
    } else {
        load_with_corlib_and_libraries_unfrozen(&core, &libraries, &caller).unwrap()
    }
}

#[test]
fn array_base_marks_respect_type_ownership() {
    let mut failures = Vec::new();
    for lazy in [false, true] {
        for &(namespace, name) in ARRAY_BASE_NAMES {
            for origin in [ArrayBaseOrigin::Local, ArrayBaseOrigin::Library, ArrayBaseOrigin::Corlib,
                ArrayBaseOrigin::MissingCorlib, ArrayBaseOrigin::MissingLibrary] {
                for generic in [false, true] {
                    if generic && !cfg!(feature = "generics") { continue; }
                    for opcode in [0x75, 0x74] {
                        let mut program = array_base_origin_fixture(namespace, name, origin, lazy, generic, opcode);
                        for storage in 0..=if cfg!(feature = "code-in-place") { 2 } else { 1 } {
                            if storage == 1 { program.module.freeze(); }
                            #[cfg(feature = "code-in-place")]
                            let baked = (storage == 2).then(|| {
                                let bytes = program.module.write_baked(Some(program.entry)).unwrap();
                                Module::from_baked(Box::leak(bytes.into_boxed_slice())).unwrap()
                            });
                            #[cfg(not(feature = "code-in-place"))]
                            let baked: Option<(Module, Option<MethodId>)> = None;
                            let (module, entry) = match &baked {
                                Some((module, entry)) => (module, entry.unwrap()),
                                None => (&program.module, program.entry),
                            };
                            let mut vm = Vm::new();
                            for rectangular in [false, true] {
                                let array = Value::Object(if rectangular {
                                    vm.heap_mut().alloc_md_array(alloc::vec![1, 1])
                                } else {
                                    vm.heap_mut().alloc_array(alloc::vec![Value::Int32(0)])
                                });
                                let result = run(module, &mut vm, entry, alloc::vec![array.clone()]);
                                let accepted = matches!(origin, ArrayBaseOrigin::Corlib | ArrayBaseOrigin::MissingCorlib);
                                let correct = if accepted { result == Ok(Some(array)) } else if opcode == 0x75 {
                                    result == Ok(Some(Value::Null))
                                } else if cfg!(feature = "exceptions") {
                                    matches!(&result, Err(lamella_cil_runtime::Trap::UnhandledException(detail))
                                        if detail.type_name.as_deref() == Some("System.InvalidCastException"))
                                } else {
                                    result == Err(lamella_cil_runtime::Trap::InvalidCast)
                                };
                                if !correct {
                                    failures.push(alloc::format!(
                                        "{namespace}.{name}, origin={origin:?}, lazy={lazy}, generic={generic}, opcode={opcode:x}, storage={storage}, rectangular={rectangular}: {result:?}"
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[cfg(feature = "generics")]
fn generic_array_test_fixture(namespace: &str, name: &str, opcode: u8, lazy: bool) -> Program {
    use lamella_pe::{TypeSig, generic_method_signature, method_spec_signature, type_signature};

    let mut corlib = ImageBuilder::new("mscorlib.dll", "mscorlib");
    corlib.add_type("System", "Object", Token(0), 1);
    let corlib = assembly(corlib, Token(0));
    let mut caller = ImageBuilder::new("caller.dll", "caller");
    let target = caller.type_ref(namespace, name);
    caller.add_type("", "Program", Token(0), 1);
    let parameter = caller.type_spec(&type_signature(&TypeSig::MVar(0)));
    let body = |token: Token, opcode: u8| {
        // Tiny method: ldarg.0; <opcode> <token>; ret.
        let mut code = alloc::vec![0x1e, 0x02, opcode];
        code.extend_from_slice(&token.0.to_le_bytes());
        code.push(0x2a);
        code
    };
    let check = caller.add_method("Check",
        &generic_method_signature(false, 1, &[TypeSig::Object], &TypeSig::Object),
        &body(parameter, opcode), 0x16, 0, &[]);
    caller.add_generic_param(check, 0, 0, "T");
    // The target TypeRef occurs only in this MethodSpec, not in a direct type operand.
    let closed = caller.method_spec(check, &method_spec_signature(&[TypeSig::Class(target)]));
    let entry = caller.add_method("Main", &[0, 1, 0x1c, 0x1c], &body(closed, 0x28), 0x16, 0, &[]);
    let caller = assembly(caller, entry);
    if lazy {
        load_program_lazy_corlib_unfrozen(&corlib, &caller).unwrap()
    } else {
        load_with_corlib_unfrozen(&corlib, &caller).unwrap()
    }
}

#[test]
#[cfg(feature = "generics")]
fn generic_array_base_operands_are_classified_without_definitions() {
    let mut failures = Vec::new();
    for lazy in [false, true] {
        for &(namespace, name) in ARRAY_BASE_NAMES {
            for opcode in [0x75, 0x74] {
                let mut program = generic_array_test_fixture(namespace, name, opcode, lazy);
                for frozen in [false, true] {
                    if frozen { program.module.freeze(); }
                    let mut vm = Vm::new();
                    for rectangular in [false, true] {
                        let array = Value::Object(if rectangular {
                            vm.heap_mut().alloc_md_array(alloc::vec![1, 1])
                        } else {
                            vm.heap_mut().alloc_array(alloc::vec![Value::Int32(0)])
                        });
                        let result = run(&program.module, &mut vm, program.entry, alloc::vec![array.clone()]);
                        if result != Ok(Some(array)) {
                            failures.push(alloc::format!(
                                "{namespace}.{name}, opcode={opcode:x}, lazy={lazy}, frozen={frozen}, rectangular={rectangular}: {result:?}"
                            ));
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
#[cfg(feature = "generics")]
fn generic_unrelated_operands_still_reject_arrays_without_definitions() {
    for lazy in [false, true] {
        for (namespace, name) in [("System", "IComparable"), ("User", "IExtra")] {
            let program = generic_array_test_fixture(namespace, name, 0x75, lazy);
            let mut vm = Vm::new();
            let array = Value::Object(vm.heap_mut().alloc_array(alloc::vec![Value::Int32(0)]));
            assert_eq!(run(&program.module, &mut vm, program.entry, alloc::vec![array]),
                Ok(Some(Value::Null)), "{namespace}.{name}, lazy={lazy}");
        }
    }
}

#[test]
#[cfg(all(feature = "generics", feature = "code-in-place"))]
fn generic_array_base_operands_keep_marks_when_baked_without_definitions() {
    for lazy in [false, true] {
        for &(namespace, name) in ARRAY_BASE_NAMES {
            for opcode in [0x75, 0x74] {
                let mut program = generic_array_test_fixture(namespace, name, opcode, lazy);
                let baked = program.module.write_baked(Some(program.entry)).unwrap();
                let (module, entry) = Module::from_baked(Box::leak(baked.into_boxed_slice())).unwrap();
                let mut vm = Vm::new();
                let array = Value::Object(vm.heap_mut().alloc_array(alloc::vec![Value::Int32(0)]));
                assert_eq!(run(&module, &mut vm, entry.unwrap(), alloc::vec![array.clone()]),
                    Ok(Some(array)), "{namespace}.{name}, opcode={opcode:x}, lazy={lazy}");
            }
        }
    }
}

fn array_store_fixture(namespace: &str, name: &str, lazy: bool) -> Program {
    let mut corlib = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let object = corlib.add_type("System", "Object", Token(0), 1);
    corlib.add_type("System", "IComparable", Token(0), 0xa1);
    corlib.add_type("User", "Other", object, 1);
    let interfaces: Vec<_> = ARRAY_BASE_NAMES[1..].iter().map(|(ns, name)| {
        corlib.add_type(ns, name, Token(0), 0xa1)
    }).collect();
    let array = corlib.add_type("System", "Array", object, 0x81);
    for interface in interfaces {
        corlib.add_interface_impl(array, interface);
    }
    let corlib = assembly(corlib, Token(0));
    let mut caller = ImageBuilder::new("caller.dll", "caller");
    let target = caller.type_ref(namespace, name);
    caller.add_type("", "Program", Token(0), 1);
    let mut code = alloc::vec![0x1a, 0xd0];
    code.extend_from_slice(&target.0.to_le_bytes());
    code.push(0x2a);
    let entry = caller.add_method("Main", &[0, 0, 0x18], &code, 0x16, 0, &[]);
    // static void Store(object[] target, object value) { target[0] = value; }
    caller.add_method("Store", &[0, 2, 1, 0x1d, 0x1c, 0x1c],
        &[0x16, 0x02, 0x16, 0x03, 0xa2, 0x2a], 0x16, 0, &[]);
    let caller = assembly(caller, entry);
    if lazy {
        load_program_lazy_corlib_unfrozen(&corlib, &caller).unwrap()
    } else {
        load_with_corlib_unfrozen(&corlib, &caller).unwrap()
    }
}

fn reflected_target(module: &Module, entry: MethodId, vm: &mut Vm) -> Value {
    let raw = run(module, vm, entry, Vec::new()).unwrap().unwrap();
    let reflected = intrinsics::type_from_handle(vm, module, &[raw.clone()]).unwrap().unwrap();
    assert_ne!(raw, reflected, "the target uses the corlib's defining handle");
    reflected
}

#[test]
fn reflected_array_base_handles_keep_marks_on_both_corlib_tiers() {
    for lazy in [false, true] {
        for &(namespace, name) in ARRAY_BASE_NAMES {
            let program = array_store_fixture(namespace, name, lazy);
            let reflected = reflected_target(&program.module, program.entry, &mut Vm::new());
            let Value::NativeInt(handle) = reflected else { panic!("type handle") };
            assert!(program.module.cast_names_array_base(handle as u64),
                "{namespace}.{name}, lazy={lazy}: a defining handle needs its array-base mark");
        }
    }
}

fn check_reflected_array_store(module: &Module, entry: MethodId, allowed: bool) {
    for rectangular in [false, true] {
        let mut vm = Vm::new();
        let reflected = reflected_target(module, entry, &mut vm);
        let array = intrinsics::array_create_instance(&mut vm, module, &[reflected, Value::Int32(1)])
            .unwrap().unwrap();
        let Value::Object(destination) = array else { panic!("array") };
        let source = if rectangular {
            vm.heap_mut().alloc_md_array(alloc::vec![1, 1])
        } else {
            vm.heap_mut().alloc_array(alloc::vec![Value::Int32(0)])
        };
        let store = module.resolve(1, Token::new(0x06, 2)).unwrap();
        let result = run(module, &mut vm, store, alloc::vec![array, Value::Object(source)]);
        if allowed {
            assert_eq!(result, Ok(None));
            assert_eq!(vm.heap().array_get(destination, 0), Some(Value::Object(source)));
        } else {
            #[cfg(feature = "exceptions")]
            assert!(matches!(&result, Err(lamella_cil_runtime::Trap::UnhandledException(detail))
                if detail.type_name.as_deref() == Some("System.ArrayTypeMismatchException")), "{result:?}");
            #[cfg(not(feature = "exceptions"))]
            assert_eq!(result, Err(lamella_cil_runtime::Trap::ArrayTypeMismatch));
            assert_eq!(vm.heap().array_get(destination, 0), Some(Value::Null));
        }
    }
}

#[test]
fn reflected_interface_array_stores_keep_assignability_on_both_corlib_tiers() {
    for (namespace, name) in [("System", "IComparable"), ("User", "Other")]
        .into_iter().chain(ARRAY_BASE_NAMES.iter().copied()) {
        let allowed = ARRAY_BASE_NAMES.contains(&(namespace, name));
        for lazy in [false, true] {
            let mut program = array_store_fixture(namespace, name, lazy);
            check_reflected_array_store(&program.module, program.entry, allowed);
            program.module.freeze();
            check_reflected_array_store(&program.module, program.entry, allowed);
            #[cfg(feature = "code-in-place")]
            {
                let image = program.module.write_baked(Some(program.entry)).unwrap();
                let (module, entry) = Module::from_baked(Box::leak(image.into_boxed_slice())).unwrap();
                check_reflected_array_store(&module, entry.unwrap(), allowed);
            }
        }
    }
}
