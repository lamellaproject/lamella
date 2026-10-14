//! The Lamella Link SOURCE-LEVEL debug session tools. Each MCP `debug_*` tool drives a `lamella_dap::Debugger`
//! (wrapping a `WireHostBackend`) as a DAP CLIENT -- it synthesizes DAP requests and parses the responses/
//! events -- so all the adapter's source-stepping, breakpoint binding, and stop-reason logic is REUSED, not
//! reimplemented (the same code the VS Code wire debugger runs). A launched program is compiled with a PDB
//! against the corlib and the libraries the call names, baked by the bake every device tool uses -- with the
//! corlib and those libraries, for the board that answered -- and paired with a source map (method_id ->
//! source line) built from the module the image is written from, so the ids the wire reports are the image's.

use lamella_dap::protocol::{Message, Request};
use lamella_dap::Debugger;
use lamella_metadata::{Assembly, PortablePdb};
use lamella_token::Token;
use lamella_wire_host::boards::ReferenceSet;
use lamella_wire_host::debug_backend::WireHostBackend;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::text_result;

/// The `MethodDef` metadata table tag (matches lamella-dap + the wasm srcmap builder).
const METHOD_DEF: u8 = 0x06;
/// The compile source path -> the PDB document name -> the breakpoint source basename.
const DOCUMENT: &str = "input.cs";

/// What a launch sends: the image, its source map, and what to say about what it carries.
struct Built {
    image: Vec<u8>,
    srcmap: Vec<u8>,
    carried: String,
}

/// Compile a submission WITH a Portable PDB against the corlib and the libraries `references` names,
/// bake it for `board` with the bake every device tool uses, and build the matching source map
/// (method_id -> lines).
///
/// **THE MAP IS BUILT FROM THE MODULE THE IMAGE IS WRITTEN FROM**, after the trim: the program is that
/// module's last assembly, behind the corlib and the libraries it uses, and a method's id there is its
/// id in the image, so a frame the board reports names the line it is on.
fn compile_bake_srcmap(
    check: &lamella_wire_host::engine::LcscCompiler,
    board: Option<&str>,
    references: &[String],
    code: &str,
) -> Result<Built, String> {
    let set = ReferenceSet::for_board(check.clone(), board).naming(references)?;
    let refs: Vec<Assembly> = set.compiler().references().iter().filter_map(|bytes| Assembly::read(bytes).ok()).collect();
    let compiled = lamella_assemble::compile_source(code, DOCUMENT, "app", "app", &refs, true);
    let pe = compiled.image.ok_or_else(|| {
        let mut t = String::from("compile failed:");
        for d in compiled.diagnostics.iter().filter(|d| d.is_error()) {
            t.push_str(&format!("\n{}", d.render(DOCUMENT, code)));
        }
        if let Some(e) = &compiled.emit_error {
            t.push_str(&format!("\nemit error: {e:?}"));
        }
        t.push_str(&crate::missing_references(&set, code));
        t
    })?;
    let pdb_bytes = compiled.pdb.ok_or_else(|| "compiler emitted no PDB (needed for source-level debug)".to_owned())?;
    let pdb = PortablePdb::read(&pdb_bytes).map_err(|e| format!("pdb parse: {e:?}"))?;
    let app_asm = Assembly::read(&pe).map_err(|e| format!("PE parse: {e:?}"))?;
    let mut names: HashMap<u32, String> = HashMap::new();
    for type_def in app_asm.type_defs() {
        let type_name = type_def.name();
        for method in type_def.methods() {
            let leaf = method.name().unwrap_or_default();
            let qualified = match &type_name {
                Some(n) if !n.namespace.is_empty() => format!("{}.{}.{}", n.namespace, n.name, leaf),
                Some(n) => format!("{}.{}", n.name, leaf),
                None => leaf.to_string(),
            };
            names.insert(method.rid(), qualified);
        }
    }
    let mut srcmap: Result<Vec<u8>, String> = Err("the bake built no module".to_owned());
    let baked = lamella_wire_host::bake::bake_for_inspecting(&set, pe.clone(), crate::NOT_IN_THE_CLASS_LIBRARY, |module, app| {
        srcmap = source_map(module, app, &pdb, &names);
    })
    .map_err(lamella_wire_host::bake::Refused::into_text)?;
    Ok(Built { srcmap: srcmap?, carried: crate::carried_text(&set, &baked), image: baked.image })
}

