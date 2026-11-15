use super::*;
use lamella_cil_runtime::{PInvokeArg, Trap, Vm, boot_baked, run};
use lamella_pe::ImageBuilder;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug)]
enum Loading {
    Single,
    Eager,
    Lazy,
}

#[derive(Clone, Copy)]
enum Initializer {
    Missing,
    Native,
    Managed,
}

struct Fixture {
    program: Program,
    asm: u8,
    cctor: Token,
    reads: Vec<Token>,
    calls: Vec<Token>,
    unused: Token,
}

fn body(code: &[u8]) -> Vec<u8> {
    let mut bytes = alloc::vec![((code.len() as u8) << 2) | 2];
    bytes.extend_from_slice(code);
    bytes
}

fn read_assembly(image: ImageBuilder, entry: Token) -> Assembly<'static> {
    let bytes = Box::leak(image.finish(entry, entry.0 == 0).into_boxed_slice());
    Assembly::read(bytes).unwrap()
}

fn fixture(loading: Loading, generic: bool, initializer: Initializer) -> Fixture {
    let mut image = ImageBuilder::new("mscorlib.dll", "mscorlib");
    let name = if generic { "External`1" } else { "External" };
    let owner = image.add_type("", name, Token(0), 1);
    if generic {
        image.add_generic_param(owner, 0, 0, "T");
    }
    let field = image.add_field("Value", &[0x06, 0x08], 0x16);
    let cctor = match initializer {
        Initializer::Missing => image.add_abstract_method(".cctor", &[0, 0, 1], 0x1891, &[]),
        Initializer::Native => {
            let method = image.add_pinvoke_method(".cctor", &[0, 0, 1], 0x3891, &[]);
            let scope = image.add_module_ref("fixture-native");
            image.add_impl_map(method, 0, "initialize", scope);
            method
        }
        Initializer::Managed => {
            let mut code = alloc::vec![0x1f, 42, 0x80];
            code.extend_from_slice(&field.0.to_le_bytes());
            code.push(0x2a);
            image.add_method(".cctor", &[0, 0, 1], &body(&code), 0x1891, 0, &[])
        }
    };
    let ping = image.add_method(
        "Ping",
        &[0, 0, 0x08],
        &body(&[0x1f, 19, 0x2a]),
        0x16,
        0,
        &[],
    );
    let corlib = if matches!(loading, Loading::Single) {
        None
    } else {
        let corlib = read_assembly(image, Token(0));
        image = ImageBuilder::new("caller.dll", "caller");
        Some(corlib)
    };
    let target = if corlib.is_some() {
        image.type_ref("", name)
    } else {
        owner
    };
    image.add_type("", "Host", Token(0), 1);
    let mut reads = Vec::new();
    let mut calls = Vec::new();
    for argument in if generic {
        &[0x08, 0x0a][..]
    } else {
        &[0x08][..]
    } {
        let target = if generic {
            let coded = (target.row() << 2) | u32::from(target.table() == TYPE_REF);
            image.type_spec(&[0x15, 0x12, u8::try_from(coded).unwrap(), 1, *argument])
        } else {
            target
        };
        let (field, ping) = if generic || corlib.is_some() {
            (
                image.member_ref(target, "Value", &[0x06, 0x08]),
                image.member_ref(target, "Ping", &[0, 0, 0x08]),
            )
        } else {
            (field, ping)
        };
        let mut code = alloc::vec![0x7e];
        code.extend_from_slice(&field.0.to_le_bytes());
        code.push(0x2a);
        reads.push(image.add_method(
            &alloc::format!("Read{argument}"),
            &[0, 0, 0x08],
            &body(&code),
            0x16,
            0,
            &[],
        ));
        code[0] = 0x28;
        code[1..5].copy_from_slice(&ping.0.to_le_bytes());
        calls.push(image.add_method(
            &alloc::format!("Call{argument}"),
            &[0, 0, 0x08],
            &body(&code),
            0x16,
            0,
            &[],
        ));
    }
    let unused = image.add_method(
        "Unused",
        &[0, 0, 0x08],
        &body(&[0x1f, 47, 0x2a]),
        0x16,
        0,
        &[],
    );
    let caller = read_assembly(image, reads[0]);
    let (program, asm) = match loading {
        Loading::Single if generic => {
            let corlib = read_assembly(ImageBuilder::new("empty.dll", "empty"), Token(0));
            (load_with_corlib_unfrozen(&corlib, &caller).unwrap(), 1)
        }
        Loading::Single => (load_unfrozen(&caller).unwrap(), 0),
        Loading::Eager => (
            load_with_corlib_unfrozen(corlib.as_ref().unwrap(), &caller).unwrap(),
            1,
        ),
        Loading::Lazy => (
            load_program_lazy_corlib_unfrozen(corlib.as_ref().unwrap(), &caller).unwrap(),
            1,
        ),
    };
    Fixture {
        program,
        asm,
        cctor,
        reads,
        calls,
        unused,
    }
}

