//! Generator objects, their three intrinsics, and the `next`/`return`/`throw` protocol.

use crate::interpreter::{Absence, Completion, Interpreter};
use crate::object::{GeneratorState, Object, Property, PropertyKey, PropertyKind};
use crate::value::{JsValue, ObjectId};

/// Builds `%GeneratorFunction%`, `%GeneratorFunction.prototype%` and `%GeneratorPrototype%`.
///
/// **THIS RUNS AFTER `iterator::install`, AND IT HAS TO.** `%GeneratorPrototype%` inherits
/// `%IteratorPrototype%`, which is where its `[Symbol.iterator]` comes from -- so a generator is
/// iterable by the same one line that makes an array iterator iterable, rather than by a second
/// copy of it. Installed before, the prototype would be built on a placeholder id and every
/// `for (const x of g())` would be a TypeError about a value that has `next`.
pub(crate) fn install(interpreter: &mut Interpreter) {
    let generator_prototype =
        interpreter.allocate(Object::new(Some(interpreter.intrinsics.iterator_prototype)));
    interpreter.intrinsics.generator_prototype = generator_prototype;
    interpreter.define_method(generator_prototype, "next", 1, next);
    interpreter.define_method(generator_prototype, "return", 1, return_);
    interpreter.define_method(generator_prototype, "throw", 1, throw);
    interpreter.define_to_string_tag(generator_prototype, "Generator");

    let generator_function_prototype =
        interpreter.allocate(Object::new(Some(interpreter.intrinsics.function_prototype)));
    interpreter.intrinsics.generator_function_prototype = generator_function_prototype;
    interpreter.define_to_string_tag(generator_function_prototype, "GeneratorFunction");

    let refuse: crate::interpreter::NativeFn =
        |interpreter, _this, _arguments| interpreter.refuse(Absence::FunctionConstructor);
    let generator_function =
        interpreter.native_constructor("GeneratorFunction", 1, refuse, refuse);
    interpreter.intrinsics.generator_function = generator_function;

    let function_constructor = match interpreter.global_value("Function") {
        Some(JsValue::Object(id)) => Some(id),
        _ => None,
    };
    if let Some(function_constructor) = function_constructor {
        interpreter.object_mut(generator_function).prototype = Some(function_constructor);
    }

    interpreter.define_constant(
        generator_function,
        "prototype",
        JsValue::Object(generator_function_prototype),
    );
    define_read_only(
        interpreter,
        generator_function_prototype,
        "constructor",
        JsValue::Object(generator_function),
    );
    define_read_only(
        interpreter,
        generator_function_prototype,
        "prototype",
        JsValue::Object(generator_prototype),
    );
    define_read_only(
        interpreter,
        generator_prototype,
        "constructor",
        JsValue::Object(generator_function_prototype),
    );
}

/// `{ [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }`: the attributes every
/// `constructor` and `prototype` property in this graph carries.
///
/// Neither `define_builtin` (writable and configurable) nor `define_constant` (neither) has this
/// shape, and it is the shape 27.3.3 and 27.5.1 specify for all four of them.
fn define_read_only(interpreter: &mut Interpreter, target: ObjectId, name: &str, value: JsValue) {
    interpreter.object_mut(target).set_own(
        PropertyKey::from_str(name),
        Property {
            kind: PropertyKind::Data { value, writable: false },
            enumerable: false,
            configurable: true,
        },
    );
}

