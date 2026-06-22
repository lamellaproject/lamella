//! The on-device DAP server: a host binary bridging a Debug Adapter Protocol client (VS
//! Code) to a Cortex-M target over the Lamella CMSIS-DAP stack. It links `lamella-dap`
//! WITHOUT the interpreter (`default-features = false`) -- the adapter, the wire protocol,
//! and the `DebugBackend` trait -- driven here by a `DeviceBackend` over a real probe.

use lamella_aot::build;
use lamella_cmsis_dap::{Dap, Transport};
use lamella_cmsis_dap_nrf::Nrf51Flash;
use lamella_debug_device::DeviceBackend;
use lamella_metadata::{Assembly, PortablePdb};
use lamella_usbhid::Device;

/// build_debug's line-table offsets are image-relative (the code sits at image offset 8, after the
/// [SP][reset] vector table, and the image flashes at address 0), so a raw PC indexes the tables
/// directly -- no base to subtract.
const FLASH_BASE: u32 = 0;

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

    let (lines, names, image, file, entry) = source_lines(&program, target.as_ref());

    let device = Device::open(0x0d28, 0x0204, serial.as_deref())
        .expect("open the DAPLink (CMSIS-DAP) probe");
    let mut dap = Dap::new(device);
    flash(&mut dap, &image);
    let backend = DeviceBackend::new(dap, lines, FLASH_BASE, names, file, entry);

    let mut debugger = lamella_dap::Debugger::with_backend(Box::new(backend));
    lamella_dap::serve_polled(
        &mut debugger,
        std::io::BufReader::new(std::io::stdin()),
        &mut std::io::stdout().lock(),
    )
}

/// Flashes a raw image to nRF51 flash at address 0 and resets the target to run it.
fn flash<T: Transport>(dap: &mut Dap<T>, image: &[u8]) {
    let words: Vec<u32> = image
        .chunks(4)
        .map(|c| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(w)
        })
        .collect();
    dap.connect_swd().expect("connect SWD");
    dap.read_idcode().expect("read IDCODE");
    dap.init_mem().expect("init MEM-AP");
    dap.halt().expect("halt");
    let pages = (words.len() * 4).div_ceil(0x400);
    for page in 0..pages as u32 {
        dap.erase_flash_page(page * 0x400).expect("erase page");
    }
    dap.write_flash(0x0, &words).expect("write flash");
    dap.reset_and_run().expect("reset and run");
}

/// Builds the flashable image and composes its native offset -> source map and per-method names:
/// `build_debug`'s per-method line tables (native -> CIL, image-relative) joined to the Portable PDB
/// beside the assembly (CIL -> source line). The `target` selects the source `file` document (its
/// declaring method, or the entry point). Lines are 0 without a PDB (instruction-level).
fn source_lines(
    program: &str,
    target: Option<&(String, String)>,
) -> (Vec<(u32, u32)>, Vec<(u32, String)>, Vec<u8>, String, String) {
    let bytes = std::fs::read(program).expect("read the program assembly");
    let assembly = Assembly::read(&bytes).expect("parse metadata");
    let method = match target {
        Some((type_name, method_name)) => {
            let (namespace, name) = type_name.rsplit_once('.').unwrap_or(("", type_name));
            let type_def = assembly.find_type(namespace, name).expect("type not found");
            type_def
                .methods()
                .find(|m| m.name() == Some(method_name.as_str()))
                .expect("method not found")
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
            type_def
                .methods()
                .find(|m| m.rid() == rid)
                .expect("entry point method not found")
        }
    };
    let entry_rid = method.rid();

    let pdb_bytes = std::fs::read(std::path::Path::new(program).with_extension("pdb")).ok();
    let pdb = pdb_bytes.as_deref().and_then(|b| PortablePdb::read(b).ok());
    let file = pdb
        .as_ref()
        .and_then(|p| p.method_document(entry_rid))
        .map(|doc| {
            let path = std::path::Path::new(&doc);
            if path.is_absolute() {
                return doc;
            }
            std::path::Path::new(program)
                .parent()
                .map(|dir| dir.join(path).to_string_lossy().into_owned())
                .unwrap_or(doc)
        })
        .unwrap_or_default();

    let (image, method_debug) = build::build_debug(&bytes, "microbit").expect("build_debug");
    let mut lines: Vec<(u32, u32)> = Vec::new();
    let mut names: Vec<(u32, String)> = Vec::new();
    for (rid, offset, line_table) in &method_debug {
        names.push((*offset, name_of(&assembly, *rid)));
        for &(native, cil) in &line_table.0 {
            let line = pdb
                .as_ref()
                .and_then(|p| p.source_location(*rid, cil))
                .map_or(0, |sp| sp.start_line);
            lines.push((native, line));
        }
    }
    lines.sort_by_key(|&(native, _)| native);
    names.sort_by_key(|&(offset, _)| offset);
    let entry = name_of(&assembly, entry_rid);
    (lines, names, image, file, entry)
}

/// The `Type.Method` name for a MethodDef `rid`, or a synthetic `rid<N>` for the entry trampoline
/// and stub gaps (which have no real method).
fn name_of(assembly: &Assembly, rid: u32) -> String {
    for type_def in assembly.type_defs() {
        if let Some(method) = type_def.methods().find(|m| m.rid() == rid) {
            let method_name = method.name().unwrap_or("?");
            return match type_def.name() {
                Some(t) if t.namespace.is_empty() => format!("{}.{method_name}", t.name),
                Some(t) => format!("{}.{}.{method_name}", t.namespace, t.name),
                None => method_name.to_string(),
            };
        }
    }
    format!("rid{rid}")
}
