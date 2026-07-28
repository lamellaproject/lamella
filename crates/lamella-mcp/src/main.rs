//! The Lamella MCP server (canonical, native): exposes the Lamella toolchain -- compile / run / bake / size /
//! enumerate / deploy / Lamella Link SOURCE-LEVEL debug -- as MCP tools over stdio (JSON-RPC 2.0, newline-delimited),
//! linking the crates directly (no wasm round-trip; always the latest toolchain; one self-contained binary).
//! Hand-rolled on `serde_json` -- no tokio, no MCP SDK -- matching the workspace's dependency-minimal ethos.

use lamella_wire::{Capabilities, Negotiated, TransportError};
use lamella_wire_host::debug_backend::WireTransport;
use lamella_wire_host::engine::{CompileFailure, LcscCompiler, LoopbackLink, Outcome, Repl, ReplCompiler};
#[cfg(feature = "bake")]
use lamella_wire_host::engine::BakedSerialLink;
use lamella_wire_host::{deployed_status_blocking, hello_blocking, list_serial, SerialTransport, UsbTransport};
#[cfg(feature = "bake")]
use lamella_wire_host::{deploy_chunked_blocking, send_deploy_run};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::time::Duration;

/// The default serial baud for a Lamella Link carrier (USB-CDC ignores it; a real UART wants it).
const BAUD: u32 = 115_200;

#[cfg(feature = "bake")]
mod debug;

/// The shared, canonical tool contract (name / description / inputSchema / annotations). Embedded so the binary
/// is self-contained; the browser host vendors a byte-identical copy. `tools/list` serves the `tools` array.
const CONTRACT: &str = include_str!("../tools.json");

/// The MCP RESOURCES: the chip registry, the C# surface and the samples, as stored text. The board
/// registry is the exception -- its body is computed by [`boards_resource_text`] from the wire's own
/// board table, so it cannot fall behind the boards this build knows.
const RESOURCES: &str = include_str!("../resources.json");

/// The managed corlib bytes the in-process host runner (`LoopbackLink`) executes against: `LAMELLA_CORLIB` if
/// set, else the committed dev fixture (mirrors `LcscCompiler::discover`).
fn corlib_bytes() -> Result<Vec<u8>, String> {
    if let Some(path) = std::env::var_os("LAMELLA_CORLIB") {
        return std::fs::read(&path).map_err(|error| format!("LAMELLA_CORLIB: {error}"));
    }
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../lamella-load/tests/fixtures/corlib.dll");
    std::fs::read(&fixture).map_err(|error| format!("corlib fixture {}: {error}", fixture.display()))
}

/// The tool-list payload: the `tools` array from the embedded contract, verbatim.
fn tools() -> Value {
    serde_json::from_str::<Value>(CONTRACT)
        .ok()
        .and_then(|c| c.get("tools").cloned())
        .unwrap_or_else(|| json!([]))
}

