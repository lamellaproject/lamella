//! The evaluation-stack value model: the CLI's reduced set of stack types.

use crate::object::ObjectRef;

/// A value as it lives on the evaluation stack (ECMA-335 1st ed, III.1.1).
///
/// Object references, managed pointers, and value-type instances arrive with the
/// object model; for now the set covers the numeric types plus the null
/// reference, which is all arithmetic, control flow, and static calls need.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    /// A 32-bit integer. Also how `bool`, `char`, `int8`, and `int16` appear on
    /// the stack, widened per III.1.1.1.
    Int32(i32),
    /// A 64-bit integer.
    Int64(i64),
    /// A native-sized integer (and unmanaged pointers). Held as 64-bit on the
    /// host; the width becomes target-configurable with the device tiers.
    NativeInt(i64),
    /// A floating-point value. The CLI tracks one native float type `F`; it is
    /// held as `f64` (III permits carrying higher internal precision).
    Float(f64),
    /// An object reference (`O`): a handle to a heap object.
    Object(ObjectRef),
    /// The null object reference.
    Null,
}

impl Value {
    /// Whether this value is "true" for a `brtrue`/`brfalse` test: a non-zero
    /// integer, or a non-null reference (ECMA-335 1st ed, III for `brtrue`).
    #[must_use]
    pub fn is_truthy(self) -> bool {
        match self {
            Value::Int32(value) => value != 0,
            Value::Int64(value) | Value::NativeInt(value) => value != 0,
            Value::Float(value) => value != 0.0,
            Value::Object(_) => true,
            Value::Null => false,
        }
    }
}
