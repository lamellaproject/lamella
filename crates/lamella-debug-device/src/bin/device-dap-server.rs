//! The on-device DAP server: a host binary bridging a Debug Adapter Protocol client (VS
//! Code) to a Cortex-M target over the Lamella CMSIS-DAP stack. It links `lamella-dap`
//! WITHOUT the interpreter (`default-features = false`) -- the adapter, the wire protocol,
//! and the `DebugBackend` trait -- driven here by a `DeviceBackend` over a real probe.

use lamella_aot::{arm32, cil};
use lamella_cmsis_dap::Dap;
use lamella_debug_device::DeviceBackend;
use lamella_metadata::{Assembly, PortablePdb};
use lamella_usbhid::Device;

/// The deployed image places the method's code after an 8-byte vector table (SP + reset).
const FLASH_BASE: u32 = 8;

const USAGE: &str = "usage: device-dap-server <program.dll> [<Type> <Method>] [probe-serial]";

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let program = args.next().expect(USAGE);
    let rest: Vec<String> = args.collect();
    let (target, serial): (Option<(String, String)>, Option<String>) = match rest.len() {
        0 => (None, None),
        1 => (None, Some(rest[0].clone())),
        2 => (Some((rest[0].clone(), rest[1].clone())), None),
        3 => (Some((rest[0].clone(), rest[1].clone())), Some(rest[2].clone())),
        _ => panic!("{USAGE}"),
    };

    let (lines, file, display) = source_lines(&program, target.as_ref());

    let device = Device::open(0x0d28, 0x0204, serial.as_deref())
        .expect("open the DAPLink (CMSIS-DAP) probe");
    let dap = Dap::new(device);
    let backend = DeviceBackend::new(dap, lines, FLASH_BASE, display, file);

    let mut debugger = lamella_dap::Debugger::with_backend(Box::new(backend));
    lamella_dap::serve(
        &mut debugger,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
    )
}

/// Lowers the target method and composes its native offset -> source line: the AOT line table
/// (native -> CIL byte offset) joined to the Portable PDB beside the assembly (CIL byte
/// offset -> source line). Lines are 0 without a PDB (instruction-level). The target is the
/// given `Type`/`Method` pair, or -- when none is given -- the assembly's entry point. Also
/// returns the resolved `Type.Method` display name passed to the backend.
fn source_lines(
    program: &str,
    target: Option<&(String, String)>,
) -> (Vec<(u32, u32)>, String, String) {
    let bytes = std::fs::read(program).expect("read the program assembly");
    let assembly = Assembly::read(&bytes).expect("parse metadata");
    let (type_def, method) = match target {
        Some((type_name, method_name)) => {
            let (namespace, name) = type_name.rsplit_once('.').unwrap_or(("", type_name));
            let type_def = assembly.find_type(namespace, name).expect("type not found");
            let method = type_def
                .methods()
                .find(|m| m.name() == Some(method_name.as_str()))
                .expect("method not found");
            (type_def, method)
        }
        None => {
            let token = assembly.image().entry_point_token();
            assert!(
                token != 0,
                "assembly has no entry point; pass <Type> <Method> explicitly"
            );
            let rid = token & 0x00ff_ffff;
            let type_def = assembly
                .type_defs()
                .find(|type_def| type_def.methods().any(|m| m.rid() == rid))
                .expect("entry point's declaring type not found");
            let method = type_def
                .methods()
                .find(|m| m.rid() == rid)
                .expect("entry point method not found");
            (type_def, method)
        }
    };
    let display = {
        let type_name = type_def.name().expect("type has no name");
        let method_name = method.name().expect("method has no name");
        if type_name.namespace.is_empty() {
            format!("{}.{method_name}", type_name.name)
        } else {
            format!("{}.{}.{method_name}", type_name.namespace, type_name.name)
        }
    };
    let rid = method.rid();
    let body = method.body().expect("method has no CIL body");
    let (func, source_map) = cil::lower_method_debug(&body).expect("CIL -> MIR");
    let (_code, line_table) = arm32::lower_debug(&func, &source_map.0).expect("MIR -> ARM32");

    let pdb_bytes = std::fs::read(std::path::Path::new(program).with_extension("pdb")).ok();
    let pdb = pdb_bytes.as_deref().and_then(|b| PortablePdb::read(b).ok());
    let file = pdb
        .as_ref()
        .and_then(|p| p.method_document(rid))
        .unwrap_or_default();
    let lines = line_table
        .0
        .iter()
        .map(|&(native, cil)| {
            let line = pdb
                .as_ref()
                .and_then(|p| p.source_location(rid, cil))
                .map_or(0, |sp| sp.start_line);
            (native, line)
        })
        .collect();
    (lines, file, display)
}