/// The resource metadata for `resources/list` (uri/name/description/mimeType; the text is served by read).
fn resources_list() -> Value {
    let items = serde_json::from_str::<Value>(RESOURCES)
        .ok()
        .and_then(|r| r.get("resources").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let meta: Vec<Value> = items
        .iter()
        .map(|r| json!({ "uri": r.get("uri"), "name": r.get("name"), "description": r.get("description"), "mimeType": r.get("mimeType") }))
        .collect();
    json!({ "resources": meta })
}

/// The `lamella://boards` body, COMPUTED from [`lamella_wire::board_model`] rather than stored.
///
/// That module is the one canonical value -> name map, so enumerating it here is the difference
/// between a list that is right and a list that was right when someone last copied it. It also
/// scales: a board is one row, and the boards a client actually cares about are the ones it can
/// see, which `lamella_enumerate_devices` and `lamella_identify_device` report live.
///
/// The models are contiguous from 0, so the scan stops at the first unrecognized value -- the same
/// idiom `lamella-wire`'s `board-models-json` example uses to emit the JS registry.
fn boards_resource_text() -> String {
    let mut out = String::from(
        "# Supported dev boards\n\nThe `board_model` wire values a board reports over Lamella Link:\n\n\
         | model | board |\n|--:|---|\n",
    );
    let mut model: u16 = 0;
    while let Some(name) = lamella_wire::board_model::name(model) {
        out.push_str(&format!("| {model} | {name} |\n"));
        model += 1;
    }
    out.push_str(
        "\nRun `lamella_enumerate_devices` to see the boards attached to this machine, and \
         `lamella_identify_device` on one of them to read its `board_model` and chip IDCODE live.\n",
    );
    out
}

/// The contents for `resources/read`, or `None` if the uri is unknown.
fn read_resource(uri: &str) -> Option<Value> {
    let parsed = serde_json::from_str::<Value>(RESOURCES).ok()?;
    let items = parsed.get("resources").and_then(Value::as_array)?;
    let r = items.iter().find(|r| r.get("uri").and_then(Value::as_str) == Some(uri))?;
    let text = if uri == "lamella://boards" {
        json!(boards_resource_text())
    } else {
        r.get("text").cloned().unwrap_or_else(|| json!(""))
    };
    Some(json!({ "contents": [{ "uri": r.get("uri"), "mimeType": r.get("mimeType"), "text": text }] }))
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// Parse a `usb` / `usb:<serial>` / `usb:<vid>:<pid>[:<serial>]` target into (vid, pid, serial).
fn parse_usb(target: &str) -> (u16, u16, Option<String>) {
    let rest = target.strip_prefix("usb").unwrap_or("").trim_start_matches(':');
    if rest.is_empty() {
        return (0, 0, None);
    }
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() >= 2 {
        if let (Ok(vid), Ok(pid)) = (u16::from_str_radix(parts[0], 16), u16::from_str_radix(parts[1], 16)) {
            return (vid, pid, parts.get(2).map(|s| (*s).to_owned()));
        }
    }
    (0, 0, Some(rest.to_owned()))
}

/// Open the carrier named by `target`: `usb[:...]` -> native-USB, anything else -> a serial port. The
/// `WireTransport` enum abstracts either so every device tool shares one open path.
fn open_transport(target: &str) -> Result<WireTransport, TransportError> {
    if target == "usb" || target.starts_with("usb:") {
        let (vid, pid, serial) = parse_usb(target);
        Ok(WireTransport::Usb(UsbTransport::open_matching(vid, pid, serial.as_deref())?))
    } else {
        Ok(WireTransport::Serial(SerialTransport::open(target, BAUD)?))
    }
}

/// A generous host capability set to advertise on HELLO -- offering `PROFILE_CHIPID` so a target that fills its
/// chip identity sends it. The negotiated set is this intersected with the target's, so it reflects the board.
fn host_caps() -> Capabilities {
    Capabilities(
        Capabilities::PROFILE_CHIPID
            | Capabilities::DEBUG_BASIC
            | Capabilities::BREAKPOINTS
            | Capabilities::STEPPING
            | Capabilities::REPL_RUN
            | Capabilities::BAKED_IMAGE
            | Capabilities::DEBUG_ATTACH,
    )
}

/// A display name for a Lamella Link `board_model`. DERIVES from [`lamella_wire::board_model::name`] -- the ONE
/// canonical value -> name map -- so it cannot drift from the registry. (Hand-mirroring it drifted twice: "SAM E54"
/// for canonical "SAME54", and four boards missing entirely.) `None` means the registry does not know the code;
/// UNKNOWN (0) names ITSELF "custom board", which is a recognized answer rather than an unrecognized one.
fn board_model_name(model: u16) -> Option<&'static str> {
    lamella_wire::board_model::name(model)
}

/// Enumerate attached boards (native-USB Lamella Link devices + OS serial ports), cross-platform.
fn tool_list_devices() -> Value {
    let mut items: Vec<Value> = Vec::new();
    if let Ok(boards) = UsbTransport::list() {
        for b in boards {
            let target = match &b.serial_number {
                Some(s) => format!("usb:{:04x}:{:04x}:{s}", b.vendor_id, b.product_id),
                None => format!("usb:{:04x}:{:04x}", b.vendor_id, b.product_id),
            };
            items.push(json!({ "carrier": "usb", "target": target, "vid": b.vendor_id, "pid": b.product_id, "serial": b.serial_number, "product": b.product }));
        }
    }
    for p in list_serial() {
        items.push(json!({ "carrier": "serial", "target": p.port, "vid": p.vid, "pid": p.pid, "serial": p.serial_number, "product": p.product }));
    }
    let text = if items.is_empty() {
        "(no boards found)".to_owned()
    } else {
        serde_json::to_string_pretty(&json!(items)).unwrap_or_default()
    };
    text_result(text, false)
}

/// Render the identity a HELLO negotiated: Lamella Link version, capabilities, and (when reported) the board name,
/// profile name/abi, and chip IDCODE + device id.
fn format_identity(target: &str, neg: &Negotiated) -> String {
    let mut s = format!("target: {target}\nLamella Link version: {}\ncapabilities: {:#010x}\n", neg.version, neg.caps.0);
    match &neg.profile {
        Some(p) => {
            let board = board_model_name(p.board_model).unwrap_or("(unrecognized board_model)");
            s.push_str(&format!("board: {board} (board_model {})\n", p.board_model));
            s.push_str(&format!("profile: {} (abi {})\n", p.name(), p.abi));
            if p.chip_idcode != 0 {
                s.push_str(&format!("chip IDCODE: {:#010x}\n", p.chip_idcode));
                if p.chip_devid != 0 {
                    s.push_str(&format!("chip devid: {:#010x}\n", p.chip_devid));
                }
            } else {
                s.push_str("chip IDCODE: (not reported by this firmware yet)\n");
            }
        }
        None => s.push_str("profile: (target reported no ProfileIdentity)\n"),
    }
    s
}

/// HELLO a target and report what it is -- no deploy, no run.
fn tool_identify_device(target: &str) -> Value {
    if target.is_empty() {
        return text_result("identify: a 'target' (COM port or usb[:...]) is required.".to_owned(), true);
    }
    let mut transport = match open_transport(target) {
        Ok(t) => t,
        Err(e) => return text_result(format!("cannot open {target}: {e:?}"), true),
    };
    match hello_blocking(&mut transport, 1, host_caps(), Duration::from_secs(3)) {
        Ok(neg) => text_result(format_identity(target, &neg), false),
        Err(e) => text_result(format!("no HELLO_ACK from {target} (is serve firmware running?): {e:?}"), true),
    }
}

/// Compile a C# submission and BAKE its PE into a `.lmli` flash image (single-assembly: BCL references resolve
/// to the interpreter's intrinsics, the same shape `BakedSerialLink` deploys). One leaked buffer per bake --
/// the accepted host `Assembly<'static>` pattern. `write_baked` needs `code-in-place`, hence `bake`-gated.
#[cfg(feature = "bake")]
fn compile_and_bake(compiler: &LcscCompiler, code: &str) -> Result<Vec<u8>, String> {
    let pe = compiler.compile(code).map_err(|e| match e {
        CompileFailure::Diagnostics(d) => format!("compile failed:\n{d}"),
        CompileFailure::Toolchain(t) => format!("toolchain error: {t}"),
    })?;
    let program: &'static [u8] = Box::leak(pe.into_boxed_slice());
    let assembly = lamella_metadata::Assembly::read(program).map_err(|e| format!("PE does not parse: {e:?}"))?;
    let loaded = lamella_load::load(&assembly).map_err(|e| format!("load: {e}"))?;
    let mut module = loaded.module;
    module.write_baked(Some(loaded.entry)).map_err(|e| format!("bake: {e:?}"))
}

/// Compile a submission and run it ON the device over Lamella Link (BakedSerialLink: bake host-side, the image
/// crosses the wire, the device interprets it and returns output). Serial targets only.
#[cfg(feature = "bake")]
fn tool_run_on_device(target: &str, code: &str) -> Value {
    if target.starts_with("usb") {
        return text_result("run_on_device supports serial targets today (BakedSerialLink); usb Lamella Link run is a follow-up.".to_owned(), true);
    }
    let compiler = match LcscCompiler::discover() {
        Ok(c) => c,
        Err(e) => return text_result(format!("compiler: {e}"), true),
    };
    let link = match BakedSerialLink::open(target, BAUD, Duration::from_secs(15)) {
        Ok(l) => l,
        Err(e) => return text_result(format!("cannot open {target} as a baked-image target (needs serve + BAKED_IMAGE): {e:?}"), true),
    };
    let mut repl = Repl::new(Box::new(compiler), Box::new(link));
    match repl.eval_program(code) {
        Ok(Outcome::Ran { output, exit, .. }) => {
            let body = if output.is_empty() { "(empty)".to_owned() } else { output };
            text_result(format!("[ran on {target}]\nexit code: {exit}\nstdout:\n{body}"), false)
        }
        Ok(Outcome::CompileError(t)) => text_result(format!("Compile failed:\n{t}"), true),
        Ok(Outcome::Empty) => text_result("(empty submission)".to_owned(), false),
        Err(e) => text_result(format!("device error: {e:?}"), true),
    }
}

/// Query the resident deployed image on a target (read-only): HELLO to enter serve mode, then DEPLOY_STATUS.
fn tool_deploy_status(target: &str) -> Value {
    let mut t = match open_transport(target) {
        Ok(t) => t,
        Err(e) => return text_result(format!("cannot open {target}: {e:?}"), true),
    };
    let timeout = Duration::from_secs(5);
    if let Err(e) = hello_blocking(&mut t, 0, host_caps(), timeout) {
        return text_result(format!("no HELLO_ACK from {target}: {e:?}"), true);
    }
    match deployed_status_blocking(&mut t, 1, timeout) {
        Ok(Some(sum)) => text_result(format!("{target} holds a resident image, checksum {sum:#018x}"), false),
        Ok(None) => text_result(format!("{target} has no resident image."), false),
        Err(e) => text_result(format!("status query on {target} failed: {e:?}"), true),
    }
}

/// Server state: the host compile/run engine (+ a check-only compiler), and (added with the debug tools) the
/// live debug sessions. `allow_device` gates every tool that touches a board.
struct Server {
    check: LcscCompiler,
    repl: Repl,
    allow_device: bool,
    #[cfg(feature = "bake")]
    debug: debug::Sessions,
}

impl Server {
    fn new(allow_device: bool) -> Result<Self, String> {
        let corlib = corlib_bytes()?;
        let check = LcscCompiler::discover().map_err(|e| e.to_string())?;
        let run_compiler = LcscCompiler::discover().map_err(|e| e.to_string())?;
        let repl = Repl::new(Box::new(run_compiler), Box::new(LoopbackLink::new(corlib)));
        Ok(Self {
            check,
            repl,
            allow_device,
            #[cfg(feature = "bake")]
            debug: debug::Sessions::new(),
        })
    }

    fn call_tool(&mut self, name: &str, args: &Value) -> Value {
        let lang = args.get("language").and_then(Value::as_str).unwrap_or("csharp");
        let code = args.get("code").and_then(Value::as_str).unwrap_or_default();
        let target = args.get("target").and_then(Value::as_str).unwrap_or_default();
        #[cfg(feature = "bake")]
        let session = args.get("session").and_then(Value::as_str).unwrap_or_default();
        #[cfg(feature = "bake")]
        let source = args.get("source").and_then(Value::as_str).unwrap_or_default();
        #[cfg(feature = "bake")]
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("over");
        #[cfg(feature = "bake")]
        let expression = args.get("expression").and_then(Value::as_str).unwrap_or_default();
        let device_tool = matches!(
            name,
            "lamella_list_devices"
                | "lamella_identify_device"
                | "lamella_run_on_device"
                | "lamella_deploy"
                | "lamella_deploy_status"
                | "lamella_debug_launch"
                | "lamella_debug_set_breakpoints"
                | "lamella_debug_continue"
                | "lamella_debug_step"
                | "lamella_debug_stack"
                | "lamella_debug_locals"
                | "lamella_debug_eval"
                | "lamella_debug_disconnect"
        );
        if device_tool && !self.allow_device {
            return text_result(
                "This tool touches a hardware board; the server was started without --allow-device.".to_owned(),
                true,
            );
        }
        match name {
            "lamella_check" => self.tool_check(lang, code),
            "lamella_run" => self.tool_run(lang, code),
            "lamella_list_devices" => tool_list_devices(),
            "lamella_identify_device" => tool_identify_device(target),
            "lamella_deploy_status" => tool_deploy_status(target),
            #[cfg(feature = "bake")]
            "lamella_size" => self.tool_size(code),
            #[cfg(feature = "bake")]
            "lamella_bake" => self.tool_bake(code, args.get("out_path").and_then(Value::as_str)),
            #[cfg(feature = "bake")]
            "lamella_run_on_device" => tool_run_on_device(target, code),
            #[cfg(feature = "bake")]
            "lamella_deploy" => self.tool_deploy(target, code, args.get("run").and_then(Value::as_bool).unwrap_or(true)),
            #[cfg(feature = "bake")]
            "lamella_debug_launch" => self.debug.launch(target, code),
            #[cfg(feature = "bake")]
            "lamella_debug_set_breakpoints" => {
                let lines: Vec<i64> = args
                    .get("lines")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_i64).collect())
                    .unwrap_or_default();
                self.debug.set_breakpoints(session, &lines, source)
            }
            #[cfg(feature = "bake")]
            "lamella_debug_continue" => self.debug.cont(session),
            #[cfg(feature = "bake")]
            "lamella_debug_step" => self.debug.step(session, kind),
            #[cfg(feature = "bake")]
            "lamella_debug_stack" => self.debug.stack(session),
            #[cfg(feature = "bake")]
            "lamella_debug_locals" => self.debug.locals(session, args.get("frame").and_then(Value::as_i64).unwrap_or(0)),
            #[cfg(feature = "bake")]
            "lamella_debug_eval" => self.debug.eval(session, expression),
            #[cfg(feature = "bake")]
            "lamella_debug_disconnect" => self.debug.disconnect(session),
            #[cfg(not(feature = "bake"))]
            "lamella_size" | "lamella_bake" | "lamella_run_on_device" | "lamella_deploy"
            | "lamella_debug_launch" | "lamella_debug_set_breakpoints" | "lamella_debug_continue"
            | "lamella_debug_step" | "lamella_debug_stack" | "lamella_debug_locals" | "lamella_debug_eval"
            | "lamella_debug_disconnect" => text_result(
                format!("{name}: this server was built WITHOUT the `bake` feature (compile / run / enumerate / identify only). Rebuild with `cargo build -p lamella-mcp --features bake` for on-device bake / deploy / debug."),
                true,
            ),
            other => text_result(format!("unknown tool: {other}"), true),
        }
    }

    fn tool_check(&self, lang: &str, code: &str) -> Value {
        if lang == "python" {
            return text_result("Python check runs on the browser host; not wired into the native server yet.".to_owned(), false);
        }
        match self.check.compile(code) {
            Ok(_) => text_result("OK -- compiles.".to_owned(), false),
            Err(CompileFailure::Diagnostics(text)) => text_result(text, false),
            Err(CompileFailure::Toolchain(text)) => text_result(format!("toolchain error: {text}"), true),
        }
    }

    fn tool_run(&mut self, lang: &str, code: &str) -> Value {
        if lang == "python" {
            return text_result("Python run is on the browser host; not wired into the native server yet.".to_owned(), false);
        }
        match self.repl.eval_program(code) {
            Ok(Outcome::Ran { output, exit, .. }) => {
                let body = if output.is_empty() { "(empty)".to_owned() } else { output };
                text_result(format!("exit code: {exit}\nstdout:\n{body}"), false)
            }
            Ok(Outcome::CompileError(text)) => text_result(format!("Compile failed:\n{text}"), true),
            Ok(Outcome::Empty) => text_result("(empty submission)".to_owned(), false),
            Err(error) => text_result(format!("error: {error}"), true),
        }
    }

    #[cfg(feature = "bake")]
    fn tool_bake(&self, code: &str, out_path: Option<&str>) -> Value {
        match compile_and_bake(&self.check, code) {
            Ok(image) => {
                let mut msg = format!("baked OK: {} bytes (.lmli flash image).", image.len());
                if let Some(path) = out_path {
                    match std::fs::write(path, &image) {
                        Ok(()) => msg.push_str(&format!("\nWrote {path}.")),
                        Err(e) => msg.push_str(&format!("\n(could not write {path}: {e})")),
                    }
                }
                text_result(msg, false)
            }
            Err(e) => text_result(e, true),
        }
    }

    #[cfg(feature = "bake")]
    fn tool_size(&self, code: &str) -> Value {
        match compile_and_bake(&self.check, code) {
            Ok(image) => text_result(
                format!(
                    "flash (baked .lmli image): {} bytes\n(This is the deployable image size; it is not a RAM or per-section budget.)",
                    image.len()
                ),
                false,
            ),
            Err(e) => text_result(e, true),
        }
    }

    #[cfg(feature = "bake")]
    fn tool_deploy(&self, target: &str, code: &str, run: bool) -> Value {
        let image = match compile_and_bake(&self.check, code) {
            Ok(i) => i,
            Err(e) => return text_result(e, true),
        };
        let mut t = match open_transport(target) {
            Ok(t) => t,
            Err(e) => return text_result(format!("cannot open {target}: {e:?}"), true),
        };
        let timeout = Duration::from_secs(20);
        if let Err(e) = hello_blocking(&mut t, 0, host_caps(), timeout) {
            return text_result(format!("no HELLO_ACK from {target}: {e:?}"), true);
        }
        match deploy_chunked_blocking(&mut t, 1, &image, 8 * 1024, timeout) {
            Ok(true) => {
                let mut msg = format!("deployed {} bytes to {target}.", image.len());
                if run {
                    match send_deploy_run(&mut t, 2) {
                        Ok(()) => msg.push_str(" Booted it (DEPLOY_RUN)."),
                        Err(e) => msg.push_str(&format!(" (deploy ok, DEPLOY_RUN failed: {e:?})")),
                    }
                }
                text_result(msg, false)
            }
            Ok(false) => text_result(format!("deploy to {target} was not fully acked (a chunk failed to verify)."), true),
            Err(e) => text_result(format!("deploy to {target} failed: {e:?}"), true),
        }
    }
}

