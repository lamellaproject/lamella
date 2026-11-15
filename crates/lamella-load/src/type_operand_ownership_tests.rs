use super::*;
#[cfg(feature = "reflection")]
use lamella_cil_runtime::{Vm, intrinsics, run};
use lamella_pe::{ImageBuilder, TypeSig, type_signature};

fn assembly(image: ImageBuilder, entry: Token) -> Assembly<'static> {
    Assembly::read(Box::leak(
        image.finish(entry, entry.0 == 0).into_boxed_slice(),
    ))
    .unwrap()
}

fn load_fixture(
    core: &Assembly<'static>,
    libraries: &[Assembly<'static>],
    caller: &Assembly<'static>,
    lazy: bool,
) -> Result<Program, LazyLoadError> {
    if lazy {
        load_program_lazy_corlib_with_libraries_unfrozen(core, libraries, caller)
    } else {
        load_with_corlib_and_libraries_unfrozen(core, libraries, caller).map_err(Into::into)
    }
}

fn assert_collision(result: Result<Program, LazyLoadError>, name: &str, context: &str) {
    let Err(LazyLoadError::Load(LoadError::SameNamedTypes(message))) = result else {
        panic!("{context}: the ambiguous type must be refused at load")
    };
    for part in [name, "mscorlib", "compat"] {
        assert!(message.contains(part), "{context}: {message}");
    }
}

#[test]
fn named_type_operands_refuse_core_library_collisions() {
    for (namespace, name) in [
        ("System", "Array"),
        ("System", "ICloneable"),
        ("System.Collections", "IList"),
        ("System.Collections", "ICollection"),
        ("System.Collections", "IEnumerable"),
        ("System.Collections", "IStructuralComparable"),
        ("System.Collections", "IStructuralEquatable"),
    ] {
        let full_name = alloc::format!("{namespace}.{name}");
        for scope in [
            "mscorlib",
            "System.Runtime",
            "System.Private.CoreLib",
            "netstandard",
            "compat",
        ] {
            for shape in 0..3 {
                let interface = name != "Array";
                let mut core = ImageBuilder::new("mscorlib.dll", "mscorlib");
                let object = core.add_type("System", "Object", Token(0), 1);
                core.add_type(
                    namespace,
                    name,
                    if interface { Token(0) } else { object },
                    if interface { 0xa1 } else { 1 },
                );
                let core = assembly(core, Token(0));
                let mut library = ImageBuilder::new("compat.dll", "compat");
                let object = library.type_ref("System", "Object");
                library.add_type(
                    namespace,
                    name,
                    if interface { Token(0) } else { object },
                    if interface { 0xa1 } else { 1 },
                );
                let library = assembly(library, Token(0));
                let other = assembly(ImageBuilder::new("other.dll", "other"), Token(0));
                let mut libraries = alloc::vec![library, other];
                let mut caller = ImageBuilder::new("caller.dll", "caller");
                caller.set_type_assembly(&full_name, scope);
                let named = caller.type_ref(namespace, name);
                let target = if shape == 1 {
                    caller.type_spec(&type_signature(&TypeSig::SzArray(Box::new(
                        TypeSig::Class(named),
                    ))))
                } else {
                    named
                };
                caller.add_type("", "Program", Token(0), 1);
                let mut code = if shape == 2 {
                    alloc::vec![0xd0]
                } else {
                    alloc::vec![0x02, 0x75]
                };
                code.extend_from_slice(&target.0.to_le_bytes());
                code.push(0x2a);
                code.insert(0, ((code.len() as u8) << 2) | 2);
                let signature: &[u8] = if shape == 2 {
                    &[0, 0, 0x18]
                } else {
                    &[0, 1, 0x1c, 0x1c]
                };
                let entry = caller.add_method("Main", signature, &code, 0x16, 0, &[]);
                let caller = assembly(caller, entry);
                for reversed in [false, true] {
                    if reversed {
                        libraries.reverse();
                    }
                    for lazy in [false, true] {
                        assert_collision(
                            load_fixture(&core, &libraries, &caller, lazy),
                            &full_name,
                            &alloc::format!(
                                "scope={scope}, shape={shape}, reversed={reversed}, lazy={lazy}"
                            ),
                        );
                    }
                }
            }
        }
    }
}

#[cfg(feature = "reflection")]
fn ldtoken_fixture(collision: bool, lazy: bool) -> Result<(Program, u64), LazyLoadError> {
    let mut core = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let object = core.add_type("System", "Object", Token(0), 1);
    let definition = core.add_type("System", "ICloneable", Token(0), 0xa1);
    let reference = core.type_ref("System", "ICloneable");
    core.add_type("", "Factory", object, 1);
    let mut code = alloc::vec![0x1a, 0xd0];
    code.extend_from_slice(&reference.0.to_le_bytes());
    code.push(0x2a);
    // This reachable corlib body names the TypeRef only through ldtoken.
    core.add_method("Handle", &[0, 0, 0x18], &code, 0x16, 0, &[]);
    let core = assembly(core, Token(0));
    let mut library = ImageBuilder::new("compat.dll", "compat");
    if collision {
        library.add_type("System", "ICloneable", Token(0), 0xa1);
    }
    let libraries = [assembly(library, Token(0))];
    let mut caller = ImageBuilder::new("caller.dll", "caller");
    let factory = caller.type_ref("", "Factory");
    let handle = caller.member_ref(factory, "Handle", &[0, 0, 0x18]);
    caller.add_type("", "Program", Token(0), 1);
    let mut code = alloc::vec![0x1a, 0x28];
    code.extend_from_slice(&handle.0.to_le_bytes());
    code.push(0x2a);
    let entry = caller.add_method("Main", &[0, 0, 0x18], &code, 0x16, 0, &[]);
    let caller = assembly(caller, entry);
    load_fixture(&core, &libraries, &caller, lazy)
        .map(|program| (program, asm_key(0, definition.0)))
}

#[cfg(feature = "reflection")]
fn check_ldtoken(module: &Module, entry: MethodId, definition: u64) {
    let mut vm = Vm::new();
    let raw = run(module, &mut vm, entry, Vec::new()).unwrap().unwrap();
    let reflected = intrinsics::type_from_handle(&mut vm, module, &[raw]).unwrap();
    assert_eq!(reflected, Some(Value::NativeInt(definition as i64)));
    assert!(module.cast_names_array_base(definition));
}

#[test]
#[cfg(feature = "reflection")]
fn corlib_ldtoken_references_keep_identity_or_refuse_a_collision() {
    for lazy in [false, true] {
        assert_collision(
            ldtoken_fixture(true, lazy).map(|(program, _)| program),
            "System.ICloneable",
            &alloc::format!("corlib ldtoken, lazy={lazy}"),
        );
        let (mut program, definition) = ldtoken_fixture(false, lazy).unwrap();
        check_ldtoken(&program.module, program.entry, definition);
        program.module.freeze();
        check_ldtoken(&program.module, program.entry, definition);
        #[cfg(feature = "code-in-place")]
        {
            let bytes = program.module.write_baked(Some(program.entry)).unwrap();
            let (module, entry) = Module::from_baked(Box::leak(bytes.into_boxed_slice())).unwrap();
            check_ldtoken(&module, entry.unwrap(), definition);
        }
    }
}

#[cfg(feature = "reflection")]
fn extra_interface_fixture(implemented: bool, opcode: u8, lazy: bool) -> Program {
    let mut core = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let object = core.add_type("System", "Object", Token(0), 1);
    let extra = core.add_type("User", "IExtra", Token(0), 0xa1);
    core.add_type("User", "IUnrelated", Token(0), 0xa1);
    let array = core.add_type("System", "Array", object, 0x81);
    core.add_interface_impl(array, extra);
    let core = assembly(core, Token(0));
    let mut caller = ImageBuilder::new("caller.dll", "caller");
    let target = caller.type_ref("User", if implemented { "IExtra" } else { "IUnrelated" });
    caller.add_type("", "Program", Token(0), 1);
    // No operand names System.Array; an incoming array still inherits its interfaces.
    let mut code = alloc::vec![0x1e, 0x02, opcode];
    code.extend_from_slice(&target.0.to_le_bytes());
    code.push(0x2a);
    let entry = caller.add_method("Main", &[0, 1, 0x1c, 0x1c], &code, 0x16, 0, &[]);
    let caller = assembly(caller, entry);
    load_fixture(&core, &[], &caller, lazy).unwrap()
}

#[cfg(feature = "reflection")]
fn check_extra_interface(module: &Module, entry: MethodId, implemented: bool, opcode: u8) {
    let array_handle = module.type_handle_by_name("System.Array").unwrap();
    assert!(module.cast_names_array_base(array_handle));
    assert!(module.type_id_by_handle(array_handle).is_some());
    for rectangular in [false, true] {
        let mut vm = Vm::new();
        let value = Value::Object(if rectangular {
            vm.heap_mut().alloc_md_array(alloc::vec![1, 1])
        } else {
            vm.heap_mut().alloc_array(alloc::vec![Value::Int32(0)])
        });
        let result = run(module, &mut vm, entry, alloc::vec![value.clone()]);
        if implemented {
            assert_eq!(result, Ok(Some(value)));
        } else if opcode == 0x75 {
            assert_eq!(result, Ok(Some(Value::Null)));
        } else if cfg!(feature = "exceptions") {
            assert!(
                matches!(&result, Err(lamella_cil_runtime::Trap::UnhandledException(detail))
                if detail.type_name.as_deref() == Some("System.InvalidCastException")),
                "{result:?}"
            );
        } else {
            assert_eq!(result, Err(lamella_cil_runtime::Trap::InvalidCast));
        }
    }
}

#[test]
#[cfg(feature = "reflection")]
fn array_materialization_preserves_additional_corlib_interfaces() {
    for lazy in [false, true] {
        for implemented in [false, true] {
            for opcode in [0x75, 0x74] {
                let mut program = extra_interface_fixture(implemented, opcode, lazy);
                check_extra_interface(&program.module, program.entry, implemented, opcode);
                program.module.freeze();
                check_extra_interface(&program.module, program.entry, implemented, opcode);
                #[cfg(feature = "code-in-place")]
                {
                    let bytes = program.module.write_baked(Some(program.entry)).unwrap();
                    let (module, entry) =
                        Module::from_baked(Box::leak(bytes.into_boxed_slice())).unwrap();
                    check_extra_interface(&module, entry.unwrap(), implemented, opcode);
                }
            }
        }
    }
}