/// `OrdinaryCreateFromConstructor(F, "%GeneratorFunction.prototype.prototype%")` followed by
/// `GeneratorStart`: what CALLING a generator function does.
///
/// **THE BODY DOES NOT RUN HERE, AND THAT IS THE FEATURE RATHER THAN AN OPTIMIZATION.**
/// `function* g() { effect(); }` followed by `g()` must produce a generator and run nothing; the
/// body waits for the first `next()`. An implementation that ran the body eagerly and handed back
/// its answer would agree with the standard on every generator that has no side effects, which is
/// every generator anybody writes a quick test with.
///
/// The PARAMETERS are already bound by the time this is called, because
/// `FunctionDeclarationInstantiation` is step 1 of `EvaluateGeneratorBody` and the generator object
/// is step 2. That ordering is observable: `function* g(a = effect()) {}` runs `effect` at the CALL,
/// not at the first `next()`, and `function* g([a]) {}` called with no argument throws its TypeError
/// there too.
pub(crate) fn create(
    interpreter: &mut Interpreter,
    function: ObjectId,
    context: crate::interpreter::GeneratorContext,
) -> Completion {
    let prototype = match interpreter.get_property(function, &PropertyKey::from_str("prototype")) {
        Completion::Normal(JsValue::Object(id)) => id,
        Completion::Normal(_) => interpreter.intrinsics.generator_prototype,
        abrupt => return abrupt,
    };
    let mut object = Object::new(Some(prototype));
    object.generator = Some(crate::Box::new(crate::interpreter::GeneratorData {
        state: GeneratorState::SuspendedStart,
        context: Some(context),
    }));
    Completion::Normal(JsValue::Object(interpreter.allocate(object)))
}

/// `GeneratorValidate`: the brand check, and then the one state that refuses.
///
/// **IT IS THE FIRST STEP OF ALL THREE METHODS, AND THE ORDER IS OBSERVABLE.** `g.throw(e)` on a
/// generator whose body is running is a **TypeError about re-entry**, not `e` -- so a `throw` that
/// validated after deciding what to do with its argument would report the caller's exception and
/// hide the misuse.
fn validate(interpreter: &mut Interpreter, this: &JsValue) -> Result<GeneratorState, Completion> {
    let JsValue::Object(id) = this else {
        return Err(interpreter.type_error("this value is not a generator"));
    };
    let Some(data) = interpreter.object(*id).generator.as_ref() else {
        return Err(interpreter.type_error("this value is not a generator"));
    };
    let state = data.state;
    if state == GeneratorState::Executing {
        return Err(interpreter.type_error("this generator is already running"));
    }
    Ok(state)
}

/// `%GeneratorPrototype%.next`.
///
/// **THE ARGUMENT IS DISCARDED, AND THE STANDARD DISCARDS IT TOO.** `next(v)` delivers `v` as the
/// value of the `yield` the generator is suspended at -- and a generator in `suspendedStart` is not
/// suspended at one, so `GeneratorStart` drops the first resumption's value however it is spelled.
/// Since `suspendedYield` cannot be reached while a `yield` does not parse, every resumption this
/// profile can perform is a first one. It stops being right at the same moment `yield` lands, which
/// is why the parameter is named rather than elided from the signature.
fn next(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue]) -> Completion {
    let _ = arguments;
    let state = match validate(interpreter, &this) {
        Ok(state) => state,
        Err(abrupt) => return abrupt,
    };
    let JsValue::Object(id) = this else {
        return interpreter.internal_defect("a validated generator is not an object");
    };
    match state {
        GeneratorState::Completed => iter_result(interpreter, JsValue::Undefined, true),
        GeneratorState::SuspendedStart | GeneratorState::SuspendedYield => {
            resume_to_completion(interpreter, id)
        }
        GeneratorState::Executing => {
            interpreter.internal_defect("a validated generator is executing")
        }
    }
}

/// `%GeneratorPrototype%.return`.
///
/// **IT COMPLETES A `suspendedStart` GENERATOR WITHOUT RUNNING ANY OF THE BODY**, which is why it
/// works in full here while `next` still has a shape it refuses: closing a generator that has not
/// started is `GeneratorResumeAbrupt` steps 2 and 3, and neither one resumes anything.
fn return_(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue]) -> Completion {
    let value = arguments.first().cloned().unwrap_or(JsValue::Undefined);
    let state = match validate(interpreter, &this) {
        Ok(state) => state,
        Err(abrupt) => return abrupt,
    };
    let JsValue::Object(id) = this else {
        return interpreter.internal_defect("a validated generator is not an object");
    };
    match state {
        GeneratorState::SuspendedStart
        | GeneratorState::SuspendedYield
        | GeneratorState::Completed => {
            complete(interpreter, id);
            iter_result(interpreter, value, true)
        }
        GeneratorState::Executing => {
            interpreter.internal_defect("a validated generator is executing")
        }
    }
}

