use super::*;
use lamella_cil_runtime::{run, Vm};
use lamella_pe::ImageBuilder;

#[test]
fn stack_struct_fields_execute_with_loaded_layout() {
    let mut image = ImageBuilder::new("stack-struct.dll", "stack-struct");
    let value_type = image.type_ref("System", "ValueType");
    image.add_type("", "Record", value_type, 0x109);
    let payload = image.add_field("Payload", &[0x06, 0x08], 0x06);
    image.add_type("", "Host", Token(0), 1);
    // Allocate two records, then write and read the second record's field.
    let mut il = alloc::vec![0x1e, 0xe0, 0xfe, 0x0f, 0x1a, 0x58, 0x25, 0x1f, 47, 0x7d];
    il.extend_from_slice(&payload.0.to_le_bytes());
    il.push(0x7b);
    il.extend_from_slice(&payload.0.to_le_bytes());
    il.push(0x2a);
    let mut body = alloc::vec![((il.len() as u8) << 2) | 2];
    body.extend(il);
    let entry = image.add_method("Main", &[0, 0, 0x08], &body, 0x16, 0, &[]);
    let assembly = Assembly::read(Box::leak(image.finish(entry, false).into_boxed_slice()))
        .expect("generated stack struct metadata");
    let mut program = load_unfrozen(&assembly).expect("generated stack struct program");
    let check = |module: &Module, phase: &str| {
        let entry = module.resolve(0, entry).unwrap();
        assert_eq!(run(module, &mut Vm::new(), entry, Vec::new()),
            Ok(Some(Value::Int32(47))), "{phase}");
    };
    check(&program.module, "live");
    #[cfg(feature = "code-in-place")]
    {
        let mut trimmed = program.module.clone();
        let (methods, types, strings) = trimmed.reachable_set(Some(program.entry));
        trimmed.retain_reachable(&methods, &types, &strings);
        check(&trimmed, "trimmed");
        let image = trimmed.write_baked(Some(program.entry)).expect("bake trimmed stack struct");
        let (baked, _) = Module::from_baked(Box::leak(image.into_boxed_slice())).unwrap();
        check(&baked, "trimmed baked");
    }
    program.module.freeze();
    check(&program.module, "frozen");
    #[cfg(feature = "code-in-place")]
    {
        let baked = program.module.write_baked(Some(program.entry)).expect("bake stack struct program");
        let (module, _) = Module::from_baked(Box::leak(baked.into_boxed_slice())).expect("read stack struct program");
        check(&module, "baked");
    }
}

#[test]
fn stack_struct_nested_and_enum_fields_use_declared_storage() {
    for is_enum in [false, true] {
        let mut image = ImageBuilder::new("nested-stack-struct.dll", "nested-stack-struct");
        let value_type = image.type_ref("System", "ValueType");
        let enum_type = image.type_ref("System", "Enum");
        let nested = image.add_type("", "Nested", if is_enum { enum_type } else { value_type }, 0x109);
        let left = image.add_field(if is_enum { "value__" } else { "Left" }, &[0x06, 0x05], if is_enum { 0x606 } else { 6 });
        if !is_enum {
            image.add_field("Right", &[0x06, 0x06], 6);
        }
        image.add_type("", "Outer", value_type, 0x109);
        image.add_field("Tag", &[0x06, 0x05], 6);
        let field = image.add_field("Value", &[0x06, 0x11, (nested.row() << 2) as u8], 6);
        image.add_type("", "Unused", value_type, 0x109);
        image.add_field("Unused", &[0x06, 0x08], 6);
        image.add_type("", "Host", Token(0), 1);
        let mut il = alloc::vec![0x1f, 16, 0xfe, 0x0f, 0x1e, 0x58, 0x25, 0x02, 0x7d];
        il.extend_from_slice(&field.0.to_le_bytes());
        if !is_enum {
            // Write a nested byte via its address, then return the containing value field.
            il.extend([0x25, 0x7c]);
            il.extend_from_slice(&field.0.to_le_bytes());
            il.extend([0x1f, 37, 0x7d]);
            il.extend_from_slice(&left.0.to_le_bytes());
        }
        il.push(0x7b);
        il.extend_from_slice(&field.0.to_le_bytes());
        il.push(0x2a);
        let mut body = alloc::vec![((il.len() as u8) << 2) | 2];
        body.extend(il);
        let signature = [0, 1, 0x11, (nested.row() << 2) as u8, 0x11, (nested.row() << 2) as u8];
        let entry = image.add_method("Main", &signature, &body, 0x16, 0, &[]);
        let assembly = Assembly::read(Box::leak(image.finish(entry, false).into_boxed_slice())).unwrap();
        let mut program = load_unfrozen(&assembly).unwrap();
        let value = if is_enum { Value::Int32(255) } else {
            Value::Struct(alloc::vec![Value::Int32(14), Value::Int32(-258)].into_boxed_slice())
        };
        let expected = if is_enum { Value::Int32(255) } else {
            Value::Struct(alloc::vec![Value::Int32(37), Value::Int32(-258)].into_boxed_slice())
        };
        let check = |module: &Module| {
            assert_eq!(run(module, &mut Vm::new(), program.entry, alloc::vec![value.clone()]), Ok(Some(expected.clone())));
        };
        check(&program.module);
        #[cfg(feature = "code-in-place")]
        {
            let mut trimmed = program.module.clone();
            let (methods, types, strings) = trimmed.reachable_set(Some(program.entry));
            assert!(types[trimmed.type_id_of(0, nested).unwrap() as usize]);
            trimmed.retain_reachable(&methods, &types, &strings);
            check(&trimmed);
            let image = trimmed.write_baked(Some(program.entry)).unwrap();
            let (baked, _) = Module::from_baked(Box::leak(image.into_boxed_slice())).unwrap();
            check(&baked);
        }
        program.module.freeze();
        check(&program.module);
    }
}
