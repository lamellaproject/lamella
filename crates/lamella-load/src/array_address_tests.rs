use super::*;
use lamella_cil_runtime::{Trap, Vm, run};
use lamella_pe::ImageBuilder;

fn assembly(image: ImageBuilder, entry: Token) -> Assembly<'static> {
    Assembly::read(Box::leak(image.finish(entry, entry.0 == 0).into_boxed_slice())).unwrap()
}

fn tiny(code: &[u8]) -> Vec<u8> {
    assert!(code.len() < 64);
    let mut body = alloc::vec![((code.len() as u8) << 2) | 2];
    body.extend_from_slice(code);
    body
}

fn address_program(element: &str, requested: &str, in_corlib: bool, lazy: bool) -> Program {
    let mut corlib = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let object = corlib.add_type("System", "Object", Token(0), 1);
    let value_type = corlib.add_type("System", "ValueType", object, 0x81);
    let mut element_token = object;
    for name in ["Byte", "SByte", "Int64", "String"] {
        let base = if name == "String" { object } else { value_type };
        let token = corlib.add_type("System", name, base, 1);
        if name == element { element_token = token; }
    }
    let requested_in_corlib = corlib.type_ref("System", requested);
    corlib.add_type("", "Factory", object, 1);
    let mut make = alloc::vec![0x17, 0x8d];
    make.extend_from_slice(&element_token.0.to_le_bytes());
    make.push(0x2a);
    corlib.add_method("Make", &[0, 0, 0x1c], &tiny(&make), 0x16, 0, &[]);
    let address = |token: Token| {
        let mut code = alloc::vec![0x16, 0x8f];
        code.extend_from_slice(&token.0.to_le_bytes());
        code.extend_from_slice(&[0x26, 0x1f, 42, 0x2a]);
        code
    };
    let mut check = alloc::vec![0x02];
    check.extend(address(requested_in_corlib));
    corlib.add_method("Check", &[0, 1, 0x08, 0x1c], &tiny(&check), 0x16, 0, &[]);
    let corlib = assembly(corlib, Token(0));

    let mut caller = ImageBuilder::new("array-address.dll", "array-address");
    let factory = caller.type_ref("", "Factory");
    let requested_in_caller = caller.type_ref("System", requested);
    let make = caller.member_ref(factory, "Make", &[0, 0, 0x1c]);
    let check = caller.member_ref(factory, "Check", &[0, 1, 0x08, 0x1c]);
    caller.add_type("", "Program", Token(0), 1);
    let mut code = alloc::vec![0x28];
    code.extend_from_slice(&make.0.to_le_bytes());
    if in_corlib {
        code.push(0x28);
        code.extend_from_slice(&check.0.to_le_bytes());
        code.push(0x2a);
    } else {
        code.extend(address(requested_in_caller));
    }
    // The requested type token occurs only at ldelema, never at newarr, a cast or ldtoken.
    let entry = caller.add_method("Main", &[0, 0, 0x08], &tiny(&code), 0x16, 0, &[]);
    let caller = assembly(caller, entry);
    if lazy {
        load_program_lazy_corlib_unfrozen(&corlib, &caller).unwrap()
    } else {
        load_with_corlib_unfrozen(&corlib, &caller).unwrap()
    }
}

fn check_address_tiers(element: &str, requested: &str, allowed: bool) {
    for lazy in [false, true] {
        for in_corlib in [false, true] {
            let mut program = address_program(element, requested, in_corlib, lazy);
            let check = |module: &Module, entry: MethodId, phase: &str| {
                let result = run(module, &mut Vm::new(), entry, Vec::new());
                let context = alloc::format!("{element}[] -> &{requested}, lazy={lazy}, in_corlib={in_corlib}, {phase}");
                if allowed {
                    assert_eq!(result, Ok(Some(Value::Int32(42))), "{context}");
                } else {
                    match result {
                        Err(Trap::ArrayTypeMismatch) => {}
                        Err(Trap::UnhandledException(exception)) => {
                            assert_eq!(exception.type_name.as_deref(), Some("System.ArrayTypeMismatchException"), "{context}");
                        }
                        other => panic!("{context}: expected an array type mismatch, got {other:?}"),
                    }
                }
            };
            check(&program.module, program.entry, "live");
            program.module.freeze();
            check(&program.module, program.entry, "frozen");
            #[cfg(feature = "code-in-place")]
            {
                let bytes = program.module.write_baked(Some(program.entry)).unwrap();
                let (module, entry) = Module::from_baked(Box::leak(bytes.into_boxed_slice())).unwrap();
                check(&module, entry.unwrap(), "baked");
            }
        }
    }
}

#[test]
fn array_addresses_resolve_standalone_operand_types_on_both_corlib_tiers() {
    check_address_tiers("Byte", "Byte", true);
    check_address_tiers("Byte", "SByte", true);
    check_address_tiers("String", "String", true);
}

#[test]
fn array_addresses_still_reject_incompatible_storage_on_both_corlib_tiers() {
    check_address_tiers("Byte", "Int64", false);
    check_address_tiers("String", "Object", false);
    check_address_tiers("Object", "String", false);
}
