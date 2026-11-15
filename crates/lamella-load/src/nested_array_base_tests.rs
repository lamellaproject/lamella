use super::*;
use lamella_cil_runtime::{run, Vm};
use lamella_pe::{generic_method_signature, method_spec_signature, type_signature, ImageBuilder, TypeSig};

const ARRAY_BASES: &[(&str, &str)] = &[
    ("System", "Array"),
    ("System", "ICloneable"),
    ("System.Collections", "IList"),
    ("System.Collections", "ICollection"),
    ("System.Collections", "IEnumerable"),
    ("System.Collections", "IStructuralComparable"),
    ("System.Collections", "IStructuralEquatable"),
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Origin { NoCorlib, MissingCorlib, Corlib, Local, Library, MissingLibrary }

fn assembly(image: ImageBuilder, entry: Token) -> Assembly<'static> {
    Assembly::read(Box::leak(image.finish(entry, entry.0 == 0).into_boxed_slice())).unwrap()
}

fn fixture(
    namespace: &str, name: &str, origin: Origin, lazy: bool, rectangular: bool, opcode: u8,
    generic: bool,
) -> Program {
    let interface = name != "Array";
    let flags = if interface { 0xa1 } else { 1 };
    let mut core = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let object = core.add_type("System", "Object", Token(0), 1);
    if matches!(origin, Origin::Corlib | Origin::Local) {
        core.add_type(namespace, name, if interface { Token(0) } else { object }, flags);
    }
    if name != "Array" { core.add_type("System", "Array", object, 1); }
    let core = assembly(core, Token(0));
    let mut library = ImageBuilder::new("compat.dll", "compat");
    let object = library.type_ref("System", "Object");
    library.add_type(namespace, name, if interface { Token(0) } else { object }, flags);
    let library = assembly(library, Token(0));

    let mut caller = ImageBuilder::new("caller.dll", "caller");
    let named = if origin == Origin::Local {
        let object = caller.type_ref("System", "Object");
        caller.add_type(namespace, name, if interface { Token(0) } else { object }, flags)
    } else {
        if matches!(origin, Origin::Library | Origin::MissingLibrary) {
            caller.set_type_assembly(&alloc::format!("{namespace}.{name}"), "compat");
        }
        caller.type_ref(namespace, name)
    };
    caller.add_type("", "Program", Token(0), 1);
    let outer = |element| if rectangular {
        TypeSig::Array { element: Box::new(element), rank: 2 }
    } else {
        TypeSig::SzArray(Box::new(element))
    };
    let shape = outer(TypeSig::Class(named));
    let (target, operation) = if generic {
        let parameter = caller.type_spec(&type_signature(&TypeSig::MVar(0)));
        let mut body = alloc::vec![0x1e, 0x02, opcode];
        body.extend_from_slice(&parameter.0.to_le_bytes());
        body.push(0x2a);
        let check = caller.add_method("Check",
            &generic_method_signature(false, 1, &[TypeSig::Object], &TypeSig::Object),
            &body, 0x16, 0, &[]);
        caller.add_generic_param(check, 0, 0, "T");
        // Only the MethodSpec names this element; no closed type operand preclassifies it.
        (caller.method_spec(check, &method_spec_signature(&[shape])), 0x28)
    } else {
        // Only the target TypeSpec names this type; no direct operand preclassifies it.
        (caller.type_spec(&type_signature(&shape)), opcode)
    };
    let inner = TypeSig::SzArray(Box::new(TypeSig::Int32));
    let mut code = if rectangular {
        let source = caller.type_spec(&type_signature(&outer(inner)));
        let ctor = caller.member_ref(source, ".ctor", &[0x20, 2, 0x01, 0x08, 0x08]);
        let mut code = alloc::vec![0x16, 0x16, 0x73];
        code.extend_from_slice(&ctor.0.to_le_bytes());
        code
    } else {
        let element = caller.type_spec(&type_signature(&inner));
        let mut code = alloc::vec![0x16, 0x8d];
        code.extend_from_slice(&element.0.to_le_bytes());
        code
    };
    code.push(operation);
    code.extend_from_slice(&target.0.to_le_bytes());
    code.push(0x2a);
    code.insert(0, ((code.len() as u8) << 2) | 2);
    let entry = caller.add_method("Main", &[0, 0, 0x1c], &code, 0x16, 0, &[]);
    let caller = assembly(caller, entry);
    let libraries = if origin == Origin::Library { alloc::vec![library] } else { Vec::new() };
    if origin == Origin::NoCorlib {
        load_unfrozen(&caller).unwrap()
    } else if lazy {
        load_program_lazy_corlib_with_libraries_unfrozen(&core, &libraries, &caller).unwrap()
    } else {
        load_with_corlib_and_libraries_unfrozen(&core, &libraries, &caller).unwrap()
    }
}

fn check_nested_array_bases(generic: bool) {
    let mut failures = Vec::new();
    for &(namespace, name) in ARRAY_BASES.iter().chain([&("System", "IComparable")]) {
        for origin in [Origin::NoCorlib, Origin::MissingCorlib, Origin::Corlib,
            Origin::Local, Origin::Library, Origin::MissingLibrary] {
            if generic && origin == Origin::NoCorlib { continue; }
            for lazy in [false, true] {
                if lazy && origin == Origin::NoCorlib { continue; }
                for rectangular in [false, true] {
                    for opcode in [0x75, 0x74] {
                        let mut program = fixture(namespace, name, origin, lazy, rectangular, opcode, generic);
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
                            let result = run(module, &mut Vm::new(), entry, Vec::new());
                            let accepted = name != "IComparable" && matches!(origin,
                                Origin::NoCorlib | Origin::MissingCorlib | Origin::Corlib);
                            let correct = if accepted {
                                matches!(result, Ok(Some(Value::Object(_))))
                            } else if opcode == 0x75 {
                                result == Ok(Some(Value::Null))
                            } else if cfg!(feature = "exceptions") {
                                matches!(&result, Err(lamella_cil_runtime::Trap::UnhandledException(detail))
                                    if detail.type_name.as_deref() == Some("System.InvalidCastException"))
                            } else {
                                result == Err(lamella_cil_runtime::Trap::InvalidCast)
                            };
                            if !correct {
                                failures.push(alloc::format!(
                                    "{namespace}.{name}, origin={origin:?}, lazy={lazy}, rectangular={rectangular}, opcode={opcode:x}, generic={generic}, storage={storage}: {result:?}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn nested_array_bases_are_classified_by_the_loader() {
    check_nested_array_bases(false);
}

#[test]
#[cfg(feature = "generics")]
fn generic_nested_array_bases_are_classified_by_the_loader() {
    check_nested_array_bases(true);
}