fn respond(out: &mut impl Write, id: &Value, result: Value) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).unwrap_or_default()
    );
    let _ = out.flush();
}

fn respond_error(out: &mut impl Write, id: &Value, code: i64, message: &str) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
            .unwrap_or_default()
    );
    let _ = out.flush();
}

fn main() {
    let allow_device = std::env::args().any(|a| a == "--allow-device");
    let mut server = Server::new(allow_device).unwrap_or_else(|error| {
        eprintln!("lamella-mcp: {error}");
        std::process::exit(1)
    });

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else { continue };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        match method {
            "initialize" => {
                if let Some(id) = &id {
                    respond(
                        &mut out,
                        id,
                        json!({ "protocolVersion": "2024-11-05", "capabilities": { "tools": {}, "resources": {} }, "serverInfo": { "name": "lamella-mcp", "version": "0.2.0" } }),
                    );
                }
            }
            "notifications/initialized" | "initialized" => {}
            "ping" => {
                if let Some(id) = &id {
                    respond(&mut out, id, json!({}));
                }
            }
            "tools/list" => {
                if let Some(id) = &id {
                    respond(&mut out, id, json!({ "tools": tools() }));
                }
            }
            "tools/call" => {
                if let Some(id) = &id {
                    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    let result = server.call_tool(name, &args);
                    respond(&mut out, id, result);
                }
            }
            "resources/list" => {
                if let Some(id) = &id {
                    respond(&mut out, id, resources_list());
                }
            }
            "resources/read" => {
                if let Some(id) = &id {
                    let uri = msg.get("params").and_then(|p| p.get("uri")).and_then(Value::as_str).unwrap_or_default();
                    match read_resource(uri) {
                        Some(r) => respond(&mut out, id, r),
                        None => respond_error(&mut out, id, -32602, &format!("no such resource: {uri}")),
                    }
                }
            }
            _ => {
                if let Some(id) = &id {
                    respond_error(&mut out, id, -32601, "method not found");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boards resource must be COMPUTED from the wire's board table, not stored beside it.
    /// A stored copy is a second source of truth, and this one had gone five boards stale before
    /// anybody read it. Red-proof: serve `resources.json`'s stored `text` again and every board
    /// past the one it stopped at fails this assertion by name.
    #[test]
    fn the_boards_resource_names_every_board_the_wire_knows() {
        let body = read_resource("lamella://boards").expect("the boards resource exists");
        let text = body["contents"][0]["text"].as_str().expect("it has text").to_owned();

        let mut model: u16 = 0;
        while let Some(name) = lamella_wire::board_model::name(model) {
            assert!(
                text.contains(&format!("| {model} | {name} |")),
                "board_model {model} ({name}) is missing from the boards resource -- \
                 the resource is not deriving from lamella_wire::board_model"
            );
            model += 1;
        }
        assert!(model > 1, "the scan found no boards at all, so it proves nothing");
    }
}
