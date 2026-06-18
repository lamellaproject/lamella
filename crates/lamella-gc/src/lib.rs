#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The shared garbage-collection contract.

/// An opaque reference to a managed object on our collector's heap.
///
/// The representation is deliberately hidden: a handle / arena index or a tagged
/// linear-memory offset for our own collector (bare metal and WASM 1.0 linear
/// memory). Under WasmGC the host VM tracks references with its own type and this
/// contract is not used at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectRef(u32);

impl ObjectRef {
    /// The null reference.
    pub const NULL: ObjectRef = ObjectRef(0);

    /// Wraps a collector-assigned handle. Constructing references is the
    /// collector's job; this is for the heap that owns the numbering.
    #[must_use]
    pub const fn from_handle(handle: u32) -> ObjectRef {
        ObjectRef(handle)
    }

    /// The raw handle, for the collector's own bookkeeping.
    #[must_use]
    pub const fn handle(self) -> u32 {
        self.0
    }

    /// Whether this is the null reference.
    #[must_use]
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// A managed pointer (`&`): a reference into the interior of an object, kept as
/// the base object plus a byte offset so it survives compaction (fix the base,
/// recompute the interior).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedPtr {
    /// The object the pointer is interior to.
    pub base: ObjectRef,
    /// The byte offset of the pointee within `base`.
    pub offset: u32,
}

/// The collector's sink for references discovered during a trace.
///
/// Slots are passed by mutable reference so a moving collector can write the
/// relocated reference back in place. A non-moving collector simply reads them.
pub trait RootVisitor {
    /// Report an object-reference slot (a root, or an object's reference field).
    fn visit_object(&mut self, slot: &mut ObjectRef);

    /// Report an interior pointer: its base-object slot (updatable) and the byte
    /// offset, reported apart so the base relocates and the interior is recomputed.
    fn visit_interior(&mut self, base: &mut ObjectRef, offset: u32);
}

/// Something that holds references and can report them to a [`RootVisitor`].
///
/// Implemented by both ends of the reachability walk: a stack frame reports its
/// roots (eval stack, locals, argument slots), and a heap object reports its
/// reference fields. The collector treats them uniformly.
pub trait Trace {
    /// Report every reference held, by visiting each as an updatable slot.
    fn trace(&mut self, visitor: &mut dyn RootVisitor);
}

/// The allocation seam, selecting the memory-management strategy.
///
/// A collecting heap implements this and traces on out-of-memory; the no-GC
/// (Marshal) profile implements it as bump/explicit allocation with no tracing.
/// The whole tracing contract above compiles out under the no-GC profile.
pub trait Allocator {
    /// Allocate `size` bytes for an object whose runtime type is identified by
    /// `type_id` (the collector uses it to find the object's reference-field map
    /// when tracing). Returns the new reference, or `None` on out-of-memory after
    /// any collection attempt.
    fn allocate(&mut self, type_id: u32, size: usize) -> Option<ObjectRef>;
}

/// Records that a reference field is being overwritten, then performs the write.
///
/// A non-generational, stop-the-world collector ignores the event (this is the
/// baseline). Generational, incremental, or concurrent collectors override the
/// hook to maintain a remembered set / card table. Defined now so AOT can emit the
/// call and the interpreter can route through it, at zero cost until a collector
/// tier needs it.
#[inline]
pub fn write_barrier(_holder: ObjectRef, slot: &mut ObjectRef, new: ObjectRef) {
    *slot = new;
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::vec::Vec;

    struct Frame {
        locals: [ObjectRef; 2],
        pointer: ManagedPtr,
    }

    impl Trace for Frame {
        fn trace(&mut self, visitor: &mut dyn RootVisitor) {
            for local in &mut self.locals {
                visitor.visit_object(local);
            }
            visitor.visit_interior(&mut self.pointer.base, self.pointer.offset);
        }
    }

    #[derive(Default)]
    struct Relocator {
        seen: Vec<u32>,
    }

    impl RootVisitor for Relocator {
        fn visit_object(&mut self, slot: &mut ObjectRef) {
            self.seen.push(slot.handle());
            *slot = ObjectRef::from_handle(slot.handle() + 10);
        }
        fn visit_interior(&mut self, base: &mut ObjectRef, _offset: u32) {
            self.seen.push(base.handle());
            *base = ObjectRef::from_handle(base.handle() + 10);
        }
    }

    #[test]
    fn a_frame_reports_roots_and_the_collector_relocates_them_in_place() {
        let mut frame = Frame {
            locals: [ObjectRef::from_handle(1), ObjectRef::from_handle(2)],
            pointer: ManagedPtr {
                base: ObjectRef::from_handle(3),
                offset: 8,
            },
        };
        let mut collector = Relocator::default();
        frame.trace(&mut collector);

        assert_eq!(collector.seen, alloc::vec![1, 2, 3]);
        assert_eq!(frame.locals[0], ObjectRef::from_handle(11));
        assert_eq!(frame.locals[1], ObjectRef::from_handle(12));
        assert_eq!(frame.pointer.base, ObjectRef::from_handle(13));
        assert_eq!(frame.pointer.offset, 8);
    }

    #[test]
    fn the_write_barrier_baseline_just_assigns() {
        let mut slot = ObjectRef::from_handle(1);
        write_barrier(
            ObjectRef::from_handle(9),
            &mut slot,
            ObjectRef::from_handle(2),
        );
        assert_eq!(slot, ObjectRef::from_handle(2));
    }
}
