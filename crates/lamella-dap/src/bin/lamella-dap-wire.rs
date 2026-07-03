//! The wireline DEVICE DAP server: VS Code speaks DAP-JSON over stdio to this binary,
//! which speaks wireline frames over a serial port to the on-device interpreter. The
//! adapter is unchanged -- [`lamella_wireline::debug_backend::WirelineBackend`] implements
//! the same `DebugBackend` seam the host interpreter backend does, and the polled serve
//! loop surfaces asynchronous stops (a breakpoint the device hits) on their own.

use lamella_wireline::debug_backend::WirelineBackend;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: lamella-dap-wire <COM-port> <image.lmli> [baud]";
    let port = args.next().expect(usage);
    let image_path = args.next().expect(usage);
    let baud: u32 = args.next().map_or(115_200, |s| s.parse().expect("baud"));

    let image = std::fs::read(&image_path).expect("read image");
    let backend = WirelineBackend::open(&port, baud, image, Duration::from_secs(5))
        .expect("wireline target (is the serve firmware running?)");
    let mut debugger = lamella_dap::Debugger::with_backend(Box::new(backend));
    let stdout = std::io::stdout();
    let reader = std::io::BufReader::new(std::io::stdin());
    lamella_dap::serve_polled(&mut debugger, reader, &mut stdout.lock())
}
