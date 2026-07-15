//! The Lamella MCP server (canonical, native): exposes the Lamella toolchain -- compile + run C# -- as MCP
//! tools over stdio (JSON-RPC 2.0, newline-delimited), linking the crates directly (no wasm round-trip, no JS
//! marshaling; always the latest toolchain; a single self-contained binary). Hand-rolled on `serde_json` --
//! no tokio, no MCP SDK -- matching the workspace's dependency-minimal ethos. The compile+run path is the
//! Lamella Link REPL engine (`LcscCompiler` + `LoopbackLink`), the same one `wire-repl` and the Debug Console use.

use lamella_wire_host::engine::{CompileFailure, LcscCompiler, LoopbackLink, Outcome, Repl, ReplCompiler};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// The managed corlib bytes the in-process runner (LoopbackLink) executes against: `LAMELLA_CORLIB` if set,
/// else the committed dev fixture (mirrors `LcscCompiler::discover`). A future rung embeds it via
/// `include_bytes!` for a fully self-contained binary.
fn corlib_bytes() -> Result<Vec<u8>, String> {
    if let Some(path) = std::env::var_os("LAMELLA_CORLIB") {
        return std::fs::read(&path).map_err(|error| format!("LAMELLA_CORLIB: {error}"));
    }
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../lamella-load/tests/fixtures/corlib.dll");
    std::fs::read(&fixture).map_err(|error| format!("corlib fixture {}: {error}", fixture.display()))
}

fn tools() -> Value {
    json!([
        {
            "name": "lamella_run",
            "description": "Compile and RUN a C# program on the Lamella interpreter; returns stdout + exit code, or compile diagnostics. Write a full program with a Main.",
            "inputSchema": { "type": "object", "properties": { "code": { "type": "string", "description": "The C# program source." } }, "required": ["code"] }
        },
        {
            "name": "lamella_check",
            "description": "Compile-CHECK a C# program WITHOUT running it -- diagnostics only, for fast iteration before you run.",
            "inputSchema": { "type": "object", "properties": { "code": { "type": "string", "description": "The C# program source." } }, "required": ["code"] }
        }
    ])
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn call_tool(repl: &mut Repl, check: &LcscCompiler, name: &str, args: &Value) -> Value {
    let code = args.get("code").and_then(Value::as_str).unwrap_or_default();
    match name {
        "lamella_run" => match repl.eval_program(code) {
            Ok(Outcome::Ran { output, exit, .. }) => {
                let body = if output.is_empty() { "(empty)".to_owned() } else { output };
                text_result(format!("exit code: {exit}\nstdout:\n{body}"), false)
            }
            Ok(Outcome::CompileError(text)) => text_result(format!("Compile failed:\n{text}"), true),
            Ok(Outcome::Empty) => text_result("(empty submission)".to_owned(), false),
            Err(error) => text_result(format!("error: {error}"), true),
        },
        "lamella_check" => match check.compile(code) {
            Ok(_) => text_result("OK -- compiles.".to_owned(), false),
            Err(CompileFailure::Diagnostics(text)) => text_result(text, false),
            Err(CompileFailure::Toolchain(text)) => text_result(format!("toolchain error: {text}"), true),
        },
        other => text_result(format!("unknown tool: {other}"), true),
    }
}

fn respond(out: &mut impl Write, id: &Value, result: Value) {
    let _ = writeln!(out, "{}", serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).unwrap_or_default());
    let _ = out.flush();
}

fn respond_error(out: &mut impl Write, id: &Value, code: i64, message: &str) {
    let _ = writeln!(out, "{}", serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })).unwrap_or_default());
    let _ = out.flush();
}

fn main() {
    let corlib = corlib_bytes().unwrap_or_else(|error| { eprintln!("lamella-mcp-csharp: {error}"); std::process::exit(1) });
    let check = LcscCompiler::discover().unwrap_or_else(|error| { eprintln!("lamella-mcp-csharp: {error}"); std::process::exit(1) });
    let run_compiler = LcscCompiler::discover().unwrap_or_else(|error| { eprintln!("lamella-mcp-csharp: {error}"); std::process::exit(1) });
    let mut repl = Repl::new(Box::new(run_compiler), Box::new(LoopbackLink::new(corlib)));

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
                    respond(&mut out, id, json!({ "protocolVersion": "2024-11-05", "capabilities": { "tools": {} }, "serverInfo": { "name": "lamella-mcp-csharp", "version": "0.1.0" } }));
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
                    let result = call_tool(&mut repl, &check, name, &args);
                    respond(&mut out, id, result);
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