/// The source map of the program at assembly id `app` in `module`: for each of its methods with a
/// sequence point, the image's id for it, its document, its name, its points and its locals.
fn source_map(
    module: &lamella_cil_runtime::Module,
    app: u8,
    pdb: &PortablePdb,
    names: &HashMap<u32, String>,
) -> Result<Vec<u8>, String> {
    let mut methods = serde_json::Map::new();
    for rid in 1..=pdb.method_count() {
        let Some(method_id) = module.resolve(app, Token::new(METHOD_DEF, rid)) else {
            continue;
        };
        let points: Vec<Value> = pdb
            .sequence_points(rid)
            .into_iter()
            .filter(|p| !p.is_hidden)
            .map(|p| json!({ "o": p.il_offset, "l": p.start_line, "c": p.start_column }))
            .collect();
        if points.is_empty() {
            continue;
        }
        let document = pdb.method_document(rid).unwrap_or_default();
        let name = names.get(&rid).cloned().unwrap_or_default();
        let locals: Vec<Value> = pdb
            .local_variables(rid)
            .into_iter()
            .map(|lv| json!({ "index": lv.index, "name": lv.name }))
            .collect();
        methods.insert(method_id.to_string(), json!({ "document": document, "name": name, "points": points, "locals": locals }));
    }
    let entry_point = pdb
        .entry_point()
        .and_then(|t| module.resolve(app, t))
        .map_or(Value::Null, Value::from);
    serde_json::to_vec(&json!({ "methods": methods, "entryPoint": entry_point, "error": Value::Null }))
        .map_err(|e| format!("srcmap encode: {e}"))
}

/// A live debug session: the DAP debugger over a wire backend, and its request seq.
struct Session {
    debugger: Debugger,
    seq: i64,
}

/// Send one DAP request and collect the resulting messages (response + events).
fn handle(dbg: &mut Debugger, seq: &mut i64, command: &str, arguments: Option<Value>) -> Vec<Message> {
    let req = Request { seq: *seq, command: command.to_owned(), arguments };
    *seq += 1;
    dbg.handle(&req)
}

fn find_event<'a>(msgs: &'a [Message], event: &str) -> Option<&'a Value> {
    msgs.iter().find_map(|m| match m {
        Message::Event(e) if e.event == event => Some(e.body.as_ref().unwrap_or(&Value::Null)),
        _ => None,
    })
}

fn response_ok(msgs: &[Message], command: &str) -> bool {
    msgs.iter().any(|m| matches!(m, Message::Response(r) if r.command == command && r.success))
}

/// The top stack frame as "Type.Method (input.cs:line)", by asking the debugger for its stack.
fn top_frame(session: &mut Session) -> String {
    let st = handle(&mut session.debugger, &mut session.seq, "stackTrace", Some(json!({ "threadId": 1 })));
    let frame = st.iter().find_map(|m| match m {
        Message::Response(r) if r.command == "stackTrace" => r
            .body
            .as_ref()
            .and_then(|b| b.get("stackFrames"))
            .and_then(Value::as_array)
            .and_then(|f| f.first()),
        _ => None,
    });
    match frame {
        Some(f) => {
            let name = f.get("name").and_then(Value::as_str).unwrap_or("?");
            let line = f.get("line").and_then(Value::as_i64).unwrap_or(0);
            let src = f
                .get("source")
                .and_then(|s| s.get("name").or_else(|| s.get("path")))
                .and_then(Value::as_str)
                .unwrap_or(DOCUMENT);
            format!("{name} ({src}:{line})")
        }
        None => "(no frame)".to_owned(),
    }
}

/// Summarize what a resume/step produced: an exit, a fault, or a stop (with reason + location).
fn stop_summary(session: &mut Session, msgs: &[Message]) -> String {
    if let Some(body) = find_event(msgs, "exited") {
        let code = body.get("exitCode").and_then(Value::as_i64).unwrap_or(0);
        return format!("program exited (code {code}).");
    }
    if find_event(msgs, "stopped").is_none() {
        if find_event(msgs, "terminated").is_some() {
            return "program terminated.".to_owned();
        }
        return "running (no stop reported within the timeout).".to_owned();
    }
    let body = find_event(msgs, "stopped").unwrap_or(&Value::Null);
    let reason = body.get("reason").and_then(Value::as_str).unwrap_or("stopped");
    let extra = body
        .get("description")
        .or_else(|| body.get("text"))
        .and_then(Value::as_str)
        .map(|d| format!(" -- {d}"))
        .unwrap_or_default();
    format!("stopped ({reason}) at {}{extra}", top_frame(session))
}

