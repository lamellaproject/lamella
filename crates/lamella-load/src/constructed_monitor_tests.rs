use super::*;
use lamella_cil_runtime::{Trap, Vm, intrinsics, run};
use lamella_pe::ImageBuilder;

fn assembly(image: ImageBuilder, entry: Token) -> Assembly<'static> {
    Assembly::read(Box::leak(
        image.finish(entry, entry.0 == 0).into_boxed_slice(),
    ))
    .unwrap()
}

fn type_handles(image: &mut ImageBuilder, named: Token, other: Token) -> Vec<Token> {
    let coded = |token: Token| ((token.row() << 2) | u32::from(token.table() == 1)) as u8;
    let signatures = [
        alloc::vec![0x1d, 0x08],
        alloc::vec![0x1d, 0x09],
        alloc::vec![0x1d, 0x02],
        alloc::vec![0x1d, 0x05],
        alloc::vec![0x14, 0x08, 1, 0, 0],
        alloc::vec![0x14, 0x08, 2, 0, 0],
        alloc::vec![0x14, 0x08, 3, 0, 0],
        alloc::vec![0x1d, 0x1d, 0x08],
        alloc::vec![0x0f, 0x08],
        alloc::vec![0x0f, 0x09],
        alloc::vec![0x0f, 0x0f, 0x08],
        alloc::vec![0x0f, 0x01],
        alloc::vec![0x1d, 0x11, coded(named)],
        alloc::vec![0x1d, 0x11, coded(other)],
        alloc::vec![0x0f, 0x11, coded(named)],
        alloc::vec![0x0f, 0x11, coded(other)],
    ];
    signatures
        .iter()
        .enumerate()
        .map(|(index, signature)| {
            let spec = image.type_spec(signature);
            let mut code = alloc::vec![0x1a, 0xd0];
            code.extend_from_slice(&spec.0.to_le_bytes());
            code.push(0x2a);
            image.add_method(
                &alloc::format!("Handle{index}"),
                &[0, 0, 0x18],
                &code,
                0x16,
                0,
                &[],
            )
        })
        .collect()
}

fn check_handles(module: &Module, methods: &[(u8, Vec<Token>)]) {
    let handles: Vec<Vec<Value>> = methods
        .iter()
        .map(|(asm, methods)| {
            methods
                .iter()
                .map(|token| {
                    run(
                        module,
                        &mut Vm::new(),
                        module.resolve(*asm, *token).unwrap(),
                        Vec::new(),
                    )
                    .unwrap()
                    .unwrap()
                })
                .collect()
        })
        .collect();
    for (index, first) in handles[0].iter().enumerate() {
        let mut vm = Vm::new();
        assert_eq!(
            intrinsics::monitor_enter(&mut vm, module, &[first.clone()]),
            Ok(None),
            "enter shape {index}"
        );
        for assembly_handles in &handles {
            let alias = &assembly_handles[index];
            assert_eq!(
                intrinsics::object_reference_equals(
                    &mut vm,
                    module,
                    &[first.clone(), alias.clone()]
                ),
                Ok(Some(Value::Int32(1))),
                "reference alias {index}"
            );
            assert_eq!(
                intrinsics::reflect_handle_equals(&mut vm, module, &[first.clone(), alias.clone()]),
                Ok(Some(Value::Int32(1))),
                "type alias {index}"
            );
            assert_eq!(
                intrinsics::monitor_pulse(&mut vm, module, &[alias.clone()]),
                Ok(None),
                "pulse alias {index}"
            );
            for (other_index, other) in assembly_handles.iter().enumerate() {
                if other_index == index {
                    continue;
                }
                assert_eq!(
                    intrinsics::object_reference_equals(
                        &mut vm,
                        module,
                        &[first.clone(), other.clone()]
                    ),
                    Ok(Some(Value::Int32(0))),
                    "distinct types {index}, {other_index}"
                );
                assert_eq!(
                    intrinsics::monitor_pulse(&mut vm, module, &[other.clone()]),
                    Err(Trap::SynchronizationLock),
                    "distinct monitors {index}, {other_index}"
                );
            }
        }
        assert_eq!(
            intrinsics::monitor_exit(&mut vm, module, &[handles.last().unwrap()[index].clone()]),
            Ok(None)
        );
    }
}

fn fixture() -> (Program, [(u8, Vec<Token>); 2]) {
    let mut library = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let value_type = library.add_type("System", "ValueType", Token(0), 0x81);
    let shared = library.add_type("", "Shared", value_type, 0x101);
    let other = library.add_type("", "Other", value_type, 0x101);
    library.add_type("", "LibraryHost", Token(0), 1);
    let library_methods = type_handles(&mut library, shared, other);
    let library = assembly(library, Token(0));
    let mut caller = ImageBuilder::new("caller.dll", "caller");
    let shared = caller.type_ref("", "Shared");
    let other = caller.type_ref("", "Other");
    caller.add_type("", "Host", Token(0), 1);
    caller.type_spec(&[0x1d, 0x0a]);
    let caller_methods = type_handles(&mut caller, shared, other);
    let caller = assembly(caller, caller_methods[0]);
    let program = load_with_corlib_unfrozen(&library, &caller).unwrap();
    (program, [(0, library_methods), (1, caller_methods)])
}

#[test]
fn constructed_monitor_identity_survives_loading_and_freezing() {
    let (mut program, methods) = fixture();
    check_handles(&program.module, &methods);
    program.module.freeze();
    check_handles(&program.module, &methods);
    #[cfg(feature = "code-in-place")]
    {
        let bytes = program.module.write_baked(Some(program.entry)).unwrap();
        let (module, _) = Module::from_baked(Box::leak(bytes.into_boxed_slice())).unwrap();
        check_handles(&module, &methods);
    }
}

#[cfg(feature = "code-in-place")]
#[test]
fn constructed_monitor_identity_survives_trimming() {
    for index in [12, 14] {
        let (mut program, methods) = fixture();
        let entry = program.module.resolve(1, methods[1].1[index]).unwrap();
        let (keep_methods, types, strings) = program.module.reachable_set(Some(entry));
        program
            .module
            .retain_reachable(&keep_methods, &types, &strings);
        let check = |module: &Module| {
            let mut vm = Vm::new();
            let method = module.resolve(1, methods[1].1[index]).unwrap();
            let handle = run(module, &mut vm, method, Vec::new()).unwrap().unwrap();
            assert_eq!(
                intrinsics::monitor_enter(&mut vm, module, &[handle.clone()]),
                Ok(None),
                "trimmed shape {index}"
            );
            assert_eq!(
                intrinsics::monitor_exit(&mut vm, module, &[handle]),
                Ok(None)
            );
        };
        check(&program.module);
        program.module.freeze();
        check(&program.module);
        #[cfg(feature = "code-in-place")]
        {
            let bytes = program.module.write_baked(None).unwrap();
            let (module, _) = Module::from_baked(Box::leak(bytes.into_boxed_slice())).unwrap();
            check(&module);
        }
    }
}