/// `%GeneratorPrototype%.throw`.
///
/// The argument is thrown FROM the generator, so a `suspendedStart` one completes and the
/// exception comes straight back out -- there is no body between the caller and the throw.
fn throw(interpreter: &mut Interpreter, this: JsValue, arguments: &[JsValue]) -> Completion {
    let value = arguments.first().cloned().unwrap_or(JsValue::Undefined);
    let state = match validate(interpreter, &this) {
        Ok(state) => state,
        Err(abrupt) => return abrupt,
    };
    let JsValue::Object(id) = this else {
        return interpreter.internal_defect("a validated generator is not an object");
    };
    match state {
        GeneratorState::SuspendedStart
        | GeneratorState::SuspendedYield
        | GeneratorState::Completed => {
            complete(interpreter, id);
            Completion::Throw(value)
        }
        GeneratorState::Executing => {
            interpreter.internal_defect("a validated generator is executing")
        }
    }
}

/// Runs the whole body in one resumption, which is what a body with no `yield` in it does.
///
/// **THE STATE IS WRITTEN BEFORE THE BODY RUNS AND CLEARED AFTER IT, AND BOTH HALVES ARE
/// OBSERVABLE.** `executing` is what makes a generator that resumes itself --
/// `var g = f(); function* f() { g.next(); }` -- a TypeError rather than unbounded recursion, and
/// clearing it is what leaves the generator `completed` rather than permanently locked when the
/// body throws.
fn resume_to_completion(interpreter: &mut Interpreter, id: ObjectId) -> Completion {
    let Some(context) = interpreter
        .object(id)
        .generator
        .as_ref()
        .and_then(|data| data.context.clone())
    else {
        return interpreter.internal_defect("a suspended generator has no context to resume");
    };
    let before = frame_state(interpreter, context.frame);
    set_state(interpreter, id, GeneratorState::Executing);
    let completion = interpreter.run_generator_body(&context);
    let after = frame_state(interpreter, context.frame);
    let suspended = after != before;
    match completion {
        Completion::Normal(value) => {
            if suspended {
                set_state(interpreter, id, GeneratorState::SuspendedYield);
                iter_result(interpreter, value, false)
            } else {
                complete(interpreter, id);
                iter_result(interpreter, value, true)
            }
        }
        abrupt => {
            complete(interpreter, id);
            abrupt
        }
    }
}

/// Reads the resume state out of the frame the desugared body dispatches on.
///
/// A MISSING OR NON-NUMERIC STATE READS AS `NaN`, which compares unequal to everything including
/// itself -- so a frame a program somehow damaged reports "suspended" rather than "finished", and
/// the generator stops rather than being resumed against a state nobody wrote.
fn frame_state(interpreter: &mut Interpreter, frame: ObjectId) -> f64 {
    match interpreter
        .object(frame)
        .own(&PropertyKey::from_str(crate::generator_transform::STATE))
        .and_then(|property| property.data_value().cloned())
    {
        Some(JsValue::Number(state)) => state,
        _ => f64::NAN,
    }
}

/// Moves a generator to `completed` and DROPS its context.
///
/// Dropping is not tidiness: the context holds the arguments, the `this` and the scope the
/// parameters were bound into, and a completed generator that kept them would retain every
/// argument it was ever called with on a heap with no collector.
fn complete(interpreter: &mut Interpreter, id: ObjectId) {
    if let Some(data) = interpreter.object_mut(id).generator.as_mut() {
        data.state = GeneratorState::Completed;
        data.context = None;
    }
}

