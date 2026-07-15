//! The Lamella Link DEVICE DAP server: VS Code speaks DAP-JSON over stdio to this binary,
//! which speaks Lamella Link frames over a serial port to the on-device interpreter. The
//! adapter is unchanged -- [`lamella_wire_host::debug_backend::WireHostBackend`] implements
//! the same `DebugBackend` seam the host interpreter backend does, and the polled serve
//! loop surfaces asynchronous stops (a breakpoint the device hits) on their own.

use lamella_wire_host::debug_backend::WireHostBackend;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: lamella-dap-wire <COM-port | usb[:serial]> <image.lmli> [baud]  |  lamella-dap-wire --list";
    let first = args.next().expect(usage);
    if first == "--list" {
        print_boards();
        return Ok(());
    }
    let target = first;
    let image_path = args.next().expect(usage);
    let baud: u32 = args.next().map_or(115_200, |s| s.parse().expect("baud"));

    let image = std::fs::read(&image_path).expect("read image");
    let srcmap = std::fs::read(std::path::Path::new(&image_path).with_extension("srcmap.json")).ok();
    let backend = WireHostBackend::open_target(&target, baud, image, Duration::from_secs(5))
        .expect("Lamella Link target (is the serve firmware running?)")
        .with_srcmap(srcmap);
    let mut debugger = lamella_dap::Debugger::with_backend(Box::new(backend));
    let stdout = std::io::stdout();
    let reader = std::io::BufReader::new(std::io::stdin());
    lamella_dap::serve_polled(&mut debugger, reader, &mut stdout.lock())
}

/// Print the attached boards as a JSON array the extension's board picker consumes -- each entry a
/// `{ carrier, target, vid, pid, serial, product }`, where `target` is exactly what goes in the launch
/// config's `port`. Two carriers: native-USB (driverless) Lamella Link boards, and OS serial ports (a debug
/// probe / FTDI / EDBG UART). Cross-platform; a platform that cannot enumerate one carrier just omits it.
/// Hand-formatted JSON (no serde dep in this bin), with the handful of strings properly escaped.
fn print_boards() {
    let mut items: Vec<String> = Vec::new();

    if let Ok(boards) = lamella_wire_host::UsbTransport::list() {
        for board in boards {
            let target = match &board.serial_number {
                Some(serial) => format!("usb:{:04x}:{:04x}:{serial}", board.vendor_id, board.product_id),
                None => format!("usb:{:04x}:{:04x}", board.vendor_id, board.product_id),
            };
            items.push(format!(
                "{{\"carrier\":\"usb\",\"target\":{},\"vid\":{},\"pid\":{},\"serial\":{},\"product\":{}}}",
                json_str(&target),
                board.vendor_id,
                board.product_id,
                json_opt(board.serial_number.as_deref()),
                json_opt(board.product.as_deref()),
            ));
        }
    }

    for port in lamella_wire_host::list_serial() {
        items.push(format!(
            "{{\"carrier\":\"serial\",\"target\":{},\"vid\":{},\"pid\":{},\"serial\":{},\"product\":{}}}",
            json_str(&port.port),
            port.vid.map_or_else(|| "null".to_string(), |v| v.to_string()),
            port.pid.map_or_else(|| "null".to_string(), |v| v.to_string()),
            json_opt(port.serial_number.as_deref()),
            json_opt(port.product.as_deref()),
        ));
    }

    println!("[{}]", items.join(","));
}

/// A JSON string literal (quoted + escaped).
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A JSON string literal, or the literal `null` for `None`.
fn json_opt(s: Option<&str>) -> String {
    s.map_or_else(|| "null".to_string(), json_str)
}
