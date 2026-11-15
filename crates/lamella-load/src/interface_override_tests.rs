use super::*;
use lamella_cil_runtime::{run, Value, Vm};
use lamella_pe::ImageBuilder;

const INSTANCE_INT: &[u8] = &[0x20, 0, 0x08];
const PUBLIC_VIRTUAL: u16 = 0x0146;
const EXPLICIT: u16 = 0x0161;

fn constant(image: &mut ImageBuilder, name: &str, value: u8, flags: u16) -> Token {
    image.add_method(name, INSTANCE_INT, &[0x0e, 0x1f, value, 0x2a], flags, 0, &[])
}

struct Fixture {
    program: Program,
    types: BTreeMap<&'static str, Token>,
    invoke_other: Token,
}

impl Fixture {
    fn read(&self, name: &str, other: bool) -> i32 {
        let mut vm = Vm::new();
        let module = &self.program.module;
        let ty = module.type_id_of(0, self.types[name]).expect("receiver type");
        let receiver = vm.heap_mut().alloc_instance(ty, Vec::new());
        let method = if other {
            module.resolve(0, self.invoke_other).expect("other interface caller")
        } else {
            self.program.entry
        };
        let result = run(module, &mut vm, method, alloc::vec![Value::Object(receiver)]);
        match result {
            Ok(Some(Value::Int32(value))) => value,
            result => panic!("{name}, other={other}: {result:?}"),
        }
    }
}

fn fixture() -> Fixture {
    let mut image = ImageBuilder::new("interfaces.dll", "interfaces");
    let nil = Token(0);
    let read = image.add_type("", "IRead", nil, 0xa1);
    let read_method = image.add_abstract_method("Read", INSTANCE_INT, 0x0546, &[]);
    let other = image.add_type("", "IOther", nil, 0xa1);
    let other_method = image.add_abstract_method("Read", INSTANCE_INT, 0x0546, &[]);
    let child = image.add_type("", "IChild", nil, 0xa1);
    image.add_interface_impl(child, read);

    let mut types = BTreeMap::new();
    let root = image.add_type("", "Root", nil, 1);
    types.insert("Root", root);
    image.add_interface_impl(root, read);
    image.add_interface_impl(root, other);
    let body = constant(&mut image, "IRead.Read", 11, EXPLICIT);
    image.add_method_impl(root, body, read_method);
    let body = constant(&mut image, "IOther.Read", 12, EXPLICIT);
    image.add_method_impl(root, body, other_method);
    constant(&mut image, "Read", 99, 6);

    let inherited = image.add_type("", "Inherited", root, 1);
    types.insert("Inherited", inherited);
    let relisted = image.add_type("", "Relisted", inherited, 1);
    types.insert("Relisted", relisted);
    image.add_interface_impl(relisted, read);

    let own = image.add_type("", "Own", root, 1);
    types.insert("Own", own);
    image.add_interface_impl(own, read);
    let body = constant(&mut image, "IRead.Read", 21, EXPLICIT);
    image.add_method_impl(own, body, read_method);

    let implicit = image.add_type("", "Implicit", root, 1);
    types.insert("Implicit", implicit);
    image.add_interface_impl(implicit, read);
    constant(&mut image, "Read", 31, PUBLIC_VIRTUAL | 0x20);
    let leaf = image.add_type("", "ImplicitLeaf", implicit, 1);
    types.insert("ImplicitLeaf", leaf);

    let hidden = image.add_type("", "Hidden", root, 1);
    types.insert("Hidden", hidden);
    constant(&mut image, "Read", 41, PUBLIC_VIRTUAL);

    let child_relisted = image.add_type("", "ChildRelisted", root, 1);
    types.insert("ChildRelisted", child_relisted);
    image.add_interface_impl(child_relisted, child);
    constant(&mut image, "Read", 51, PUBLIC_VIRTUAL);

    let virtual_impl = image.add_type("", "VirtualImpl", root, 1);
    types.insert("VirtualImpl", virtual_impl);
    image.add_interface_impl(virtual_impl, read);
    constant(&mut image, "Read", 61, PUBLIC_VIRTUAL);
    let virtual_leaf = image.add_type("", "VirtualLeaf", virtual_impl, 1);
    types.insert("VirtualLeaf", virtual_leaf);
    constant(&mut image, "Read", 62, 0x0046);

    let other_relisted = image.add_type("", "OtherRelisted", root, 1);
    types.insert("OtherRelisted", other_relisted);
    image.add_interface_impl(other_relisted, other);
    constant(&mut image, "Read", 71, PUBLIC_VIRTUAL);

    image.add_type("", "Host", nil, 1);
    let mut invoke = |name: &str, declaration: Token| {
        let mut body = alloc::vec![0x1e, 0x02, 0x6f];
        body.extend_from_slice(&declaration.0.to_le_bytes());
        body.push(0x2a);
        image.add_method(name, &[0, 1, 0x08, 0x1c], &body, 0x16, 0, &[])
    };
    let entry = invoke("Invoke", read_method);
    let invoke_other = invoke("InvokeOther", other_method);
    let bytes = Box::leak(image.finish(entry, false).into_boxed_slice());
    let assembly = Assembly::read(bytes).expect("generated interface metadata");
    Fixture { program: load(&assembly).expect("generated interface program"), types, invoke_other }
}

