use super::*;
use lamella_cil_runtime::{run, Vm};
use lamella_pe::ImageBuilder;

#[test]
fn jagged_array_does_not_satisfy_a_rectangular_typespec() {
    let mut image = ImageBuilder::new("array-shapes.dll", "array-shapes");
    image.add_type("", "Host", Token(0), 1);
    let vector_element = image.type_spec(&[0x1d, 0x08]);
    let jagged = image.type_spec(&[0x1d, 0x1d, 0x08]);
    let rectangular = image.type_spec(&[0x14, 0x08, 2, 0, 0]);
    let mut test_method = |name: &str, target: Token| {
        let mut body = alloc::vec![0x32, 0x18, 0x8d];
        body.extend_from_slice(&vector_element.0.to_le_bytes());
        body.push(0x75);
        body.extend_from_slice(&target.0.to_le_bytes());
        body.push(0x2a);
        image.add_method(name, &[0, 0, 0x1c], &body, 0x16, 0, &[])
    };
    let same_shape = test_method("AsJagged", jagged);
    let entry = test_method("AsRectangular", rectangular);
    let bytes = Box::leak(image.finish(entry, false).into_boxed_slice());
    let assembly = Assembly::read(bytes).expect("generated array metadata");
    let program = load(&assembly).expect("generated array program");
    let mut vm = Vm::new();
    let control = program.module.resolve(0, same_shape).unwrap();
    assert!(matches!(run(&program.module, &mut vm, control, Vec::new()),
        Ok(Some(Value::Object(_)))), "the jagged identity cast must succeed");
    assert_eq!(run(&program.module, &mut vm, program.entry, Vec::new()),
        Ok(Some(Value::Null)), "jagged and rectangular arrays are distinct types");
}

#[test]
fn array_casts_preserve_rank_and_element_type() {
    let mut image = ImageBuilder::new("array-casts.dll", "array-casts");
    let host = image.add_type("", "Host", Token(0), 1);
    let signatures: &[&[u8]] = &[
        &[0x1d, 0x08],                         // int[]
        &[0x1d, 0x1d, 0x08],                   // int[][]
        &[0x14, 0x08, 2, 0, 0],                // int[,]
        &[0x14, 0x08, 3, 0, 0],                // int[,,]
        &[0x14, 0x0e, 2, 0, 0],                // string[,]
        &[0x14, 0x1c, 2, 0, 0],                // object[,]
        &[0x14, 0x0a, 2, 0, 0],                // long[,]
        &[0x1d, 0x14, 0x08, 2, 0, 0],          // Vector of int[,].
        &[0x14, 0x1d, 0x08, 2, 0, 0],          // Rectangular array of int[].
        &[0x14, 0x08, 1, 0, 0],                // int[*]
        &[0x1d, 0x1c],                         // object[]
        &[0x1c],                               // object
    ];
    let mut shapes: Vec<Token> = signatures.iter().map(|sig| image.type_spec(sig)).collect();
    shapes.push(host);
    let ctor_signature = |rank: usize| {
        let mut sig = alloc::vec![0x20, rank as u8, 0x01];
        sig.extend(core::iter::repeat_n(0x08, rank));
        sig
    };
    let mut allocations = Vec::new();
    for (index, rank) in [(2, 2), (3, 3), (4, 2), (5, 2), (6, 2), (8, 2), (9, 1)] {
        let ctor = image.member_ref(shapes[index], ".ctor", &ctor_signature(rank));
        // Empty arrays ensure casts use the declared element type, not element values.
        let mut il = alloc::vec![0x16; rank];
        il.push(0x73);
        il.extend_from_slice(&ctor.0.to_le_bytes());
        allocations.push((index, il));
    }
    for (index, element) in [(1, shapes[0]), (7, shapes[2])] {
        let mut il = alloc::vec![0x16, 0x8d];
        il.extend_from_slice(&element.0.to_le_bytes());
        allocations.push((index, il));
    }
    let cases = [
        (2, 2, true), (2, 3, false), (2, 0, false), (2, 1, false),
        (2, 5, false), (2, 6, false), (4, 5, true), (5, 4, false),
        (1, 1, true), (1, 2, false), (1, 10, true),
        (7, 7, true), (7, 8, false), (7, 10, true),
        (8, 8, true), (8, 7, false), (8, 5, true), (8, 2, false),
        (9, 9, true), (9, 0, false), (9, 2, false),
        (2, 11, true), (2, 12, false),
    ];
    let mut methods = Vec::new();
    for &(source, target, expected) in &cases {
        let mut il = allocations.iter().find(|(index, _)| *index == source).unwrap().1.clone();
        il.push(0x75);
        il.extend_from_slice(&shapes[target].0.to_le_bytes());
        il.push(0x2a);
        let mut body = alloc::vec![((il.len() as u8) << 2) | 2];
        body.extend(il);
        let name = alloc::format!("Cast{source}To{target}");
        let token = image.add_method(&name, &[0, 0, 0x1c], &body, 0x16, 0, &[]);
        methods.push((token, expected, name));
    }
    let bytes = Box::leak(image.finish(methods[0].0, false).into_boxed_slice());
    let assembly = Assembly::read(bytes).expect("generated array metadata");
    let mut program = load_unfrozen(&assembly).expect("generated array program");
    let check = |module: &Module, phase: &str| {
        for (token, expected, name) in &methods {
            let mut vm = Vm::new();
            let method = module.resolve(0, *token).unwrap();
            let value = run(module, &mut vm, method, Vec::new()).expect(name);
            assert_eq!(matches!(value, Some(Value::Object(_))), *expected, "{phase}: {name}");
            assert!(matches!(value, Some(Value::Object(_) | Value::Null)), "{phase}: {name}");
        }
    };
    check(&program.module, "live");
    program.module.freeze();
    check(&program.module, "frozen");
    #[cfg(feature = "code-in-place")]
    {
        let baked = program.module.write_baked(Some(program.entry)).expect("bake array program");
        let (module, _) = Module::from_baked(Box::leak(baked.into_boxed_slice())).expect("read array program");
        check(&module, "baked");
    }
}

