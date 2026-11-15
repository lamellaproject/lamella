use super::*;
use lamella_cil_runtime::{run, Location, Vm};
use lamella_metadata::signature::parse_method;
use lamella_pe::ImageBuilder;

#[test]
fn fixed_string_offset_getter_binds_and_runs_in_each_storage_mode() {
    let mut image = ImageBuilder::new("fixed-string.dll", "fixed-string");
    image.add_type("", "Host", Token(0), 1);
    let helpers = image.type_ref("System.Runtime.CompilerServices", "RuntimeHelpers");
    let getter = image.member_ref(helpers, "get_OffsetToStringData", &[0, 0, 0x08]);
    let mut offset_il = alloc::vec![0x28];
    offset_il.extend_from_slice(&getter.0.to_le_bytes());
    offset_il.push(0x2a);
    let mut offset_body = alloc::vec![((offset_il.len() as u8) << 2) | 2];
    offset_body.extend(offset_il);
    let offset = image.add_method("Offset", &[0, 0, 0x08], &offset_body, 0x16, 0, &[]);
    // ldarg.0; conv.i; call get_OffsetToStringData; add; ret
    let mut pin_il = alloc::vec![0x02, 0xd3, 0x28];
    pin_il.extend_from_slice(&getter.0.to_le_bytes());
    pin_il.extend([0x58, 0x2a]);
    let mut pin_body = alloc::vec![((pin_il.len() as u8) << 2) | 2];
    pin_body.extend(pin_il);
    let pin = image.add_method("Pin", &[0, 1, 0x18, 0x0e], &pin_body, 0x16, 0, &[]);
    let assembly = Assembly::read(Box::leak(image.finish(offset, false).into_boxed_slice()))
        .expect("generated fixed-string metadata");
    let mut program = load_unfrozen(&assembly).expect("generated fixed-string program");
    let check = |module: &Module, phase: &str| {
        let mut vm = Vm::new();
        let offset = module.resolve(0, offset).unwrap();
        assert_eq!(run(module, &mut vm, offset, Vec::new()), Ok(Some(Value::Int32(0))), "{phase}");
        let pin = module.resolve(0, pin).unwrap();
        let string = vm.heap_mut().alloc_text("s\0t");
        assert_eq!(run(module, &mut vm, pin, alloc::vec![Value::Object(string)]),
            Ok(Some(Value::ByRef(Location::StringChar { string, byte_offset: 0 }))), "{phase}");
    };
    check(&program.module, "live");
    program.module.freeze();
    check(&program.module, "frozen");
    #[cfg(feature = "code-in-place")]
    {
        let baked = program.module.write_baked(Some(program.entry)).expect("bake fixed-string program");
        let (module, _) = Module::from_baked(Box::leak(baked.into_boxed_slice())).expect("read fixed-string program");
        check(&module, "baked");
    }
}

#[test]
fn fixed_string_offset_getter_requires_its_exact_signature() {
    let bind = |signature: Option<&MethodSig>| bcl_intrinsic(
        "System.Runtime.CompilerServices", "RuntimeHelpers", "get_OffsetToStringData", signature);
    let correct = parse_method(&[0, 0, 0x08]).unwrap();
    assert!(bind(Some(&correct)).is_some());
    assert!(bind(None).is_none());
    for bytes in [
        &[0x20, 0, 0x08][..], // Instance.
        &[0x60, 0, 0x08], // Explicit this.
        &[0, 1, 0x08, 0x08], // Parameter.
        &[0, 0, 0x01], // Void return.
        &[0, 0, 0x18], // Native-int return.
        &[0x05, 0, 0x08], // Vararg.
        &[0x10, 1, 0, 0x08], // Generic.
    ] {
        let signature = parse_method(bytes).unwrap();
        assert!(bind(Some(&signature)).is_none(), "{bytes:?}");
    }
    for (namespace, type_name, method) in [
        ("Other", "RuntimeHelpers", "get_OffsetToStringData"),
        ("System.Runtime.CompilerServices", "Other", "get_OffsetToStringData"),
        ("System.Runtime.CompilerServices", "RuntimeHelpers", "OffsetToStringData"),
    ] {
        assert!(bcl_intrinsic(namespace, type_name, method, Some(&correct)).is_none());
    }
}
