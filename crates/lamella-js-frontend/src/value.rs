//! ECMAScript values.


use crate::string_value::JsString;

/// A handle to an object. Opaque here; the interpreter owns what it points at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub u32);

/// A handle to a Symbol. Opaque here; the interpreter owns the description behind it.
///
/// # A SYMBOL IS A HANDLE BECAUSE ITS IDENTITY IS NOT ITS CONTENTS
///
/// Two symbols with the same description are DIFFERENT values -- `Symbol("x") === Symbol("x")` is
/// `false`, and that is the entire point of the type: a symbol-keyed property cannot be reached by
/// a program that merely knows the name. Carrying the description inline and deriving equality from
/// it would make every `Symbol("x")` the same symbol, which is not a smaller feature but the
/// opposite of the feature.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolId(pub u32);

/// An ECMAScript language value.
#[derive(Debug, Clone, PartialEq)]
pub enum JsValue {
    Undefined,
    Null,
    Boolean(bool),
    /// **Always an f64.** A small-integer fast path is a REPRESENTATION
    /// choice; it may never change an observable result, which is why the abstract operations below
    /// are written against the f64 semantics and not against an integer shortcut.
    Number(f64),
    String(JsString),
    /// **A PRIMITIVE, NOT AN OBJECT**, and the only one with no implicit conversion to String.
    /// `"" + sym` is a TypeError where every other primitive would have concatenated -- deliberate,
    /// so a symbol cannot be stringified into a program by accident.
    Symbol(SymbolId),
    Object(ObjectId),
}

impl JsValue {
    #[must_use]
    pub fn number(value: f64) -> Self {
        JsValue::Number(value)
    }

    #[must_use]
    pub fn string(text: &str) -> Self {
        JsValue::String(JsString::from(text))
    }

    #[must_use]
    pub fn is_object(&self) -> bool {
        matches!(self, JsValue::Object(_))
    }

    /// The `typeof` result, which is a table rather than a computation.
    ///
    /// `typeof null` is `"object"` -- a bug preserved for compatibility since 1995, and normative.
    /// An engine that "fixes" it is not conforming.
    #[must_use]
    pub fn type_of(&self) -> &'static str {
        match self {
            JsValue::Undefined => "undefined",
            JsValue::Null => "object",
            JsValue::Boolean(_) => "boolean",
            JsValue::Number(_) => "number",
            JsValue::String(_) => "string",
            JsValue::Symbol(_) => "symbol",
            JsValue::Object(_) => "object",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Normative, and famous: `typeof null` is `"object"`. An engine that returns `"null"` is more
    /// sensible and less conforming.
    #[test]
    fn typeof_null_is_object_because_the_standard_says_so() {
        assert_eq!(JsValue::Null.type_of(), "object");
        assert_eq!(JsValue::Undefined.type_of(), "undefined");
        assert_eq!(JsValue::Number(0.0).type_of(), "number");
    }

    /// `-0` and `+0` are different values that compare equal. The distinction is observable through
    /// `1/x`, `Object.is` and `Math.sign`, so it has to survive in the representation even though
    /// `==` and `===` cannot see it.
    #[test]
    fn negative_zero_is_a_distinct_value_that_compares_equal() {
        let negative = JsValue::Number(-0.0);
        let positive = JsValue::Number(0.0);
        assert_eq!(negative, positive, "PartialEq on f64 follows the language: -0 == +0");
        match (&negative, &positive) {
            (JsValue::Number(a), JsValue::Number(b)) => {
                assert!(a.is_sign_negative() && b.is_sign_positive(), "and the sign survives");
            }
            _ => unreachable!(),
        }
    }
}
