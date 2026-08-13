//! The grammar's parameters, threaded from the first production.


/// Which grammar goal the source is being parsed as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseGoal {
    Script,
    /// Module code is **always strict**, reserves `await` at the top level, and enables `import` and
    /// `export` positionally. Not in this profile, but the parameter exists from the start
    /// because adding a goal later means revisiting every production that reads one.
    Module,
}

/// The grammar parameters in force at a production.
///
/// Every field is a spec parameter or a mode, and each carries the rule that reads it -- so a change
/// here is checkable against the clause rather than against someone's memory of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    pub goal: ParseGoal,
    /// Strict mode: from a directive prologue, from a class body, or from being module code.
    ///
    /// Decides a long list of early errors -- `with`, assignment to `eval`/`arguments`, duplicate
    /// parameter names, `delete` of an unqualified name -- and it is *inherited*, so a nested
    /// function of a strict function is strict.
    pub strict: bool,
    /// `[In]`: whether the `in` operator is permitted.
    ///
    /// **False inside a `for` statement's init clause**, where `in` would be ambiguous with
    /// `for (x in y)`. This is the parameter people forget, and forgetting it makes
    /// `for (var i = 'a' in b; ...)` parse as a for-in loop.
    pub allow_in: bool,
    /// `[Yield]`: `yield` is a keyword here rather than an identifier.
    pub allow_yield: bool,
    /// `[Await]`: `await` is a keyword here rather than an identifier.
    ///
    /// `await` is a **plain identifier in sloppy scripts** and reserved in modules and async
    /// bodies, which is exactly why this is a parameter and not a keyword table.
    pub allow_await: bool,
    /// `[Return]`: whether a `return` statement is legal, i.e. whether we are in a function body.
    pub allow_return: bool,
    /// `[Tagged]`: whether the template being parsed is a tagged template.
    ///
    /// Since ES2018 an **invalid escape sequence is legal in a tagged template** -- its cooked value
    /// becomes `undefined` and the raw text survives -- while the same chunk untagged is a syntax
    /// error. The lexer cannot know which, so it records the fact and this parameter decides.
    pub tagged: bool,
}

impl Context {
    /// A top-level script context.
    #[must_use]
    pub fn script() -> Self {
        Self {
            goal: ParseGoal::Script,
            strict: false,
            allow_in: true,
            allow_yield: false,
            allow_await: false,
            allow_return: false,
            tagged: false,
        }
    }

    /// A module context. Module code is strict and reserves `await` at the top level, both of which
    /// fall out here rather than being remembered at each use.
    #[must_use]
    pub fn module() -> Self {
        Self { goal: ParseGoal::Module, strict: true, allow_await: true, ..Self::script() }
    }

    /// A copy with `[In]` withdrawn, for a `for` header's init clause.
    #[must_use]
    pub fn without_in(self) -> Self {
        Self { allow_in: false, ..self }
    }

    #[must_use]
    pub fn with_in(self) -> Self {
        Self { allow_in: true, ..self }
    }

    /// A copy for a function body: `return` becomes legal, and `yield`/`await` take the values this
    /// function's own kind implies rather than inheriting the enclosing one's.
    ///
    /// **Inheriting them would be the bug**: a plain function nested inside an async function does
    /// NOT reserve `await`, and a copy that forgot to reset it would reject a legal identifier.
    #[must_use]
    pub fn function_body(self, is_generator: bool, is_async: bool) -> Self {
        Self {
            allow_return: true,
            allow_yield: is_generator,
            allow_await: is_async,
            allow_in: true,
            ..self
        }
    }

    /// A copy that is strict from here down. Strictness is inherited and never withdrawn.
    #[must_use]
    pub fn strict(self) -> Self {
        Self { strict: true, ..self }
    }

    #[must_use]
    pub fn tagged(self, tagged: bool) -> Self {
        Self { tagged, ..self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE DEFECT THIS SHAPE REMOVES: a mutable context field leaks when a restore is missed on
    /// some path. A `Copy` value passed as an argument cannot -- the caller's copy is untouched by
    /// definition, so this test is really asserting a property of the TYPE rather than of any code.
    #[test]
    fn a_child_context_cannot_change_its_parents() {
        let outer = Context::script();
        let inner = outer.function_body(false, true);
        assert!(inner.allow_await, "the async body reserves await");
        assert!(!outer.allow_await, "and the caller's context is untouched, with nothing to restore");
    }

    /// THE DEFECT THIS GUARDS: a plain function nested in an async function does NOT reserve
    /// `await`. Inheriting the flag rejects `function f(await) {}` inside an async function, which
    /// is legal.
    #[test]
    fn a_plain_function_inside_an_async_one_does_not_reserve_await() {
        let async_body = Context::script().function_body(false, true);
        let nested_plain = async_body.function_body(false, false);
        assert!(!nested_plain.allow_await, "the nested plain function releases it");
        assert!(!nested_plain.allow_yield);
        assert!(nested_plain.allow_return, "but return is still legal, being a function body");
    }

    /// `in` is withdrawn only for the for-header init clause, and it comes back inside any
    /// parenthesized subexpression -- so the withdrawal must be explicit at both ends.
    #[test]
    fn the_in_parameter_is_withdrawn_and_restored_explicitly() {
        let header = Context::script().without_in();
        assert!(!header.allow_in);
        assert!(header.with_in().allow_in);
    }

    #[test]
    fn module_code_is_strict_and_reserves_await_without_anyone_remembering_to() {
        let module = Context::module();
        assert!(module.strict);
        assert!(module.allow_await);
    }

    /// Strictness is inherited and never withdrawn: a sloppy function inside a strict one is still
    /// strict. A `with_strict(bool)` setter would make "turn it off" expressible, which the grammar
    /// never permits, so the only operation offered is turning it on.
    #[test]
    fn strictness_is_one_way() {
        let strict = Context::script().strict();
        assert!(strict.function_body(false, false).strict, "inherited into the nested body");
    }
}
