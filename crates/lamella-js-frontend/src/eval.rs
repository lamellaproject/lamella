//! `eval`, behind the `eval` cargo feature.

use crate::builtins::arg;
use crate::interpreter::{Completion, Interpreter};
use crate::value::JsValue;
use alloc::rc::Rc;

/// `eval` reached as a value: the program runs in the GLOBAL scope.
pub(crate) fn indirect_eval(
    interpreter: &mut Interpreter,
    _this: JsValue,
    arguments: &[JsValue],
) -> Completion {
    let source = arg(arguments, 0);
    let JsValue::String(text) = &source else {
        return Completion::Normal(source);
    };
    let text = text.to_lossy_string();
    compile_and_run(interpreter, &text)
}

fn compile_and_run(interpreter: &mut Interpreter, source: &str) -> Completion {
    let parsed = crate::parse_script(source);
    if parsed.has_errors() {
        let message = parsed
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.is_error())
            .map(|diagnostic| diagnostic.message.clone())
            .unwrap_or_else(|| crate::String::from("invalid program"));
        return interpreter.throw("SyntaxError", &message);
    }
    let bytes = crate::bytecode::encode(&parsed.program, &crate::bytecode::Options::minimal());
    drop(parsed);
    interpreter.run_artifact(Rc::from(bytes))
}
