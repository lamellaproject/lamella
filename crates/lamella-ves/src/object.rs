//! The managed heap and the reference-type object model.

use crate::value::Value;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// A reference to a heap object: an index into the [`Heap`] arena. The null
/// reference is [`crate::value::Value::Null`], not an `ObjectRef`, so every
/// `ObjectRef` names a live object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectRef(u32);

/// A heap-allocated object: a `System.String` or an instance of a declared
/// reference type.
///
/// Strings are UTF-16 code units, matching `ldstr`'s `#US` heap and the lexer, so a
/// lone surrogate is representable. An instance carries its type id and its instance
/// fields. (`Eq` is not derived: a field may be a `Float`.)
#[derive(Debug, Clone, PartialEq)]
pub enum Object {
    /// A `System.String`, as its UTF-16 code units.
    Str(Box<[u16]>),
    /// An instance of a declared reference type: its type id (an index into the
    /// module's type table) and its instance fields in declaration-order slots.
    Instance {
        /// The declaring type, as a module type id.
        type_id: u32,
        /// The instance fields, one per slot (instance fields only, in declaration
        /// order), each initialized to its zero value at allocation.
        fields: Vec<Value>,
    },
    /// A single-dimensional, zero-based array (a vector): its elements, each
    /// initialized to the element type's zero value at allocation.
    Array {
        /// The elements, indexed 0..len.
        elements: Vec<Value>,
    },
    /// A boxed value type (III.4.1 `box`): a heap copy of a value-type value, tagged
    /// with the value type's token so `unbox` / `unbox.any` and casts can recover it.
    Boxed {
        /// The boxed value type's token (from the `box` instruction).
        type_token: u32,
        /// The boxed value (a copy of the value-type value).
        value: Value,
    },
    /// A delegate (II.14.6): a bound method -- an optional target object and the method
    /// it calls. Constructed by `newobj` on a delegate type, invoked by its `Invoke`.
    Delegate {
        /// The bound target: a `Value::Object` for an instance method, `Value::Null`
        /// for a static method.
        target: Value,
        /// The method the delegate invokes.
        method: u32,
    },
}

/// The managed heap: an append-only arena of [`Object`]s.
#[derive(Debug, Default)]
pub struct Heap {
    objects: Vec<Object>,
}

impl Heap {
    /// Creates an empty heap.
    #[must_use]
    pub fn new() -> Heap {
        Heap::default()
    }

    /// Allocates `object` and returns a reference to it. The object is never
    /// freed yet (the MVP leaks).
    pub fn alloc(&mut self, object: Object) -> ObjectRef {
        let index = self.objects.len() as u32;
        self.objects.push(object);
        ObjectRef(index)
    }

    /// Interns a UTF-16 string as a `System.String` and returns a reference.
    pub fn alloc_string(&mut self, chars: &[u16]) -> ObjectRef {
        self.alloc(Object::Str(chars.into()))
    }

    /// The object `reference` names, if it is live.
    #[must_use]
    pub fn get(&self, reference: ObjectRef) -> Option<&Object> {
        self.objects.get(reference.0 as usize)
    }

    /// The UTF-16 code units of the string at `reference`, if it is a string.
    #[must_use]
    pub fn as_string(&self, reference: ObjectRef) -> Option<&[u16]> {
        match self.get(reference)? {
            Object::Str(chars) => Some(chars),
            Object::Instance { .. }
            | Object::Array { .. }
            | Object::Boxed { .. }
            | Object::Delegate { .. } => None,
        }
    }

    /// Allocates an instance of `type_id` with initial `fields` and returns a
    /// reference. The caller supplies the zero-initialized field slots.
    pub fn alloc_instance(&mut self, type_id: u32, fields: Vec<Value>) -> ObjectRef {
        self.alloc(Object::Instance { type_id, fields })
    }

    /// The value of instance field `slot` at `reference`, if it is an instance with
    /// that slot.
    #[must_use]
    pub fn instance_field(&self, reference: ObjectRef, slot: u32) -> Option<Value> {
        match self.get(reference)? {
            Object::Instance { fields, .. } => fields.get(slot as usize).cloned(),
            Object::Str(_)
            | Object::Array { .. }
            | Object::Boxed { .. }
            | Object::Delegate { .. } => None,
        }
    }