#[test]
fn inherited_explicit_implementation_precedes_public_member_in_its_class() {
    let fixture = fixture();
    for name in ["Root", "Inherited", "Relisted"] {
        assert_eq!(fixture.read(name, false), 11, "{name}");
    }
}

#[test]
fn explicit_implementation_keeps_interface_identity() {
    let fixture = fixture();
    assert_eq!(fixture.read("Own", false), 21);
    assert_eq!(fixture.read("Own", true), 12);
    assert_eq!(fixture.read("OtherRelisted", false), 11);
    assert_eq!(fixture.read("OtherRelisted", true), 71);
}

#[test]
fn public_reimplementation_displaces_inherited_explicit_implementation() {
    let fixture = fixture();
    for name in ["Implicit", "ImplicitLeaf"] {
        assert_eq!(fixture.read(name, false), 31, "{name}");
    }
    assert_eq!(fixture.read("ChildRelisted", false), 51);
    assert_eq!(fixture.read("ChildRelisted", true), 12);
}

#[test]
fn hiding_keeps_mapping_while_virtual_overrides_replace_implementation() {
    let fixture = fixture();
    assert_eq!(fixture.read("VirtualImpl", false), 61);
    assert_eq!(fixture.read("VirtualLeaf", false), 62);
    assert_eq!(fixture.read("Hidden", false), 11);
}

struct CrossAssemblyFixture {
    corlib: Assembly<'static>,
    library: Assembly<'static>,
    caller: Assembly<'static>,
    receivers: Vec<(u8, Token, i32)>,
    calls: Vec<(u8, Token, bool)>,
}

fn generated_assembly(image: ImageBuilder, entry: Token) -> Assembly<'static> {
    let bytes = Box::leak(image.finish(entry, entry.0 == 0).into_boxed_slice());
    Assembly::read(bytes).expect("generated assembly metadata")
}

fn interface_call(image: &mut ImageBuilder, name: &str, target: Token) -> Token {
    let mut body = alloc::vec![0x1e, 0x02, 0x6f];
    body.extend_from_slice(&target.0.to_le_bytes());
    body.push(0x2a);
    image.add_method(name, &[0, 1, 0x08, 0x1c], &body, 0x16, 0, &[])
}

