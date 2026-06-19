//! The on-device DAP server: a host binary bridging a Debug Adapter Protocol client (VS
//! Code) to a Cortex-M target over the Lamella CMSIS-DAP stack. It links `lamella-dap`
//! WITHOUT the interpreter (`default-features = false`) -- the adapter, the wire protocol,
//! and the `DebugBackend` trait -- driven here by a `DeviceBackend` over a real probe.

use lamella_cmsis_dap::Dap;
use lamella_debug_device::DeviceBackend;
use lamella_usbhid::Device;

fn main() -> std::io::Result<()> {
    let serial = std::env::args().nth(1);
    let device = Device::open(0x0d28, 0x0204, serial.as_deref())
        .expect("open the DAPLink (CMSIS-DAP) probe");
    let dap = Dap::new(device);

    let backend = DeviceBackend::new(dap, Vec::new(), 8, "Run".into());

    let mut debugger = lamella_dap::Debugger::with_backend(Box::new(backend));
    lamella_dap::serve(
        &mut debugger,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
    )
}
