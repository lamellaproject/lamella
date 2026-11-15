use super::*;
use lamella_cil_runtime::{Vm, run};
use lamella_pe::{
    ImageBuilder, TypeSig, generic_method_signature, method_spec_signature, type_signature,
};

const ARRAY_BASES: &[(&str, &str)] = &[
    ("System", "Array"),
    ("System", "ICloneable"),
    ("System.Collections", "IList"),
    ("System.Collections", "ICollection"),
    ("System.Collections", "IEnumerable"),
    ("System.Collections", "IStructuralComparable"),
    ("System.Collections", "IStructuralEquatable"),
];

/// Where the program runs: alone, or beside a core library resolved whole or on demand.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tier {
    NoCorlib,
    Eager,
    Lazy,
}

/// Which assembly the program's reference to the tested name is scoped to.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Owner {
    /// The assembly the program also takes `System.Object` from, which defines the name when it is
    /// loaded.
    Core,
    /// Another library, which is not loaded and which the core library's name does not cover.
    Other,
}

fn assembly(image: ImageBuilder, entry: Token) -> Assembly<'static> {
    Assembly::read(Box::leak(
        image.finish(entry, entry.0 == 0).into_boxed_slice(),
    ))
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn fixture(
    core_name: &str,
    namespace: &str,
    name: &str,
    owner: Owner,
    tier: Tier,
    nested: bool,
    rectangular: bool,
    opcode: u8,
    generic: bool,
) -> Program {
    let interface = name != "Array";
    let flags = if interface { 0xa1 } else { 1 };
    let mut core = ImageBuilder::new(&alloc::format!("{core_name}.dll"), core_name);
    let object = core.add_type("System", "Object", Token(0), 1);
    if owner == Owner::Core {
        core.add_type(
            namespace,
            name,
            if interface { Token(0) } else { object },
            flags,
        );
    }
    if name != "Array" {
        core.add_type("System", "Array", object, 1);
    }
    let core = assembly(core, Token(0));

    // The program is compiled against `core_name` as its core library: its class extends that
    // library's `System.Object`, as every class a compiler writes does. Its first `System.Object`
    // reference is scoped to `mscorlib` whatever the core library is called, as this repository's
    // compiler writes one.
    let mut caller = ImageBuilder::new("caller.dll", "caller");
    caller.object_type();
    caller.set_type_assembly("System.Object", core_name);
    caller.set_type_assembly(
        &alloc::format!("{namespace}.{name}"),
        if owner == Owner::Core {
            core_name
        } else {
            "compat"
        },
    );
    let object = caller.type_ref("System", "Object");
    let named = caller.type_ref(namespace, name);
    caller.add_type("", "Program", object, 1);
    let outer = |element| {
        if rectangular {
            TypeSig::Array {
                element: Box::new(element),
                rank: 2,
            }
        } else {
            TypeSig::SzArray(Box::new(element))
        }
    };
    let shape = if nested {
        outer(TypeSig::Class(named))
    } else {
        TypeSig::Class(named)
    };
    let (target, operation) = if generic {
        let parameter = caller.type_spec(&type_signature(&TypeSig::MVar(0)));
        let mut body = alloc::vec![0x1e, 0x02, opcode];
        body.extend_from_slice(&parameter.0.to_le_bytes());
        body.push(0x2a);
        let check = caller.add_method(
            "Check",
            &generic_method_signature(false, 1, &[TypeSig::Object], &TypeSig::Object),
            &body,
            0x16,
            0,
            &[],
        );
        caller.add_generic_param(check, 0, 0, "T");
        // Only the MethodSpec names the tested type.
        (
            caller.method_spec(check, &method_spec_signature(&[shape])),
            0x28,
        )
    } else if nested {
        // Only the target TypeSpec names the tested type.
        (caller.type_spec(&type_signature(&shape)), opcode)
    } else {
        (named, opcode)
    };
    let element = if nested {
        TypeSig::SzArray(Box::new(TypeSig::Int32))
    } else {
        TypeSig::Int32
    };
    let mut code = if rectangular {
        let source = caller.type_spec(&type_signature(&outer(element)));
        let ctor = caller.member_ref(source, ".ctor", &[0x20, 2, 0x01, 0x08, 0x08]);
        let mut code = alloc::vec![0x16, 0x16, 0x73];
        code.extend_from_slice(&ctor.0.to_le_bytes());
        code
    } else {
        let element = caller.type_spec(&type_signature(&element));
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
    match tier {
        Tier::NoCorlib => load_unfrozen(&caller).unwrap(),
        Tier::Eager => load_with_corlib_and_libraries_unfrozen(&core, &[], &caller).unwrap(),
        Tier::Lazy => {
            load_program_lazy_corlib_with_libraries_unfrozen(&core, &[], &caller).unwrap()
        }
    }
}

/// An array is an instance of `System.Array` and of the interfaces it implements however the
/// program's core library is named, and however that library is resolved: alone, whole or on
/// demand. A same-named type of another assembly is still a different type.
fn check_array_base_scopes(generic: bool) {
    let mut failures = Vec::new();
    let mut runs = 0usize;
    for core_name in ["corlib", "my-core", "mscorlib"] {
        for &(namespace, name) in ARRAY_BASES.iter().chain([&("System", "IComparable")]) {
            for owner in [Owner::Core, Owner::Other] {
                for tier in [Tier::NoCorlib, Tier::Eager, Tier::Lazy] {
                    // With no corlib a generic call stops at its MethodSpec before the test.
                    if generic && tier == Tier::NoCorlib {
                        continue;
                    }
                    for nested in [false, true] {
                        for rectangular in [false, true] {
                            for opcode in [0x75, 0x74] {
                                let mut program = fixture(
                                    core_name,
                                    namespace,
                                    name,
                                    owner,
                                    tier,
                                    nested,
                                    rectangular,
                                    opcode,
                                    generic,
                                );
                                for storage in 0..=if cfg!(feature = "code-in-place") {
                                    2
                                } else {
                                    1
                                } {
                                    if storage == 1 {
                                        program.module.freeze();
                                    }
                                    #[cfg(feature = "code-in-place")]
                                    let baked = (storage == 2).then(|| {
                                        let bytes = program
                                            .module
                                            .write_baked(Some(program.entry))
                                            .unwrap();
                                        Module::from_baked(Box::leak(bytes.into_boxed_slice()))
                                            .unwrap()
                                    });
                                    #[cfg(not(feature = "code-in-place"))]
                                    let baked: Option<(
                                        Module,
                                        Option<MethodId>,
                                    )> = None;
                                    let (module, entry) = match &baked {
                                        Some((module, entry)) => (module, entry.unwrap()),
                                        None => (&program.module, program.entry),
                                    };
                                    let result = run(module, &mut Vm::new(), entry, Vec::new());
                                    runs += 1;
                                    let accepted = name != "IComparable" && owner == Owner::Core;
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
                                            "core={core_name} {namespace}.{name} owner={owner:?} tier={tier:?} nested={nested} rectangular={rectangular} opcode={opcode:x} generic={generic} storage={storage}: {result:?}"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {runs} runs wrong:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn array_bases_are_recognized_by_the_programs_own_core_reference() {
    check_array_base_scopes(false);
}

#[test]
#[cfg(feature = "generics")]
fn generic_array_bases_are_recognized_by_the_programs_own_core_reference() {
    check_array_base_scopes(true);
}