fn cross_assembly_fixture() -> CrossAssemblyFixture {
    let nil = Token(0);
    let mut contract = ImageBuilder::new("contracts.dll", "contracts");
    let read = contract.add_type("", "IRead", nil, 0xa1);
    let declaration = contract.add_abstract_method("Read", INSTANCE_INT, 0x0546, &[]);
    let other = contract.add_type("", "IOther", nil, 0xa1);
    contract.add_abstract_method("Read", INSTANCE_INT, 0x0546, &[]);
    contract.add_type("", "ContractCalls", nil, 1);
    let contract_call = interface_call(&mut contract, "Read", declaration);
    let corlib = generated_assembly(contract, nil);
    assert_ne!(read, other);

    let mut library = ImageBuilder::new("implementations.dll", "implementations");
    for name in ["IRead", "IOther"] {
        library.set_type_assembly(name, "contracts");
    }
    let read_ref = library.type_ref("", "IRead");
    let other_ref = library.type_ref("", "IOther");
    let read_decl = library.member_ref(read_ref, "Read", INSTANCE_INT);
    let other_decl = library.member_ref(other_ref, "Read", INSTANCE_INT);
    let root = library.add_type("", "Root", nil, 1);
    library.add_interface_impl(root, read_ref);
    library.add_interface_impl(root, other_ref);
    let body = constant(&mut library, "IRead.Read", 11, EXPLICIT);
    library.add_method_impl(root, body, read_decl);
    let body = constant(&mut library, "IOther.Read", 12, EXPLICIT);
    library.add_method_impl(root, body, other_decl);
    let inherited = library.add_type("", "Inherited", root, 1);
    library.add_type("", "LibraryCalls", nil, 1);
    let duplicate_read = library.member_ref(read_ref, "Read", INSTANCE_INT);
    let library_call = interface_call(&mut library, "Read", duplicate_read);
    let library = generated_assembly(library, nil);

    let mut caller = ImageBuilder::new("caller.dll", "caller");
    for name in ["IRead", "IOther"] {
        caller.set_type_assembly(name, "contracts");
    }
    caller.set_type_assembly("Root", "implementations");
    let root_ref = caller.type_ref("", "Root");
    let read_ref = caller.type_ref("", "IRead");
    let other_ref = caller.type_ref("", "IOther");
    let read_call = caller.member_ref(read_ref, "Read", INSTANCE_INT);
    let duplicate_call = caller.member_ref(read_ref, "Read", INSTANCE_INT);
    let other_call = caller.member_ref(other_ref, "Read", INSTANCE_INT);
    let external_heir = caller.add_type("", "ExternalHeir", root_ref, 1);
    let hidden = caller.add_type("", "Hidden", root_ref, 1);
    constant(&mut caller, "Read", 99, PUBLIC_VIRTUAL);
    let relisted = caller.add_type("", "Relisted", root_ref, 1);
    caller.add_interface_impl(relisted, read_ref);
    constant(&mut caller, "Read", 31, PUBLIC_VIRTUAL);
    caller.add_type("", "Caller", nil, 1);
    let entry = interface_call(&mut caller, "Read", read_call);
    let duplicate = interface_call(&mut caller, "ReadAgain", duplicate_call);
    let other = interface_call(&mut caller, "ReadOther", other_call);
    let caller = generated_assembly(caller, entry);
    CrossAssemblyFixture {
        corlib,
        library,
        caller,
        receivers: alloc::vec![(1, root, 11), (1, inherited, 11), (2, external_heir, 11),
            (2, hidden, 11), (2, relisted, 31)],
        calls: alloc::vec![(0, contract_call, false), (1, library_call, false),
            (2, entry, false), (2, duplicate, false), (2, other, true)],
    }
}

fn check_cross_assembly_calls(fixture: &CrossAssemblyFixture, module: &Module, phase: &str) {
    for &(asm, token, read_value) in &fixture.receivers {
        let ty = module.type_id_of(asm, token).expect("cross-assembly receiver type");
        for &(caller_asm, caller, other) in &fixture.calls {
            let method = module.resolve(caller_asm, caller).expect("cross-assembly caller");
            let mut vm = Vm::new();
            let receiver = vm.heap_mut().alloc_instance(ty, Vec::new());
            assert_eq!(run(module, &mut vm, method, alloc::vec![Value::Object(receiver)]),
                Ok(Some(Value::Int32(if other { 12 } else { read_value }))),
                "{phase}: receiver {asm}:{token:?}, caller {caller_asm}:{caller:?}");
        }
    }
}

#[test]
fn explicit_interfaces_cross_assembly_tokens_and_inheritance() {
    let fixture = cross_assembly_fixture();
    let mut program = load_with_corlib_and_libraries_unfrozen(
        &fixture.corlib, &[fixture.library.clone()], &fixture.caller,
    ).expect("load cross-assembly interfaces");
    check_cross_assembly_calls(&fixture, &program.module, "live");
    program.module.freeze();
    check_cross_assembly_calls(&fixture, &program.module, "frozen");
    #[cfg(feature = "code-in-place")]
    {
        let image = program.module.write_baked(Some(program.entry)).expect("bake cross-assembly interfaces");
        let (module, _) = Module::from_baked(Box::leak(image.into_boxed_slice())).expect("read interface image");
        check_cross_assembly_calls(&fixture, &module, "baked");
    }
}