fn set_state(interpreter: &mut Interpreter, id: ObjectId, state: GeneratorState) {
    if let Some(data) = interpreter.object_mut(id).generator.as_mut() {
        data.state = state;
    }
}

/// `CreateIterResultObject`. A fresh ordinary object with two data properties, built directly
/// rather than through anything a program can intercept.
fn iter_result(interpreter: &mut Interpreter, value: JsValue, done: bool) -> Completion {
    let id = interpreter.allocate(Object::new(Some(interpreter.intrinsics.object_prototype)));
    let _ = interpreter.create_data_property(id, PropertyKey::from_str("value"), value);
    let _ =
        interpreter.create_data_property(id, PropertyKey::from_str("done"), JsValue::Boolean(done));
    Completion::Normal(JsValue::Object(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resumes one generator's body twice over the SAME context, which is what a suspending body
    /// will do and what nothing can do from JavaScript while `yield` is refused.
    fn resume_twice(source: &str) -> (JsValue, JsValue) {
        let mut interpreter = Interpreter::new();
        let value = interpreter.eval_source(source).expect("the probe program runs");
        let JsValue::Object(id) = value else { panic!("the probe must end with a generator") };
        let context = interpreter
            .object(id)
            .generator
            .as_ref()
            .and_then(|data| data.context.clone())
            .expect("a fresh generator carries a context");
        let first = interpreter.run_generator_body(&context);
        let second = interpreter.run_generator_body(&context);
        let unwrap = |completion| match completion {
            Completion::Normal(value) => value,
            other => panic!("the body did not complete normally: {other:?}"),
        };
        (unwrap(first), unwrap(second))
    }

    /// **A BODY THAT RE-INITIALIZES ITS STATE CANNOT SEE THE SHARED ENVIRONMENT AT ALL**, which is
    /// why the asymmetry below is easy to miss.
    ///
    /// A body re-entered per `next()` runs in the environment its parameters were bound into, and
    /// `body_environment` shares that environment only for a SIMPLE parameter list. Here both
    /// bodies assign `n = 0` on entry, so the sharing is invisible and all four answers agree --
    /// which is exactly what a quick test of the mechanism looks like, and exactly why it proves
    /// nothing about state that is NOT reset. The test beside this one asks the question where the
    /// answer can differ.
    #[test]
    fn a_body_that_reinitializes_its_state_cannot_see_the_shared_environment() {
        let simple = resume_twice("function* g() { var n = 0; n = n + 1; return n; } g();");
        let defaulted =
            resume_twice("function* g(a = 1) { var n = 0; n = n + 1; return n; } g();");
        assert_eq!(simple.0, JsValue::Number(1.0), "first resumption, simple parameters");
        assert_eq!(simple.1, JsValue::Number(1.0), "second resumption, simple parameters");
        assert_eq!(defaulted.0, JsValue::Number(1.0), "first resumption, a default");
        assert_eq!(defaulted.1, JsValue::Number(1.0), "second resumption, a default");
    }

    /// **WHAT SURVIVES A RESUMPTION MUST NOT DEPEND ON THE PARAMETER LIST**, and this measures it.
    #[test]
    fn what_a_resumption_carries_does_not_depend_on_the_parameter_list() {
        let simple = resume_twice(
            "function* g() { var n; if (n === undefined) { n = 0; } n = n + 1; return n; } g();",
        );
        let defaulted = resume_twice(
            "function* g(a = 1) { var n; if (n === undefined) { n = 0; } n = n + 1; return n; } g();",
        );
        assert_eq!(simple.0, JsValue::Number(1.0), "first resumption, simple parameters");
        assert_eq!(defaulted.0, JsValue::Number(1.0), "first resumption, a default");
        assert_eq!(simple.1, JsValue::Number(2.0), "the environment persists across a resumption");
        assert_eq!(
            simple.1, defaulted.1,
            "a resumption must not depend on the parameter list's shape: simple gave {:?} and a \
             default gave {:?}",
            simple.1, defaulted.1
        );
    }
}
