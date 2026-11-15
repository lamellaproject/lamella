use super::*;
use lamella_cil_runtime::intrinsics::get_custom_attributes;
use lamella_cil_runtime::Vm;
use lamella_pe::ImageBuilder;

#[test]
fn event_attributes_are_loaded_for_each_declaring_type_and_declarator() {
    let mut image = ImageBuilder::new("event-attributes.dll", "event-attributes");
    image.add_type("", "MarkAttribute", Token(0), 1);
    let ctor = image.add_method(".ctor", &[0x20, 1, 0x01, 0x08], &[0x06, 0x2a], 0x1886, 0, &[]);
    let mut events = Vec::new();
    for owner in ["First", "Second"] {
        let type_token = image.add_type("", owner, Token(0), 1);
        let mut first = None;
        for name in ["East", "West"] {
            let event = image.add_event(name, type_token);
            first.get_or_insert(event);
            let value = 17 + events.len() as i32;
            let mut blob = alloc::vec![1, 0];
            blob.extend_from_slice(&value.to_le_bytes());
            blob.extend_from_slice(&[0, 0]);
            image.add_custom_attribute(event, ctor, &blob);
            events.push((type_token, name, event, value));
        }
        image.add_event_map(type_token, first.unwrap());
    }
    image.add_type("", "Host", Token(0), 1);
    let entry = image.add_method("Main", &[0, 0, 0x01], &[0x06, 0x2a], 0x16, 0, &[]);
    let bytes = Box::leak(image.finish(entry, false).into_boxed_slice());
    let assembly = Assembly::read(bytes).expect("generated event metadata");
    let mut program = load_unfrozen(&assembly).expect("load event metadata");
    let check = |module: &Module, phase: &str| {
        for &(owner, name, event, code) in &events {
            let handle = asm_key(0, event.0);
            assert_eq!(module.type_event_handle(asm_key(0, owner.0), name), Some(handle), "{phase}");
            let attributes = module.custom_attributes_of(handle);
            assert_eq!(attributes.len(), 1, "{phase}: {event:?}");
            assert!(matches!(attributes[0].positional.as_slice(),
                [lamella_cil_runtime::module::AttrValue::Int { value, wide: false }] if *value == i64::from(code)));
            let mut vm = Vm::new();
            let value = get_custom_attributes(&mut vm, module, &[Value::NativeInt(handle as i64)])
                .expect("instantiate event attribute");
            let Some(Value::Object(array)) = value else { panic!("attribute array") };
            assert_eq!(vm.heap().array_len(array), Some(1));
        }
    };
    check(&program.module, "live");
    program.module.freeze();
    check(&program.module, "frozen");
}
