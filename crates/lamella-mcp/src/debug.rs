//! The wireline SOURCE-LEVEL debug session tools. Each MCP `debug_*` tool drives a `lamella_dap::Debugger`
//! (wrapping a `WirelineBackend`) as a DAP CLIENT -- it synthesizes DAP requests and parses the responses/
//! events -- so all the adapter's source-stepping, breakpoint binding, and stop-reason logic is REUSED, not
//! reimplemented (the same code the VS Code wire debugger runs). A launched program is compiled with a PDB,
//! baked SINGLE-assembly (the shape `run_on_device` deploys and the device runs), and paired with a matching
//! single-assembly source map (method_id -> source line) built natively from the PDB -- the JSON the wire
//! debugger needs (there is no host-callable srcmap builder on main; this ports the wasm `srcmap_inner`).

use lamella_dap::protocol::{Message, Request};
use lamella_dap::Debugger;
use lamella_metadata::{Assembly, PortablePdb};
use lamella_token::Token;
use lamella_wireline::debug_backend::WirelineBackend;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::text_result;

/// The `MethodDef` metadata table tag (matches lamella-dap + the wasm srcmap builder).
const METHOD_DEF: u8 = 0x06;
/// The compile source path -> the PDB document name -> the breakpoint source basename.
const DOCUMENT: &str = "input.cs";

/// Compile a submission WITH a Portable PDB, bake its PE single-assembly into a `.lmli` image, and build the
/// matching single-assembly source map (method_id -> lines). Returns `(image, srcmap_json)`. Bake + srcmap
/// run off the SAME loaded module so the method_ids the wire reports match the map.
fn compile_bake_srcmap(code: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    let corlib = crate::corlib_bytes()?;
    let corlib_asm = Assembly::read(&corlib).map_err(|e| format!("corlib parse: {e:?}"))?;
    let refs = [corlib_asm];
    let compiled = lamella_assemble::compile_source(code, DOCUMENT, "app", "app", &refs, true);
    let pe = compiled.image.ok_or_else(|| {
        let mut t = String::from("compile failed:\n");
        for d in &compiled.diagnostics {
            if d.is_error() {
                t.push_str(&format!("CS{:04}: {}\n", d.code, d.message));
            }
        }
        if let Some(e) = &compiled.emit_error {
            t.push_str(&format!("emit error: {e:?}"));
        }
        t
    })?;
    let pdb_bytes = compiled.pdb.ok_or_else(|| "compiler emitted no PDB (needed for source-level debug)".to_owned())?;

    let leaked: &'static [u8] = Box::leak(pe.into_boxed_slice());
    let app_asm = Assembly::read(leaked).map_err(|e| format!("PE parse: {e:?}"))?;
    let mut loaded = lamella_load::load(&app_asm).map_err(|e| format!("load: {e}"))?;
    let entry = loaded.entry;

    let pdb = PortablePdb::read(&pdb_bytes).map_err(|e| format!("pdb parse: {e:?}"))?;
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
    let mut methods = serde_json::Map::new();
    for rid in 1..=pdb.method_count() {
        let Some(method_id) = loaded.module.resolve(0, Token::new(METHOD_DEF, rid)) else {
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
        .and_then(|t| loaded.module.resolve(0, t))
        .map_or(Value::Null, Value::from);
    let srcmap = serde_json::to_vec(&json!({ "methods": methods, "entryPoint": entry_point, "error": Value::Null }))
        .map_err(|e| format!("srcmap encode: {e}"))?;

    let image = loaded.module.write_baked(Some(entry)).map_err(|e| format!("bake: {e:?}"))?;
    Ok((image, srcmap))
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

/// Resume the target (`continue`/`stepIn`/`next`/`stepOut`) and WAIT for the resulting stop. A wireline stop is
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

    pub fn launch(&mut self, target: &str, code: &str) -> Value {
        let (image, srcmap) = match compile_bake_srcmap(code) {
            Ok(v) => v,
            Err(e) => return text_result(e, true),
        };
        let backend = match WirelineBackend::open_target(target, crate::BAUD, image, Duration::from_secs(20)) {
            Ok(b) => b.with_srcmap(Some(srcmap)),
            Err(e) => return text_result(format!("cannot open {target} for debug: {e:?}"), true),
        };
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
        text_result(format!("debug session '{id}' launched on {target}.\n{summary}\nSet breakpoints, then continue/step. Disconnect when done."), false)
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
                "no locals available on this target yet -- on-device locals over the wire (DBG_LOCALS) are being wired up.".to_owned(),
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

    pub fn eval(&mut self, id: &str, _expression: &str) -> Value {
        if self.get(id).is_err() {
            return text_result(format!("no such debug session '{id}'."), true);
        }
        text_result("debug_eval (Debug Console on a live wire session) is a follow-up; use run_on_device for a fresh submission.".to_owned(), true)
    }

    pub fn disconnect(&mut self, id: &str) -> Value {
        let Some(mut session) = self.map.remove(id) else {
            return text_result(format!("no such debug session '{id}'."), true);
        };
        let _ = handle(&mut session.debugger, &mut session.seq, "disconnect", None);
        text_result(format!("debug session '{id}' disconnected."), false)
    }
}
