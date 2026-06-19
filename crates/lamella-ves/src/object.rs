//! The managed heap and the reference-type object model.

#[cfg(feature = "gc")]
use crate::value::Location;
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
    /// A multi-dimensional (rectangular) array (II.14.2): its per-dimension lengths and
    /// its elements in row-major order. Accessed via `Get`/`Set`/`.ctor` calls on the
    /// array type, not the `ldelem`/`stelem` opcodes.
    MdArray {
        /// The length of each dimension.
        dims: Box<[i32]>,
        /// The elements, row-major; one per product-of-dims slot.
        elements: Box<[Value]>,
    },
    /// A boxed value type (III.4.1 `box`): a heap copy of a value-type value, tagged
    /// with the value type's token so `unbox` / `unbox.any` and casts can recover it.
    Boxed {
        /// The boxed value type's token (from the `box` instruction).
        type_token: u32,
        /// The boxed value (a copy of the value-type value).
        value: Value,
    },
    /// A delegate (II.14.6): an invocation list of bound methods -- each a `(target,
    /// method)` where the target is a `Value::Object` for an instance method or
    /// `Value::Null` for a static one. A single-cast delegate has one entry; `Combine`
    /// concatenates lists for multicast. `Invoke` calls them in order.
    Delegate {
        /// The bound methods, called in order; the last one's result is the delegate's.
        invocations: Vec<(Value, u32)>,
    },
}

/// The initial collection threshold (object count) before the live set is known; it
/// adapts upward to ~2x the survivors after each collection, so the working set settles.
#[cfg(feature = "gc")]
const INITIAL_GC_THRESHOLD: usize = 64;

/// The managed heap: an arena of [`Object`]s. With the `gc` feature, the collector
/// compacts it; without it, an append-only arena that leaks (the no-GC profile).
#[derive(Debug)]
#[cfg_attr(not(feature = "gc"), derive(Default))]
pub struct Heap {
    objects: Vec<Object>,
    /// The object count at which the next collection triggers (see
    /// [`Heap::should_collect`]); adapts after each collection.
    #[cfg(feature = "gc")]
    gc_threshold: usize,
    /// Parallel to `objects`: whether each object is registered for finalization (its
    /// type has a `Finalize` and it has not been finalized or suppressed). The collector
    /// promotes an unreachable registered object to the f-reachable set instead of
    /// reclaiming it.
    #[cfg(feature = "finalizers")]
    finalize_registered: Vec<bool>,
}

#[cfg(feature = "gc")]
impl Default for Heap {
    fn default() -> Heap {
        Heap {
            objects: Vec::new(),
            gc_threshold: INITIAL_GC_THRESHOLD,
            #[cfg(feature = "finalizers")]
            finalize_registered: Vec::new(),
        }
    }
}

impl Heap {
    /// Creates an empty heap.
    #[must_use]
    pub fn new() -> Heap {
        Heap::default()
    }