    /// Stores `value` into instance field `slot` at `reference`. Returns `false` if
    /// `reference` is not an instance or `slot` is out of range.
    pub fn set_instance_field(&mut self, reference: ObjectRef, slot: u32, value: Value) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Instance { fields, .. }) => match fields.get_mut(slot as usize) {
                Some(target) => {
                    *target = value;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// The type id of the instance at `reference`, if it is an instance (the basis
    /// for virtual dispatch once that lands).
    #[must_use]
    pub fn type_of(&self, reference: ObjectRef) -> Option<u32> {
        match self.get(reference)? {
            Object::Instance { type_id, .. } => Some(*type_id),
            Object::Str(_)
            | Object::Array { .. }
            | Object::Boxed { .. }
            | Object::Delegate { .. } => None,
        }
    }

    /// Allocates an array with the given `elements` (already filled with the element
    /// type's zero value) and returns a reference.
    pub fn alloc_array(&mut self, elements: Vec<Value>) -> ObjectRef {
        self.alloc(Object::Array { elements })
    }

    /// Allocates a boxed value type tagged with `type_token` and returns a reference.
    pub fn alloc_boxed(&mut self, type_token: u32, value: Value) -> ObjectRef {
        self.alloc(Object::Boxed { type_token, value })
    }

    /// The value inside the box at `reference`, if it is a box.
    #[must_use]
    pub fn boxed_value(&self, reference: ObjectRef) -> Option<Value> {
        match self.get(reference)? {
            Object::Boxed { value, .. } => Some(value.clone()),
            _ => None,
        }
    }

    /// Allocates a delegate binding `target` to `method` and returns a reference.
    pub fn alloc_delegate(&mut self, target: Value, method: u32) -> ObjectRef {
        self.alloc(Object::Delegate { target, method })
    }

    /// The `(target, method)` of the delegate at `reference`, if it is a delegate.
    #[must_use]
    pub fn delegate_target(&self, reference: ObjectRef) -> Option<(Value, u32)> {
        match self.get(reference)? {
            Object::Delegate { target, method } => Some((target.clone(), *method)),
            _ => None,
        }
    }

    /// The length of the array at `reference`, if it is an array.
    #[must_use]
    pub fn array_len(&self, reference: ObjectRef) -> Option<usize> {
        match self.get(reference)? {
            Object::Array { elements } => Some(elements.len()),
            _ => None,
        }
    }

    /// The element at `index` of the array at `reference`, if it is an array with
    /// that index in bounds.
    #[must_use]
    pub fn array_get(&self, reference: ObjectRef, index: usize) -> Option<Value> {
        match self.get(reference)? {
            Object::Array { elements } => elements.get(index).cloned(),
            _ => None,
        }
    }

    /// Stores `value` at `index` of the array at `reference`. Returns `false` if
    /// `reference` is not an array or `index` is out of range.
    pub fn array_set(&mut self, reference: ObjectRef, index: usize, value: Value) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Array { elements }) => match elements.get_mut(index) {
                Some(slot) => {
                    *slot = value;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocated_objects_are_distinct_and_retrievable() {
        let mut heap = Heap::new();
        let a = heap.alloc_string(&[b'h' as u16, b'i' as u16]);
        let b = heap.alloc_string(&[b'y' as u16, b'o' as u16]);
        assert_ne!(a, b);
        assert_eq!(heap.as_string(a), Some(&[b'h' as u16, b'i' as u16][..]));
        assert_eq!(heap.as_string(b), Some(&[b'y' as u16, b'o' as u16][..]));
    }

    #[test]
    fn instances_carry_a_type_and_mutable_fields() {
        let mut heap = Heap::new();
        let object = heap.alloc_instance(7, alloc::vec![Value::Int32(0), Value::Null]);
        assert_eq!(heap.type_of(object), Some(7));
        assert_eq!(heap.instance_field(object, 0), Some(Value::Int32(0)));
        assert_eq!(heap.instance_field(object, 1), Some(Value::Null));
        assert!(heap.set_instance_field(object, 0, Value::Int32(42)));
        assert_eq!(heap.instance_field(object, 0), Some(Value::Int32(42)));
        assert!(!heap.set_instance_field(object, 9, Value::Int32(1)));
        let text = heap.alloc_string(&[b'x' as u16]);
        assert_eq!(heap.type_of(text), None);
        assert!(!heap.set_instance_field(text, 0, Value::Int32(1)));
    }
}