#[test]
fn explicit_interfaces_cross_assembly_with_lazy_corlib() {
    let mut fixture = cross_assembly_fixture();
    let empty_corlib = generated_assembly(ImageBuilder::new("empty.dll", "empty"), Token(0));
    let mut program = load_program_lazy_corlib_with_libraries_unfrozen(
        &empty_corlib, &[fixture.library.clone(), fixture.corlib.clone()], &fixture.caller,
    ).expect("load interfaces with lazy corlib and reversed library order");
    for (asm, _, _) in &mut fixture.receivers {
        *asm += 1;
    }
    for (asm, _, _) in &mut fixture.calls {
        *asm += 1;
    }
    check_cross_assembly_calls(&fixture, &program.module, "lazy live");
    program.module.freeze();
    check_cross_assembly_calls(&fixture, &program.module, "lazy frozen");
    #[cfg(feature = "code-in-place")]
    {
        let image = program.module.write_baked(Some(program.entry)).expect("bake lazy interfaces");
        let (module, _) = Module::from_baked(Box::leak(image.into_boxed_slice())).expect("read lazy interface image");
        check_cross_assembly_calls(&fixture, &module, "lazy baked");
    }
}

#[test]
fn explicit_interfaces_cross_assembly_keep_parameter_and_return_types() {
    let signatures: &[&[u8]] = &[
        INSTANCE_INT, &[0x20, 1, 0x08, 0x08], &[0x20, 1, 0x08, 0x1c], &[0x20, 0, 0x0a],
    ];
    let nil = Token(0);
    let mut library = ImageBuilder::new("overloads.dll", "overloads");
    let interface = library.add_type("", "IOverloaded", nil, 0xa1);
    let declarations: Vec<Token> = signatures.iter().map(|signature| {
        library.add_abstract_method("Read", signature, 0x0546, &[])
    }).collect();
    let root = library.add_type("", "Overloaded", nil, 1);
    library.add_interface_impl(root, interface);
    for (index, signature) in signatures.iter().enumerate() {
        let mut il = alloc::vec![0x1f, (11 + index) as u8];
        if index == 3 {
            il.push(0x6a); // conv.i8
        }
        il.push(0x2a);
        let mut body = alloc::vec![((il.len() as u8) << 2) | 2];
        body.extend(il);
        let method = library.add_method("IOverloaded.Read", signature, &body, EXPLICIT, 0, &[]);
        library.add_method_impl(root, method, declarations[index]);
    }
    let library = generated_assembly(library, nil);
    let mut caller = ImageBuilder::new("overload-caller.dll", "overload-caller");
    caller.set_type_assembly("IOverloaded", "overloads");
    let interface = caller.type_ref("", "IOverloaded");
    caller.add_type("", "Caller", nil, 1);
    let mut calls = Vec::new();
    for (index, signature) in signatures.iter().enumerate() {
        let target = caller.member_ref(interface, "Read", signature);
        let mut il = alloc::vec![0x02];
        match index {
            1 => il.push(0x17), // ldc.i4.1
            2 => il.push(0x14), // ldnull
            _ => {}
        }
        il.push(0x6f);
        il.extend_from_slice(&target.0.to_le_bytes());
        il.push(0x2a);
        let mut body = alloc::vec![((il.len() as u8) << 2) | 2];
        body.extend(il);
        calls.push(caller.add_method(&alloc::format!("Call{index}"), &[0, 1, signature[2], 0x1c],
            &body, 0x16, 0, &[]));
    }
    let caller = generated_assembly(caller, calls[0]);
    let program = load_with_corlib(&library, &caller).expect("load overloaded interface");
    let ty = program.module.type_id_of(0, root).expect("overloaded receiver");
    for (index, token) in calls.into_iter().enumerate() {
        let mut vm = Vm::new();
        let receiver = vm.heap_mut().alloc_instance(ty, Vec::new());
        let method = program.module.resolve(1, token).expect("overloaded caller");
        let expected = if index == 3 { Value::Int64(14) } else { Value::Int32(11 + index as i32) };
        assert_eq!(run(&program.module, &mut vm, method, alloc::vec![Value::Object(receiver)]),
            Ok(Some(expected)), "overload {index}");
    }
}
