//! The managed heap and the reference-type object model.

use alloc::boxed::Box;
use alloc::vec::Vec;

/// A reference to a heap object: an index into the [`Heap`] arena. The null
/// reference is [`crate::value::Value::Null`], not an `ObjectRef`, so every
/// `ObjectRef` names a live object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectRef(u32);

/// A heap-allocated object.
///
/// Only `System.String` exists so far; instances of declared types arrive with
/// the type system. Strings are UTF-16 code units, matching `ldstr`'s `#US` heap
/// and the lexer, so a lone surrogate is representable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Object {
    /// A `System.String`, as its UTF-16 code units.
    Str(Box<[u16]>),
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
}