#[test]
fn missing_external_cctors_trap_on_every_registration_path() {
    let mut failures = Vec::new();
    for loading in [Loading::Single, Loading::Eager, Loading::Lazy] {
        for generic in [false, true] {
            let mut fixture = fixture(loading, generic, Initializer::Missing);
            for frozen in [false, true] {
                if frozen {
                    fixture.program.module.freeze();
                }
                let module = &fixture.program.module;
                let unused = module.resolve(fixture.asm, fixture.unused).unwrap();
                let mut vm = Vm::new();
                assert_eq!(boot_baked(module, &mut vm, unused), Ok(unused));
                assert_eq!(
                    run(module, &mut vm, unused, Vec::new()),
                    Ok(Some(Value::Int32(47)))
                );
                for token in fixture.reads.iter().chain(&fixture.calls) {
                    let entry = module.resolve(fixture.asm, *token).unwrap();
                    let result = run(module, &mut Vm::new(), entry, Vec::new());
                    if result != Err(Trap::UnresolvedCall(fixture.cctor)) {
                        failures.push(alloc::format!(
                            "{loading:?} generic={generic} frozen={frozen} {token:?}: {result:?}"
                        ));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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
fn native_cctors_run_once_per_closed_type_on_every_registration_path() {
    let mut failures = Vec::new();
    for loading in [Loading::Single, Loading::Eager, Loading::Lazy] {
        for generic in [false, true] {
            let mut fixture = fixture(loading, generic, Initializer::Native);
            for frozen in [false, true] {
                if frozen {
                    fixture.program.module.freeze();
                }
                let module = &fixture.program.module;
                for call_first in [false, true] {
                    INITIALIZATIONS.store(0, Ordering::SeqCst);
                    let mut vm = Vm::new();
                    vm.set_pinvoke_host(native_initializer);
                    let unused = module.resolve(fixture.asm, fixture.unused).unwrap();
                    assert_eq!(boot_baked(module, &mut vm, unused), Ok(unused));
                    assert_eq!(
                        run(module, &mut vm, unused, Vec::new()),
                        Ok(Some(Value::Int32(47)))
                    );
                    assert_eq!(INITIALIZATIONS.load(Ordering::SeqCst), 0);
                    for index in 0..fixture.reads.len() {
                        let read = (fixture.reads[index], 0);
                        let call = (fixture.calls[index], 19);
                        for (token, expected) in if call_first {
                            [call, read, call]
                        } else {
                            [read, call, read]
                        } {
                            let entry = module.resolve(fixture.asm, token).unwrap();
                            let result = run(module, &mut vm, entry, Vec::new());
                            let count = INITIALIZATIONS.load(Ordering::SeqCst);
                            if result != Ok(Some(Value::Int32(expected))) || count != index + 1 {
                                failures.push(alloc::format!("{loading:?} generic={generic} frozen={frozen} call_first={call_first} instance={index}: {result:?}, initializations={count}"));
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn managed_cctors_initialize_each_closed_types_own_field_on_every_registration_path() {
    for loading in [Loading::Single, Loading::Eager, Loading::Lazy] {
        for generic in [false, true] {
            let mut fixture = fixture(loading, generic, Initializer::Managed);
            for frozen in [false, true] {
                if frozen {
                    fixture.program.module.freeze();
                }
                let module = &fixture.program.module;
                let mut vm = Vm::new();
                for token in &fixture.reads {
                    let entry = module.resolve(fixture.asm, *token).unwrap();
                    assert_eq!(
                        run(module, &mut vm, entry, Vec::new()),
                        Ok(Some(Value::Int32(42))),
                        "{loading:?} generic={generic} frozen={frozen}"
                    );
                }
            }
        }
    }
}