#[cfg(feature = "code-in-place")]
#[test]
fn rectangular_array_element_types_survive_trimming() {
    let mut image = ImageBuilder::new("array-trim.dll", "array-trim");
    let source_element = image.add_type("", "Source", Token(0), 1);
    let target_element = image.add_type("", "Target", Token(0), 1);
    image.add_type("", "Host", Token(0), 1);
    let shape = |element: Token| [0x14, 0x12, (element.row() << 2) as u8, 2, 0, 0];
    let source = image.type_spec(&shape(source_element));
    let target = image.type_spec(&shape(target_element));
    let ctor = image.member_ref(source, ".ctor", &[0x20, 2, 0x01, 0x08, 0x08]);
    let mut body = alloc::vec![0x36, 0x16, 0x16, 0x73];
    body.extend_from_slice(&ctor.0.to_le_bytes());
    body.push(0x75);
    body.extend_from_slice(&target.0.to_le_bytes());
    body.push(0x2a);
    let entry = image.add_method("Cast", &[0, 0, 0x1c], &body, 0x16, 0, &[]);
    let bytes = Box::leak(image.finish(entry, false).into_boxed_slice());
    let assembly = Assembly::read(bytes).expect("generated array metadata");
    let mut program = load_unfrozen(&assembly).expect("generated array program");
    let check = |module: &Module, phase: &str| {
        assert_eq!(run(module, &mut Vm::new(), program.entry, Vec::new()), Ok(Some(Value::Null)), "{phase}");
    };
    check(&program.module, "live");
    let (methods, types, strings) = program.module.reachable_set(Some(program.entry));
    program.module.retain_reachable(&methods, &types, &strings);
    check(&program.module, "trimmed");
    let bytes = program.module.write_baked(Some(program.entry)).expect("bake trimmed program");
    let (module, _) = Module::from_baked(Box::leak(bytes.into_boxed_slice())).expect("read trimmed program");
    check(&module, "baked");
}
