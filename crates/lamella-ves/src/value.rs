//! The evaluation-stack value model: the CLI's reduced set of stack types.

use crate::object::ObjectRef;
use alloc::boxed::Box;

/// A value as it lives on the evaluation stack (ECMA-335 1st ed, III.1.1).
///
/// The set covers the numeric types, the null reference, object references, plus the
/// two pieces value types need: an inline value-type instance ([`Value::Struct`]) and
/// a managed pointer ([`Value::ByRef`]). [`Value`] is `Clone` rather than `Copy`
/// precisely so a load *clones* -- which deep-copies a struct and trivially copies a
/// scalar, giving value-type copy semantics for free.
#[derive(Debug, Clone, PartialEq)]
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
    /// held as `f64` (III permits carrying higher internal precision). Gated by `float`:
    /// the no-float tier omits it and the floating-point opcodes that produce it.
    #[cfg(feature = "float")]
    Float(f64),
    /// An object reference (`O`): a handle to a heap object.
    Object(ObjectRef),
    /// The null object reference.
    Null,
    /// A value-type instance held inline: its fields in declaration order. Cloning it
    /// deep-copies the fields, which is what makes assignment copy by value.
    Struct(Box<[Value]>),
    /// A managed pointer (`&`): a reference to a [`Location`] (III.1.1.1).
    ByRef(Location),
}

/// Where a managed pointer ([`Value::ByRef`]) points. A pointer into a frame names the
/// frame by its index in the call stack, so a callee can dereference a pointer to its
/// caller's local or argument; a pointer into the heap or statics is frame-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A local-variable slot of the frame at the given call-stack index.
    Local {
        /// The owning frame's index in the call stack.
        frame: usize,
        /// The local-variable slot within that frame.
        slot: usize,
    },
    /// An argument slot of the frame at the given call-stack index.
    Arg {
        /// The owning frame's index in the call stack.
        frame: usize,
        /// The argument slot within that frame.
        slot: usize,
    },
    /// An instance-field slot of a heap object.
    Field {
        /// The heap object owning the field.
        object: ObjectRef,
        /// The instance-field slot.
        slot: u32,
    },
    /// An element of a heap array.
    Element {
        /// The heap array.
        array: ObjectRef,
        /// The element index.
        index: usize,
    },
    /// A static-field storage slot.
    Static {
        /// The static-field storage slot.
        slot: usize,
    },
    /// The value inside a box -- the managed pointer `unbox` yields for in-place access.
    Boxed {
        /// The boxed object.
        object: ObjectRef,
    },
    /// A field within a value-type (struct) instance addressed by `base` -- the managed
    /// pointer `ldflda` yields for a nested value-type field (e.g. `o.inner.x`).
    Nested {
        /// The location of the containing struct.
        base: alloc::boxed::Box<Location>,
        /// The field slot within that struct.
        slot: u32,
    },
}

impl Value {
    /// Whether this value is "true" for a `brtrue`/`brfalse` test: a non-zero
    /// integer, or a non-null reference (ECMA-335 1st ed, III for `brtrue`).
    #[must_use]
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Int32(value) => *value != 0,
            Value::Int64(value) | Value::NativeInt(value) => *value != 0,
            #[cfg(feature = "float")]
            Value::Float(value) => *value != 0.0,
            Value::Object(_) | Value::ByRef(_) | Value::Struct(_) => true,
            Value::Null => false,
        }
    }
}