/// Resume the target (`continue`/`stepIn`/`next`/`stepOut`) and WAIT for the resulting stop. A Lamella Link stop is
/// ASYNCHRONOUS -- the resume request only tells the device to run; the breakpoint/step/exit arrives later as an
/// event surfaced by `Debugger::poll`. So if the immediate response carried no terminal event, poll until one
/// does (or a timeout). Without this, a `continue` returns "still running" and the stop leaks into the next call.
fn resume_and_wait(session: &mut Session, command: &str, arguments: Option<Value>) -> String {
    let terminal = |msgs: &[Message]| {
        find_event(msgs, "stopped").is_some()
            || find_event(msgs, "exited").is_some()
            || find_event(msgs, "terminated").is_some()
    };
    let msgs = handle(&mut session.debugger, &mut session.seq, command, arguments);
    if terminal(&msgs) {
        return stop_summary(session, &msgs);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let polled = session.debugger.poll();
        if terminal(&polled) {
            return stop_summary(session, &polled);
        }
        if Instant::now() >= deadline {
            return "still running after 20s (no breakpoint hit / no exit).".to_owned();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The debug-session registry, held by the server.
pub struct Sessions {
    map: HashMap<String, Session>,
    next: u64,
}

impl Sessions {
    pub fn new() -> Self {
        Self { map: HashMap::new(), next: 1 }
    }

    fn get(&mut self, id: &str) -> Result<&mut Session, Value> {
        self.map
            .get_mut(id)
            .ok_or_else(|| text_result(format!("no such debug session '{id}' (launch one first)."), true))
    }

    /// `lamella_debug_launch`: compile once for no board, so a program that does not compile never
    /// touches the board; HELLO it; check the scope; build for the board that answered; deploy, attach
    /// and stop at entry.
    pub fn launch(
        &mut self,
        scope: &crate::DeviceScope,
        check: &lamella_wire_host::engine::LcscCompiler,
        target: &str,
        code: &str,
        references: &[String],
    ) -> Value {
        if let Err(why) = ReferenceSet::for_board(check.clone(), None)
            .naming(references)
            .and_then(|set| crate::compile_against(&set, code))
        {
            return text_result(why, true);
        }
        let backend = match WireHostBackend::open_target(target, crate::BAUD, Vec::new(), Duration::from_secs(20)) {
            Ok(b) => b,
            Err(e) => return text_result(format!("cannot open {target} for debug: {e:?}"), true),
        };
        if let Err(why) = scope.permits_link_board(backend.identity().product_model) {
            return text_result(why, true);
        }
        let board = lamella_wire_host::boards::board_answering(backend.identity().product_model);
        let built = match compile_bake_srcmap(check, board.as_deref(), references, code) {
            Ok(built) => built,
            Err(e) => return text_result(e, true),
        };
        let backend = backend.with_image(built.image).with_srcmap(Some(built.srcmap));
        let mut session = Session { debugger: Debugger::with_backend(Box::new(backend)), seq: 1 };
        let _ = handle(&mut session.debugger, &mut session.seq, "initialize", None);
        let launched = handle(&mut session.debugger, &mut session.seq, "launch", Some(json!({ "stopOnEntry": true })));
        if !response_ok(&launched, "launch") {
            return text_result(format!("debug launch failed on {target} (deploy/attach did not succeed)."), true);
        }
        let done = handle(&mut session.debugger, &mut session.seq, "configurationDone", None);
        let summary = stop_summary(&mut session, &done);
        let id = format!("dbg{}", self.next);
        self.next += 1;
        self.map.insert(id.clone(), session);
        text_result(
            format!(
                "debug session '{id}' launched on {target}.\n{summary}\nSet breakpoints, then continue/step. Disconnect when done.\n\
                 Built for {}.\n{}",
                crate::built_for(board.as_deref()),
                built.carried
            ),
            false,
        )
    }

    pub fn set_breakpoints(&mut self, id: &str, lines: &[i64], source: &str) -> Value {
        let session = match self.get(id) {
            Ok(s) => s,
            Err(v) => return v,
        };
        let doc = if source.is_empty() { DOCUMENT } else { source };
        let bps: Vec<Value> = lines.iter().map(|l| json!({ "line": l })).collect();
        let args = json!({ "source": { "path": doc, "name": doc }, "breakpoints": bps });
        let msgs = handle(&mut session.debugger, &mut session.seq, "setBreakpoints", Some(args));
        let bound: Vec<String> = msgs
            .iter()
            .find_map(|m| match m {
                Message::Response(r) if r.command == "setBreakpoints" => {
                    r.body.as_ref().and_then(|b| b.get("breakpoints")).and_then(Value::as_array).cloned()
                }
                _ => None,
            })
            .unwrap_or_default()
            .iter()
            .map(|b| {
                let line = b.get("line").and_then(Value::as_i64).unwrap_or(0);
                let ok = b.get("verified").and_then(Value::as_bool).unwrap_or(false);
                format!("line {line}: {}", if ok { "bound" } else { "UNBOUND (no code there)" })
            })
            .collect();
        let body = if bound.is_empty() { "(no breakpoints)".to_owned() } else { bound.join("\n") };
        text_result(format!("breakpoints on {doc}:\n{body}"), false)
    }

    pub fn cont(&mut self, id: &str) -> Value {
        let session = match self.get(id) {
            Ok(s) => s,
            Err(v) => return v,
        };
        text_result(resume_and_wait(session, "continue", None), false)
    }

    pub fn step(&mut self, id: &str, kind: &str) -> Value {
        let session = match self.get(id) {
            Ok(s) => s,
            Err(v) => return v,
        };
        let command = match kind {
            "in" => "stepIn",
            "out" => "stepOut",
            _ => "next",
        };
        text_result(resume_and_wait(session, command, Some(json!({ "threadId": 1 }))), false)
    }

    pub fn stack(&mut self, id: &str) -> Value {
        let session = match self.get(id) {
            Ok(s) => s,
            Err(v) => return v,
        };
        let st = handle(&mut session.debugger, &mut session.seq, "stackTrace", Some(json!({ "threadId": 1 })));
        let frames = st.iter().find_map(|m| match m {
            Message::Response(r) if r.command == "stackTrace" => {
                r.body.as_ref().and_then(|b| b.get("stackFrames")).and_then(Value::as_array).cloned()
            }
            _ => None,
        });
        let text = match frames {
            Some(frames) if !frames.is_empty() => frames
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let name = f.get("name").and_then(Value::as_str).unwrap_or("?");
                    let line = f.get("line").and_then(Value::as_i64).unwrap_or(0);
                    let src = f.get("source").and_then(|s| s.get("name")).and_then(Value::as_str).unwrap_or(DOCUMENT);
                    format!("#{i}  {name} ({src}:{line})")
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => "(no frames -- the session may not be stopped)".to_owned(),
        };
        text_result(text, false)
    }

    pub fn locals(&mut self, id: &str, frame: i64) -> Value {
        let session = match self.get(id) {
            Ok(s) => s,
            Err(v) => return v,
        };
        let scopes = handle(&mut session.debugger, &mut session.seq, "scopes", Some(json!({ "frameId": frame })));
        let var_ref = scopes.iter().find_map(|m| match m {
            Message::Response(r) if r.command == "scopes" => r
                .body
                .as_ref()
                .and_then(|b| b.get("scopes"))
                .and_then(Value::as_array)
                .and_then(|s| s.first())
                .and_then(|s| s.get("variablesReference"))
                .and_then(Value::as_i64),
            _ => None,
        });
        let vars = var_ref
            .map(|r| handle(&mut session.debugger, &mut session.seq, "variables", Some(json!({ "variablesReference": r }))))
            .and_then(|msgs| {
                msgs.iter().find_map(|m| match m {
                    Message::Response(r) if r.command == "variables" => {
                        r.body.as_ref().and_then(|b| b.get("variables")).and_then(Value::as_array).cloned()
                    }
                    _ => None,
                })
            })
            .unwrap_or_default();
        if vars.is_empty() {
            return text_result(
                "no locals available on this target.".to_owned(),
                false,
            );
        }
        let text = vars
            .iter()
            .map(|v| {
                let n = v.get("name").and_then(Value::as_str).unwrap_or("?");
                let val = v.get("value").and_then(Value::as_str).unwrap_or("?");
                format!("{n} = {val}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        text_result(text, false)
    }

    /// `lamella_debug_eval`: read one variable of the paused frame `frame` by name -- a local or an
    /// argument -- as a debugger's Watch row reads it.
    ///
    /// **A NAME, READ FROM THE FRAME, AND NOTHING RUN ON THE BOARD.** The adapter answers a
    /// frame-scoped `evaluate` from the frame's own variables, and refuses anything longer than one
    /// name rather than evaluate it somewhere that cannot see the frame; that refusal is the answer
    /// here too.
    pub fn eval(&mut self, id: &str, expression: &str, frame: i64) -> Value {
        let session = match self.get(id) {
            Ok(s) => s,
            Err(v) => return v,
        };
        let arguments = json!({ "expression": expression, "frameId": frame, "context": "repl" });
        let answered = handle(&mut session.debugger, &mut session.seq, "evaluate", Some(arguments));
        let response = answered.iter().find_map(|m| match m {
            Message::Response(r) if r.command == "evaluate" => Some(r),
            _ => None,
        });
        match response {
            Some(r) if r.success => {
                let body = r.body.as_ref();
                let value = body.and_then(|b| b.get("result")).and_then(Value::as_str).unwrap_or("?");
                let kind = body.and_then(|b| b.get("type")).and_then(Value::as_str).unwrap_or_default();
                let name = expression.trim();
                let text = if kind.is_empty() { format!("{name} = {value}") } else { format!("{name} = {value} ({kind})") };
                text_result(text, false)
            }
            Some(r) => text_result(r.message.clone().unwrap_or_else(|| "the debugger refused it.".to_owned()), true),
            None => text_result("the debugger gave no answer to the evaluation.".to_owned(), true),
        }
    }

    pub fn disconnect(&mut self, id: &str) -> Value {
        let Some(mut session) = self.map.remove(id) else {
            return text_result(format!("no such debug session '{id}'."), true);
        };
        let _ = handle(&mut session.debugger, &mut session.seq, "disconnect", None);
        text_result(format!("debug session '{id}' disconnected."), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_wire_host::engine::LcscCompiler;

    /// A program that opens a pin, then prints from a method of its own.
    const OPENS_A_PIN: &str = "using System;\nusing System.Device.Gpio;\n\npublic static class Program\n{\n    \
        public static void Main()\n    {\n        GpioController controller = new GpioController();\n        \
        controller.OpenPin(15, PinMode.Output);\n        Say(\"gpio opened\");\n    }\n\n    \
        static void Say(string what)\n    {\n        Console.WriteLine(what);\n    }\n}\n";

    /// **THE SOURCE MAP NAMES THE METHODS THE BOARD RUNS.** The image carries the corlib and
    /// `System.Device.Gpio` before the program, so the program's methods are not where a program
    /// baked alone put them. Every method the map names is one of the program's in the image, with
    /// code, and the map's entry point is the image's.
    #[test]
    fn the_source_map_names_the_programs_methods_as_the_image_numbers_them() {
        let Ok(check) = LcscCompiler::discover() else {
            return;
        };
        let built = compile_bake_srcmap(&check, Some("rpi-pico2-w"), &["System.Device.Gpio".to_owned()], OPENS_A_PIN)
            .unwrap_or_else(|refusal| panic!("it builds: {refusal}"));
        assert!(built.carried.contains("System.Device.Gpio: referenced, the rp2350 chip family's build"), "{}", built.carried);

        let image: &'static [u8] = Box::leak(built.image.into_boxed_slice());
        let (module, entry) = lamella_cil_runtime::Module::from_baked(image).expect("it boots");
        let entry = entry.expect("it records its entry point");
        let program = module.method_asm(entry);
        assert!(program > 0, "the program comes after the corlib");

        let map: Value = serde_json::from_slice(&built.srcmap).expect("the map is JSON");
        assert_eq!(map["entryPoint"], json!(entry), "the map's entry point is the image's: {map}");
        let methods = map["methods"].as_object().expect("the map names methods");
        let named: Vec<&str> = methods.values().filter_map(|method| method["name"].as_str()).collect();
        assert!(named.contains(&"Program.Main") && named.contains(&"Program.Say"), "{named:?}");
        for (id, method) in methods {
            let id: lamella_cil_runtime::MethodId = id.parse().expect("an id");
            assert_eq!(module.method_asm(id), program, "{} is the program's", method["name"]);
            assert!(module.method_code_bytes(id).is_some_and(|code| !code.is_empty()), "{} has code", method["name"]);
        }
    }

    /// A program using a library it does not name is refused before a board is opened, naming the
    /// reference to add.
    #[test]
    fn a_launch_names_a_reference_the_program_is_missing() {
        let Ok(check) = LcscCompiler::discover() else {
            return;
        };
        let refusal = compile_bake_srcmap(&check, Some("rpi-pico2-w"), &[], OPENS_A_PIN).err().expect("no reference");
        assert!(refusal.starts_with("compile failed:\n"), "{refusal}");
        assert!(refusal.contains("add \"System.Device.Gpio\" to `references`"), "{refusal}");
    }
}
