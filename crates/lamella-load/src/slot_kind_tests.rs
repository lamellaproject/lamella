use super::*;
use lamella_pe::ImageBuilder;

/// A fat method body (ECMA-335 6th ed II.25.4.3) of `code`, its locals initialized, with the local
/// variable signature `locals`.
fn fat_body(locals: Token, code: &[u8]) -> Vec<u8> {
    let mut body = alloc::vec![0x13, 0x30, 8, 0];
    body.extend_from_slice(&u32::try_from(code.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&locals.0.to_le_bytes());
    body.extend_from_slice(code);
    body
}

#[test]
fn a_methods_argument_and_local_kinds_come_from_its_signatures() {
    let mut image = ImageBuilder::new("slot-kinds.dll", "slot-kinds");
    let value_type = image.type_ref("System", "ValueType");
    image.add_type("", "Host", Token(0), 1);
    // locals: byte, double, long, int*, object, float (ECMA-335 6th ed II.23.2.6, II.23.1.16)
    let locals = image.add_standalone_sig(&[0x07, 6, 0x05, 0x0D, 0x0A, 0x0F, 0x08, 0x1C, 0x0C]);
    // static void M(float f, ref int r, IntPtr p): a MethodDefSig (ECMA-335 6th ed II.23.2.1, II.23.1.16)
    let method = image.add_method("M", &[0x00, 3, 0x01, 0x0C, 0x10, 0x08, 0x18], &fat_body(locals, &[0x2A]), 0x16, 0, &[]);
    // void Instance(), a class's: HASTHIS, no parameters (ECMA-335 6th ed II.23.2.1)
    let instance = image.add_method("Instance", &[0x20, 0, 0x01], &[0x06, 0x2A], 0x06, 0, &[]);
    image.add_type("", "Record", value_type, 0x109);
    // void Instance(), a struct's: HASTHIS, no parameters (ECMA-335 6th ed II.23.2.1)
    let struct_instance = image.add_method("Instance", &[0x20, 0, 0x01], &[0x06, 0x2A], 0x06, 0, &[]);
    let assembly = Assembly::read(Box::leak(image.finish(method, false).into_boxed_slice()))
        .expect("generated slot-kind metadata");
    let program = load_unfrozen(&assembly).expect("generated slot-kind program");
    let module = &program.module;
    let id = module.resolve(0, method).unwrap();
    let args: Vec<_> = (0..4).map(|slot| module.method_arg_kind(id, slot)).collect();
    assert_eq!(args, [
        Some(SlotKind::Primitive(PrimKind::F4)),
        Some(SlotKind::ByRef),
        Some(SlotKind::Native),
        None,
    ]);
    let locals: Vec<_> = (0..7).map(|slot| module.method_local_kind(id, slot)).collect();
    assert_eq!(locals, [
        Some(SlotKind::Primitive(PrimKind::U1)),
        Some(SlotKind::Primitive(PrimKind::F8)),
        Some(SlotKind::Primitive(PrimKind::I8)),
        Some(SlotKind::Native),
        Some(SlotKind::Reference),
        Some(SlotKind::Primitive(PrimKind::F4)),
        None,
    ]);
    // `this` is an object reference in a class's method and a managed pointer in a struct's
    // (ECMA-335 6th ed II.13.3).
    assert_eq!(module.method_arg_kind(module.resolve(0, instance).unwrap(), 0), Some(SlotKind::Reference));
    assert_eq!(module.method_arg_kind(module.resolve(0, struct_instance).unwrap(), 0), Some(SlotKind::ByRef));
}

/// The one-byte `TypeDefOrRef` coded index of a type token in a signature (ECMA-335 6th ed II.23.2.8 and
/// II.24.2.6), for the small rows these tests write.
fn coded(token: Token) -> u8 {
    let tag = match token.table() {
        TYPE_DEF => 0,
        TYPE_REF => 1,
        _ => 2,
    };
    u8::try_from((token.row() << 2) | tag).expect("a small row")
}

/// Declares in `image`, as its next type, the enum `namespace.name` over the primitive `element`
/// (an ECMA-335 6th ed II.23.1.16 element type) with one member, `value`, and returns its token.
fn add_enum(image: &mut ImageBuilder, enum_base: Token, namespace: &str, name: &str, element: u8, value: &[u8]) -> Token {
    let token = image.add_type(namespace, name, enum_base, 0x101);
    image.add_field("value__", &[0x06, element], 0x0606);
    let member = image.add_field("Member", &[0x06, 0x11, coded(token)], 0x8056);
    image.add_constant(member, element, value);
    token
}

/// A corlib of `System.Object`, `System.ValueType`, `System.Enum`, a `Helper` whose `Use` stores a byte
/// into a local of the enum `System.Later` (over `int16`, declared after `Helper`) and returns it, and
/// the enum `System.DayOfWeek` (over `int32`); and a program whose `Main` stores a byte into a local of
/// its own enum `E` (over `int32`, declared after `Program`) and into one of `DayOfWeek`, a `TypeRef`,
/// and returns the two plus `Helper.Use()`.
fn enum_slot_assemblies() -> (Assembly<'static>, Assembly<'static>) {
    let finish = |image: ImageBuilder, entry: Token| {
        Assembly::read(Box::leak(image.finish(entry, entry.0 == 0).into_boxed_slice())).expect("generated enum-slot metadata")
    };
    let mut corlib = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let object = corlib.add_type("System", "Object", Token(0), 1);
    let value_type = corlib.add_type("System", "ValueType", object, 0x81);
    let enum_base = corlib.add_type("System", "Enum", value_type, 0x81);
    let helper = corlib.add_type("", "Helper", object, 1);
    let later = Token(helper.0 + 1);
    let locals = corlib.add_standalone_sig(&[0x07, 1, 0x11, coded(later)]);
    // ldc.i4 0x0201; stloc.0; ldloca.s 0; ldc.i4.s 0x7f; stind.i1; ldloc.0; ret
    let code = [0x20, 0x01, 0x02, 0, 0, 0x0A, 0x12, 0, 0x1F, 0x7F, 0x52, 0x06, 0x2A];
    corlib.add_method("Use", &[0x00, 0, 0x08], &fat_body(locals, &code), 0x16, 0, &[]);
    assert_eq!(add_enum(&mut corlib, enum_base, "System", "Later", 0x06, &0i16.to_le_bytes()), later);
    add_enum(&mut corlib, enum_base, "System", "DayOfWeek", 0x08, &0i32.to_le_bytes());
    let corlib = finish(corlib, Token(0));

    let mut program = ImageBuilder::new("enum-slots.dll", "enum-slots");
    let enum_ref = program.type_ref("System", "Enum");
    let day = program.type_ref("System", "DayOfWeek");
    let helper_ref = program.type_ref("", "Helper");
    let use_ref = program.member_ref(helper_ref, "Use", &[0x00, 0, 0x08]);
    let program_type = program.add_type("", "Program", Token(0), 1);
    let own = Token(program_type.0 + 1);
    let locals = program.add_standalone_sig(&[0x07, 2, 0x11, coded(own), 0x11, coded(day)]);
    let mut code = alloc::vec![];
    for slot in [0u8, 1] {
        // ldc.i4 0x04030201; stloc.<slot>; ldloca.s <slot>; ldc.i4.s 0x7f; stind.i1
        code.extend_from_slice(&[0x20, 0x01, 0x02, 0x03, 0x04, 0x0A + slot, 0x12, slot, 0x1F, 0x7F, 0x52]);
    }
    // ldloc.0; ldloc.1; add; call Helper::Use; add; ret
    code.extend_from_slice(&[0x06, 0x07, 0x58, 0x28]);
    code.extend_from_slice(&use_ref.0.to_le_bytes());
    code.extend_from_slice(&[0x58, 0x2A]);
    let main = program.add_method("Main", &[0x00, 0, 0x08], &fat_body(locals, &code), 0x16, 0, &[]);
    assert_eq!(add_enum(&mut program, enum_ref, "", "E", 0x08, &0x0403_0201i32.to_le_bytes()), own);
    (corlib, finish(program, main))
}

#[test]
fn an_enum_variable_is_its_underlying_integer_whatever_order_or_assembly_declares_it() {
    for lazy in [false, true] {
        let (corlib, program) = enum_slot_assemblies();
        let loaded = if lazy {
            load_program_lazy_corlib_unfrozen(&corlib, &program).map_err(|error| alloc::format!("{error:?}"))
        } else {
            load_with_corlib_unfrozen(&corlib, &program).map_err(|error| alloc::format!("{error:?}"))
        }
        .expect("the enum-slot program loads");
        let result = lamella_cil_runtime::run(&loaded.module, &mut lamella_cil_runtime::Vm::new(), loaded.entry, Vec::new());
        assert_eq!(result, Ok(Some(Value::Int32(2 * 0x0403_027F + 0x027F))), "lazy corlib: {lazy}");
    }
}

/// A corlib of `System.Object`, `System.ValueType` and the struct `System.Pair { int A; int B; }`; and a
/// program of the struct `Before { int X; }`, then `Program`, then the struct `After { short A; short B; }`
/// (declared after the methods that use it). `Program.Main` stores the int 0x00020001 through the address
/// of an `After` local and the short 0x0102 at byte 2 of a `Before` local, and returns `After.B` plus
/// `Before.X` plus `Program.Corlib()`, which stores 7 at byte 4 of a `Pair` local (a `TypeRef`) and
/// returns `Pair.B`. `Program.Take(Before)` takes a struct argument and returns at once.
fn struct_slot_assemblies() -> (Assembly<'static>, Assembly<'static>, Token) {
    let finish = |image: ImageBuilder, entry: Token| {
        Assembly::read(Box::leak(image.finish(entry, entry.0 == 0).into_boxed_slice())).expect("generated struct-slot metadata")
    };
    let mut corlib = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let object = corlib.add_type("System", "Object", Token(0), 1);
    let value_type = corlib.add_type("System", "ValueType", object, 0x81);
    corlib.add_type("System", "Pair", value_type, 0x109);
    corlib.add_field("A", &[0x06, 0x08], 0x0006);
    corlib.add_field("B", &[0x06, 0x08], 0x0006);
    let corlib = finish(corlib, Token(0));

    let mut program = ImageBuilder::new("struct-slots.dll", "struct-slots");
    let value_type = program.type_ref("System", "ValueType");
    let pair = program.type_ref("System", "Pair");
    let pair_b = program.member_ref(pair, "B", &[0x06, 0x08]);
    let before = program.add_type("", "Before", value_type, 0x109);
    let before_x = program.add_field("X", &[0x06, 0x08], 0x0006);
    let program_type = program.add_type("", "Program", Token(0), 1);
    let after = Token(program_type.0 + 1);
    // After's fields follow Before's one field: `A` is the second row of the Field table, `B` the third.
    let after_b = Token::new(FIELD, before_x.row() + 2);
    let field_of = |code: &mut Vec<u8>, slot: u8, field: Token| {
        // ldloca.s <slot>; ldfld <field>
        code.extend_from_slice(&[0x12, slot, 0x7B]);
        code.extend_from_slice(&field.0.to_le_bytes());
    };
    let pair_local = program.add_standalone_sig(&[0x07, 1, 0x11, coded(pair)]);
    // ldloca.s 0; conv.u; ldc.i4.4; add; ldc.i4.7; stind.i4; ldloca.s 0; ldfld Pair::B; ret
    let mut code = alloc::vec![0x12, 0, 0xE0, 0x1A, 0x58, 0x1D, 0x54];
    field_of(&mut code, 0, pair_b);
    code.push(0x2A);
    let corlib_method = program.add_method("Corlib", &[0x00, 0, 0x08], &fat_body(pair_local, &code), 0x16, 0, &[]);
    let locals = program.add_standalone_sig(&[0x07, 2, 0x11, coded(after), 0x11, coded(before)]);
    // ldloca.s 0; conv.u; ldc.i4 0x00020001; stind.i4
    let mut code = alloc::vec![0x12, 0, 0xE0, 0x20, 0x01, 0x00, 0x02, 0x00, 0x54];
    // ldloca.s 1; conv.u; ldc.i4.2; add; ldc.i4 0x0102; stind.i2
    code.extend_from_slice(&[0x12, 1, 0xE0, 0x18, 0x58, 0x20, 0x02, 0x01, 0x00, 0x00, 0x53]);
    field_of(&mut code, 0, after_b);
    field_of(&mut code, 1, before_x);
    // add; call Program::Corlib; add; ret
    code.extend_from_slice(&[0x58, 0x28]);
    code.extend_from_slice(&corlib_method.0.to_le_bytes());
    code.extend_from_slice(&[0x58, 0x2A]);
    let main = program.add_method("Main", &[0x00, 0, 0x08], &fat_body(locals, &code), 0x16, 0, &[]);
    // static void Take(Before b)
    program.add_method("Take", &[0x00, 1, 0x01, 0x11, coded(before)], &[0x06, 0x2A], 0x16, 0, &[]);
    assert_eq!(program.add_type("", "After", value_type, 0x109), after);
    program.add_field("A", &[0x06, 0x06], 0x0006);
    assert_eq!(program.add_field("B", &[0x06, 0x06], 0x0006), after_b);
    (corlib, finish(program, main), corlib_method)
}

#[test]
fn a_struct_variable_records_its_type_whatever_order_or_assembly_declares_it() {
    for lazy in [false, true] {
        let (corlib, program, _) = struct_slot_assemblies();
        let loaded = if lazy {
            load_program_lazy_corlib_unfrozen(&corlib, &program).map_err(|error| alloc::format!("{error:?}"))
        } else {
            load_with_corlib_unfrozen(&corlib, &program).map_err(|error| alloc::format!("{error:?}"))
        }
        .expect("the struct-slot program loads");
        let result = lamella_cil_runtime::run(&loaded.module, &mut lamella_cil_runtime::Vm::new(), loaded.entry, Vec::new());
        assert_eq!(result, Ok(Some(Value::Int32(2 + 0x0102_0000 + 7))), "lazy corlib: {lazy}");
    }
}

#[test]
fn a_static_records_its_declared_storage() {
    for lazy in [false, true] {
        let finish = |image: ImageBuilder, entry: Token| {
            Assembly::read(Box::leak(image.finish(entry, entry.0 == 0).into_boxed_slice())).expect("generated static-storage metadata")
        };
        let mut corlib = ImageBuilder::new("mscorlib.dll", "mscorlib");
        let object = corlib.add_type("System", "Object", Token(0), 1);
        corlib.add_type("System", "ValueType", object, 0x81);
        let corlib = finish(corlib, Token(0));
        let mut program = ImageBuilder::new("static-storage.dll", "static-storage");
        program.add_type("", "Program", Token(0), 1);
        let field = program.add_field("SI", &[0x06, 0x08], 0x0016);
        let token = field.0.to_le_bytes();
        let mut code = alloc::vec![];
        // ldc.i4 0x04030201; stsfld SI
        code.extend_from_slice(&[0x20, 0x01, 0x02, 0x03, 0x04, 0x80]);
        code.extend_from_slice(&token);
        // ldsflda SI; ldc.i4.s 0x7f; stind.i1
        code.push(0x7F);
        code.extend_from_slice(&token);
        code.extend_from_slice(&[0x1F, 0x7F, 0x52]);
        // ldsfld SI; ret
        code.push(0x7E);
        code.extend_from_slice(&token);
        code.push(0x2A);
        let mut body = alloc::vec![(u8::try_from(code.len()).unwrap() << 2) | 2];
        body.extend(code);
        let main = program.add_method("Main", &[0x00, 0, 0x08], &body, 0x16, 0, &[]);
        let program = finish(program, main);
        let loaded = if lazy {
            load_program_lazy_corlib_unfrozen(&corlib, &program).map_err(|error| alloc::format!("{error:?}"))
        } else {
            load_with_corlib_unfrozen(&corlib, &program).map_err(|error| alloc::format!("{error:?}"))
        }
        .expect("the static-storage program loads");
        let result = lamella_cil_runtime::run(&loaded.module, &mut lamella_cil_runtime::Vm::new(), loaded.entry, Vec::new());
        assert_eq!(result, Ok(Some(Value::Int32(0x0403_027F))), "lazy corlib: {lazy}");
    }
}

#[test]
fn the_this_of_system_enums_and_system_value_types_methods_is_an_object_reference() {
    let mut image = ImageBuilder::new("special-bases.dll", "special-bases");
    let value_type = image.type_ref("System", "ValueType");
    image.add_type("", "Host", Token(0), 1);
    let entry = image.add_method("Main", &[0x00, 0, 0x01], &[0x06, 0x2A], 0x16, 0, &[]);
    image.add_type("System", "Enum", value_type, 0x81);
    let enum_method = image.add_method("HasFlag", &[0x20, 0, 0x01], &[0x06, 0x2A], 0x06, 0, &[]);
    image.add_type("System", "ValueType", value_type, 0x81);
    let value_type_method = image.add_method("Equals", &[0x20, 0, 0x01], &[0x06, 0x2A], 0x06, 0, &[]);
    let assembly = Assembly::read(Box::leak(image.finish(entry, false).into_boxed_slice()))
        .expect("generated special-base metadata");
    let program = load_unfrozen(&assembly).expect("generated special-base program");
    let module = &program.module;
    for method in [enum_method, value_type_method] {
        assert_eq!(module.method_arg_kind(module.resolve(0, method).unwrap(), 0), Some(SlotKind::Reference));
    }
}

#[test]
fn a_char_or_bool_backed_enum_is_its_unsigned_integer() {
    let mut module = Module::new();
    let char_enum = Token::new(TYPE_DEF, 1);
    let bool_enum = Token::new(TYPE_DEF, 2);
    record_enum_underlying(&mut module, 0, char_enum.0, Some(SigType::Char));
    record_enum_underlying(&mut module, 0, bool_enum.0, Some(SigType::Boolean));
    assert_eq!(slot_kind(&module, 0, &SigType::ValueType(char_enum), SlotGenerics::default(), None), SlotKind::Primitive(PrimKind::U2));
    assert_eq!(slot_kind(&module, 0, &SigType::ValueType(bool_enum), SlotGenerics::default(), None), SlotKind::Primitive(PrimKind::U1));
    assert!(enum_constant_is_unsigned(&ConstantValue::Char(1)) && enum_constant_is_unsigned(&ConstantValue::Bool(true)));
    assert_eq!((enum_constant_width(&ConstantValue::Char(1)), enum_constant_width(&ConstantValue::Bool(true))), (2, 1));
    assert!(!enum_constant_is_unsigned(&ConstantValue::I2(-1)));
}

#[test]
fn a_generic_copys_slot_takes_its_arguments_kind() {
    // A copy's `!n` is its type's nth type argument and `!!n` its method's nth (ECMA-335 6th ed
    // II.23.2.12 and II.23.2.15); an argument past the list is a kind the loader did not settle.
    let module = Module::new();
    let generics = SlotGenerics {
        type_arguments: &[SigType::I4, SigType::R4],
        method_arguments: &[SigType::R8],
        ..SlotGenerics::default()
    };
    assert_eq!(slot_kind(&module, 0, &SigType::Var(1), generics, None), SlotKind::Primitive(PrimKind::F4));
    assert_eq!(slot_kind(&module, 0, &SigType::MVar(0), generics, None), SlotKind::Primitive(PrimKind::F8));
    assert_eq!(slot_kind(&module, 0, &SigType::MVar(1), generics, None), SlotKind::Unknown);
    assert_eq!(slot_kind(&module, 0, &SigType::Var(0), SlotGenerics::default(), None), SlotKind::Unknown);
    assert_eq!(slot_kind(&module, 0, &SigType::Pointer(Box::new(SigType::Var(0))), generics, None), SlotKind::Native);
}

#[test]
fn a_pointer_arrays_element_starts_as_the_null_pointer() {
    // `new int*[1]`, then `ptrs[0] == null` (csc: `ldelem.i; ldc.i4.0; conv.u; ceq`): an unmanaged
    // pointer is a native unsigned int (ECMA-335 6th ed I.12.1.1.1) and an array is zero-initialized
    // (II.13.2), so the element is the native int 0 and the comparison is true.
    let mut image = ImageBuilder::new("pointer-array.dll", "pointer-array");
    image.add_type("", "Host", Token(0), 1);
    let pointer = image.type_spec(&[0x0F, 0x08]);
    let mut code = alloc::vec![0x17, 0x8D];
    code.extend_from_slice(&pointer.0.to_le_bytes());
    code.extend_from_slice(&[0x16, 0x97, 0x16, 0xE0, 0xFE, 0x01, 0x2A]);
    let mut body = alloc::vec![(u8::try_from(code.len()).unwrap() << 2) | 2];
    body.extend(code);
    let entry = image.add_method("Main", &[0x00, 0, 0x08], &body, 0x16, 0, &[]);
    let assembly = Assembly::read(Box::leak(image.finish(entry, false).into_boxed_slice()))
        .expect("generated pointer-array metadata");
    let program = load_unfrozen(&assembly).expect("generated pointer-array program");
    let main = program.module.resolve(0, entry).unwrap();
    assert_eq!(lamella_cil_runtime::run(&program.module, &mut lamella_cil_runtime::Vm::new(), main, Vec::new()),
        Ok(Some(Value::Int32(1))));
}
