use super::*;
use lamella_cil_runtime::{boot_baked, run, PInvokeArg, Trap, Vm};
use lamella_pe::ImageBuilder;
use std::sync::atomic::{AtomicUsize, Ordering};

fn body(code: &[u8]) -> Vec<u8> {
    let mut bytes = alloc::vec![((code.len() as u8) << 2) | 2];
    bytes.extend_from_slice(code);
    bytes
}

struct Fixture {
    program: Program,
    unused: Token,
    static_call: Token,
    cctor: Token,
}

fn fixture(native: bool) -> Fixture {
    let mut image = ImageBuilder::new("external-cctor.dll", "external-cctor");
    image.add_type("", "External", Token(0), 1);
    let field = image.add_field("Value", &[0x06, 0x08], 0x16);
    let cctor = if native {
        let cctor = image.add_pinvoke_method(".cctor", &[0, 0, 1], 0x3891, &[]);
        let scope = image.add_module_ref("fixture-native");
        image.add_impl_map(cctor, 0, "initialize", scope);
        cctor
    } else {
        image.add_abstract_method(".cctor", &[0, 0, 1], 0x1891, &[])
    };
    let static_method = image.add_method("Ping", &[0, 0, 0x08], &body(&[0x1f, 19, 0x2a]), 0x16, 0, &[]);
    image.add_type("", "Host", Token(0), 1);
    let mut il = alloc::vec![0x7e];
    il.extend_from_slice(&field.0.to_le_bytes());
    il.push(0x7e);
    il.extend_from_slice(&field.0.to_le_bytes());
    il.extend([0x58, 0x2a]);
    let entry = image.add_method("Main", &[0, 0, 0x08], &body(&il), 0x16, 0, &[]);
    let unused = image.add_method("Unused", &[0, 0, 0x08], &body(&[0x1f, 47, 0x2a]), 0x16, 0, &[]);
    let mut call = alloc::vec![0x28];
    call.extend_from_slice(&static_method.0.to_le_bytes());
    call.push(0x2a);
    let static_call = image.add_method("CallStatic", &[0, 0, 0x08], &body(&call), 0x16, 0, &[]);
    let bytes = Box::leak(image.finish(entry, false).into_boxed_slice());
    let assembly = Assembly::read(bytes).expect("external static constructor fixture");
    Fixture { program: load_unfrozen(&assembly).unwrap(), unused, static_call, cctor }
}

#[test]
fn missing_external_cctor_traps_only_when_its_type_is_used() {
    let mut fixture = fixture(false);
    for frozen in [false, true] {
        if frozen {
            fixture.program.module.freeze();
        }
        let module = &fixture.program.module;
        let mut vm = Vm::new();
        let unused = module.resolve(0, fixture.unused).unwrap();
        assert_eq!(boot_baked(module, &mut vm, unused), Ok(unused));
        assert_eq!(run(module, &mut vm, unused, Vec::new()), Ok(Some(Value::Int32(47))));
        for entry in [fixture.program.entry, module.resolve(0, fixture.static_call).unwrap()] {
            let result = run(module, &mut Vm::new(), entry, Vec::new());
            assert_eq!(result, Err(Trap::UnresolvedCall(fixture.cctor)), "frozen={frozen}");
        }
    }
}

static INITIALIZATIONS: AtomicUsize = AtomicUsize::new(0);

fn native_initializer(target: &PInvokeTarget, args: &mut [PInvokeArg]) -> Result<i64, ()> {
    assert_eq!(target.module, "fixture-native");
    assert_eq!(target.entry, "initialize");
    assert!(args.is_empty());
    INITIALIZATIONS.fetch_add(1, Ordering::SeqCst);
    Ok(0)
}

#[test]
fn external_cctor_uses_the_registered_host_once_at_first_access() {
    let mut fixture = fixture(true);
    for frozen in [false, true] {
        if frozen {
            fixture.program.module.freeze();
        }
        let module = &fixture.program.module;
        INITIALIZATIONS.store(0, Ordering::SeqCst);
        let mut vm = Vm::new();
        vm.set_pinvoke_host(native_initializer);
        assert_eq!(boot_baked(module, &mut vm, fixture.program.entry), Ok(fixture.program.entry));
        let unused = module.resolve(0, fixture.unused).unwrap();
        assert_eq!(run(module, &mut vm, unused, Vec::new()), Ok(Some(Value::Int32(47))));
        assert_eq!(INITIALIZATIONS.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            assert_eq!(run(module, &mut vm, fixture.program.entry, Vec::new()), Ok(Some(Value::Int32(0))));
            assert_eq!(INITIALIZATIONS.load(Ordering::SeqCst), 1, "frozen={frozen}");
        }
        let entry = module.resolve(0, fixture.static_call).unwrap();
        assert_eq!(run(module, &mut vm, entry, Vec::new()), Ok(Some(Value::Int32(19))));
        assert_eq!(INITIALIZATIONS.load(Ordering::SeqCst), 1);
    }
}