    /// Allocates `object` and returns a reference to it. With the `gc` feature an
    /// object unreachable from the roots is later reclaimed by [`Heap::collect`];
    /// without it the arena only grows.
    pub fn alloc(&mut self, object: Object) -> ObjectRef {
        let index = self.objects.len() as u32;
        self.objects.push(object);
        #[cfg(feature = "finalizers")]
        self.finalize_registered.push(false);
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
            | Object::MdArray { .. }
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
            | Object::MdArray { .. }
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
            | Object::MdArray { .. }
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

    /// Stores `value` into the box at `reference` (for `unbox` + `stobj`/`stind`); returns
    /// `false` if `reference` is not a box.
    pub fn set_boxed_value(&mut self, reference: ObjectRef, value: Value) -> bool {
        match self.objects.get_mut(reference.0 as usize) {
            Some(Object::Boxed { value: slot, .. }) => {
                *slot = value;
                true
            }
            _ => false,
        }
    }

    /// Allocates a delegate binding `target` to `method` and returns a reference.
    pub fn alloc_delegate(&mut self, target: Value, method: u32) -> ObjectRef {
        self.alloc(Object::Delegate {
            invocations: alloc::vec![(target, method)],
        })
    }

    /// Allocates a (multicast) delegate with the given invocation list.
    pub fn alloc_multicast(&mut self, invocations: Vec<(Value, u32)>) -> ObjectRef {
        self.alloc(Object::Delegate { invocations })
    }

    /// The invocation list of the delegate at `reference`, if it is a delegate.
    #[must_use]
    pub fn delegate_invocations(&self, reference: ObjectRef) -> Option<&[(Value, u32)]> {
        match self.get(reference)? {
            Object::Delegate { invocations } => Some(invocations),
            _ => None,
        }
    }

    /// The length of the array at `reference`, if it is an array.
    #[must_use]
    pub fn array_len(&self, reference: ObjectRef) -> Option<usize> {
        match self.get(reference)? {
            Object::Array { elements } => Some(elements.len()),
            Object::MdArray { elements, .. } => Some(elements.len()),
            _ => None,
        }
    }

    /// The length of dimension `dim` of the array at `reference` (a multi-dimensional
    /// array's per-dimension length, or a single-dimension array's length at `dim == 0`).
    #[must_use]
    pub fn array_dimension(&self, reference: ObjectRef, dim: i32) -> Option<i32> {
        match self.get(reference)? {
            Object::MdArray { dims, .. } => dims.get(usize::try_from(dim).ok()?).copied(),
            Object::Array { elements } if dim == 0 => i32::try_from(elements.len()).ok(),
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

    /// Allocates a multi-dimensional array with the given per-dimension lengths (elements
    /// zero-initialized to int32 zero) and returns a reference.
    pub fn alloc_md_array(&mut self, dims: Vec<i32>) -> ObjectRef {
        let total: usize = dims.iter().map(|&d| d.max(0) as usize).product();
        let elements = alloc::vec![Value::Int32(0); total].into_boxed_slice();
        self.alloc(Object::MdArray {
            dims: dims.into_boxed_slice(),
            elements,
        })
    }

    /// The element at `indices` (row-major) of the multi-dimensional array at `reference`,
    /// if it is such an array with every index in bounds.
    #[must_use]
    pub fn md_array_get(&self, reference: ObjectRef, indices: &[i32]) -> Option<Value> {
        let Object::MdArray { dims, elements } = self.get(reference)? else {
            return None;
        };
        elements.get(md_flat_index(dims, indices)?).cloned()
    }

    /// Stores `value` at `indices` of the multi-dimensional array at `reference`; returns
    /// `false` if `reference` is not such an array or an index is out of range.
    pub fn md_array_set(&mut self, reference: ObjectRef, indices: &[i32], value: Value) -> bool {
        let Some(Object::MdArray { dims, elements }) = self.objects.get_mut(reference.0 as usize)
        else {
            return false;
        };
        let Some(flat) = md_flat_index(dims, indices) else {
            return false;
        };
        match elements.get_mut(flat) {
            Some(slot) => {
                *slot = value;
                true
            }
            None => false,
        }
    }
}

/// The row-major flat index for `indices` in an array of shape `dims`, or `None` if the
/// ranks differ or any index is out of range.
fn md_flat_index(dims: &[i32], indices: &[i32]) -> Option<usize> {
    if indices.len() != dims.len() {
        return None;
    }
    let mut flat: usize = 0;
    for (&dim, &index) in dims.iter().zip(indices) {
        if index < 0 || index >= dim {
            return None;
        }
        flat = flat * (dim as usize) + (index as usize);
    }
    Some(flat)
}

/// The mark-compact garbage collector (the `gc` feature -- `surface.gc = collected`).
///
/// A precise, moving, stop-the-world collector over the index-based arena: it marks the
/// objects reachable from the roots, compacts the survivors (renumbering -- an
/// `ObjectRef` is an arena index), and rewrites every reference. `enumerate_roots` visits
/// each root value mutably; the caller exposes the interpreter frames, statics, and any
/// other root state. It is called twice -- once to mark, once to relocate.
#[cfg(feature = "gc")]
impl Heap {
    /// The number of objects currently in the arena.
    #[must_use]
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Whether the live arena has reached the collection threshold -- the trigger the
    /// interpreter checks at each instruction boundary to decide whether to collect.
    #[must_use]
    pub fn should_collect(&self) -> bool {
        self.objects.len() >= self.gc_threshold
    }

    /// Registers `reference` for finalization -- its type declares a `Finalize`, so the
    /// collector runs that finalizer when the object becomes unreachable. Also serves
    /// `ReRegisterForFinalize` (re-arming a suppressed or already-finalized object).
    #[cfg(feature = "finalizers")]
    pub fn register_finalizer(&mut self, reference: ObjectRef) {
        if let Some(slot) = self.finalize_registered.get_mut(reference.0 as usize) {
            *slot = true;
        }
    }

    /// Cancels finalization for `reference` (`GC.SuppressFinalize`).
    #[cfg(feature = "finalizers")]
    pub fn suppress_finalizer(&mut self, reference: ObjectRef) {
        if let Some(slot) = self.finalize_registered.get_mut(reference.0 as usize) {
            *slot = false;
        }
    }

    /// Reclaims every object unreachable from the roots and compacts the survivors,
    /// returning the objects promoted for finalization (the f-reachable set) at their new
    /// positions -- always empty without the `finalizers` feature.
    pub fn collect<R>(&mut self, mut enumerate_roots: R) -> Vec<ObjectRef>
    where
        R: FnMut(&mut dyn FnMut(&mut Value)),
    {
        let count = self.objects.len();
        let mut live = alloc::vec![false; count];
        let mut work: Vec<usize> = Vec::new();
        enumerate_roots(&mut |value| {
            collect_refs(value, &mut |reference| {
                let index = reference.0 as usize;
                if index < count && !live[index] {
                    live[index] = true;
                    work.push(index);
                }
            });
        });
        trace(&self.objects, &mut live, &mut work);

        #[cfg(feature = "finalizers")]
        let finalizable: Vec<usize> = {
            let mut promoted = Vec::new();
            #[allow(clippy::needless_range_loop)]
            for index in 0..count {
                if !live[index] && self.finalize_registered[index] {
                    live[index] = true;
                    self.finalize_registered[index] = false;
                    work.push(index);
                    promoted.push(index);
                }
            }
            trace(&self.objects, &mut live, &mut work);
            promoted
        };

        let mut remap: Vec<Option<u32>> = alloc::vec![None; count];
        let mut next = 0u32;
        for (slot, &alive) in remap.iter_mut().zip(live.iter()) {
            if alive {
                *slot = Some(next);
                next += 1;
            }
        }
        let old = core::mem::take(&mut self.objects);
        #[cfg(feature = "finalizers")]
        let old_registered = core::mem::take(&mut self.finalize_registered);
        for (index, mut object) in old.into_iter().enumerate() {
            if live[index] {
                remap_object(&mut object, &remap);
                self.objects.push(object);
                #[cfg(feature = "finalizers")]
                self.finalize_registered.push(old_registered[index]);
            }
        }
        enumerate_roots(&mut |value| remap_value(value, &remap));
        self.gc_threshold = self
            .objects
            .len()
            .saturating_mul(2)
            .max(INITIAL_GC_THRESHOLD);

        #[cfg(feature = "finalizers")]
        let result = finalizable
            .iter()
            .filter_map(|&index| remap[index].map(ObjectRef))
            .collect();
        #[cfg(not(feature = "finalizers"))]
        let result = Vec::new();
        result
    }
}

/// Drains the mark worklist: traces every reference out of each marked object, marking
/// and enqueuing newly reached objects. Shared by the root mark and the finalizer-promotion
/// re-trace.
#[cfg(feature = "gc")]
fn trace(objects: &[Object], live: &mut [bool], work: &mut Vec<usize>) {
    while let Some(index) = work.pop() {
        let mut children: Vec<usize> = Vec::new();
        object_refs(&objects[index], &mut |reference| {
            children.push(reference.0 as usize);
        });
        for child in children {
            if child < live.len() && !live[child] {
                live[child] = true;
                work.push(child);
            }
        }
    }
}

/// Visits each heap reference inside a value -- an object reference, a heap-pointing
/// byref, or (recursively) a value-type's fields.
#[cfg(feature = "gc")]
fn collect_refs<F: FnMut(ObjectRef)>(value: &Value, visit: &mut F) {
    match value {
        Value::Object(reference) => visit(*reference),
        Value::ByRef(Location::Field { object, .. }) => visit(*object),
        Value::ByRef(Location::Element { array, .. }) => visit(*array),
        Value::ByRef(Location::Boxed { object }) => visit(*object),
        Value::Struct(fields) => fields.iter().for_each(|field| collect_refs(field, visit)),
        _ => {}
    }
}

/// Visits each heap reference inside an object.
#[cfg(feature = "gc")]
fn object_refs<F: FnMut(ObjectRef)>(object: &Object, visit: &mut F) {
    match object {
        Object::Instance { fields, .. } => fields.iter().for_each(|f| collect_refs(f, visit)),
        Object::Array { elements } => elements.iter().for_each(|e| collect_refs(e, visit)),
        Object::MdArray { elements, .. } => elements.iter().for_each(|e| collect_refs(e, visit)),
        Object::Boxed { value, .. } => collect_refs(value, visit),
        Object::Delegate { invocations } => invocations
            .iter()
            .for_each(|(target, _)| collect_refs(target, visit)),
        Object::Str(_) => {}
    }
}

/// Rewrites each heap reference inside a value to its compacted position.
#[cfg(feature = "gc")]
fn remap_value(value: &mut Value, remap: &[Option<u32>]) {
    match value {
        Value::Object(reference) => remap_ref(reference, remap),
        Value::ByRef(Location::Field { object, .. }) => remap_ref(object, remap),
        Value::ByRef(Location::Element { array, .. }) => remap_ref(array, remap),
        Value::ByRef(Location::Boxed { object }) => remap_ref(object, remap),
        Value::Struct(fields) => fields.iter_mut().for_each(|f| remap_value(f, remap)),
        _ => {}
    }
}

/// Rewrites each heap reference inside an object to its compacted position.
#[cfg(feature = "gc")]
fn remap_object(object: &mut Object, remap: &[Option<u32>]) {
    match object {
        Object::Instance { fields, .. } => fields.iter_mut().for_each(|f| remap_value(f, remap)),
        Object::Array { elements } => elements.iter_mut().for_each(|e| remap_value(e, remap)),
        Object::MdArray { elements, .. } => elements.iter_mut().for_each(|e| remap_value(e, remap)),
        Object::Boxed { value, .. } => remap_value(value, remap),
        Object::Delegate { invocations } => invocations
            .iter_mut()
            .for_each(|(target, _)| remap_value(target, remap)),
        Object::Str(_) => {}
    }
}

/// Rewrites one reference via the compaction map (left unchanged if its target is gone).
#[cfg(feature = "gc")]
fn remap_ref(reference: &mut ObjectRef, remap: &[Option<u32>]) {
    if let Some(Some(new)) = remap.get(reference.0 as usize) {
        reference.0 = *new;
    }
}

#[cfg(all(test, feature = "gc"))]
mod gc_tests {
    use super::*;

    #[test]
    fn collect_reclaims_unreachable_and_relocates_live() {
        let mut heap = Heap::new();
        let kept = heap.alloc_string(&[b'a' as u16]);
        let _garbage = heap.alloc_string(&[b'x' as u16]);
        let root = heap.alloc_instance(7, alloc::vec![Value::Object(kept)]);
        assert_eq!(heap.object_count(), 3);

        let mut roots = alloc::vec![Value::Object(root)];
        heap.collect(|visit| roots.iter_mut().for_each(visit));

        assert_eq!(heap.object_count(), 2);
        let root = match &roots[0] {
            Value::Object(reference) => *reference,
            other => panic!("root not an object: {other:?}"),
        };
        let kept = match heap.instance_field(root, 0).unwrap() {
            Value::Object(reference) => reference,
            other => panic!("field not an object: {other:?}"),
        };
        assert_eq!(heap.as_string(kept), Some(&[b'a' as u16][..]));
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
