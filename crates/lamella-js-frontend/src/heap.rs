//! The object heap: a realm that may live in flash, and everything a program allocated.

use crate::Vec;
use crate::object::Object;
use crate::value::ObjectId;

/// No RAM copy exists yet: this resident object is still being read from flash.
///
/// A sentinel rather than `Option<u32>` because it costs nothing either way here -- `u32::MAX`
/// cannot be a real slot, since a heap with that many objects has already exhausted the id space.
const NOT_PROMOTED: u32 = u32::MAX;

/// The object table: `ObjectId` resolution, and the write barrier for whole objects.
///
/// See the module header. The fields are private ON PURPOSE and that is load-bearing: the only way
/// out of here with a `&mut Object` is [`Self::get_mut`], which promotes.
pub(crate) struct Heap {
    /// The realm, emitted at build time and read where it lies.
    ///
    /// **EMPTY until the build-time tables exist**, which is the state every configuration is in
    /// today. With it empty every id falls through to [`Self::program`] and this whole type is the
    /// `Vec<Object>` it replaced -- which is what lets the addressing land, and be exercised by the
    /// entire suite, before the tables that fill it.
    resident: &'static [Object],
    /// For each resident object, the [`Self::copies`] slot that has superseded it, or
    /// [`NOT_PROMOTED`].
    ///
    /// Parallel to `resident` and the same length. Empty when `resident` is.
    promoted: Vec<u32>,
    /// The RAM copies of resident objects something has written. Nothing else lives here.
    ///
    /// Addressed only through `promoted`, so it may reallocate freely: a promotion records an
    /// INDEX, never a pointer.
    copies: Vec<Object>,
    /// Everything the running program allocated, at ids `resident.len()..`.
    program: Vec<Object>,
}

impl Heap {
    /// A heap with no flash-resident realm: every object is allocated in RAM.
    ///
    /// `hint` pre-sizes [`Self::program`]. Pre-sizing rather than letting it double is not a style
    /// choice -- section 9.3 measures that a block over `lamella-heap`'s 20,480-byte top class is
    /// carved from the frontier and never returns, and the object table's final doubling asked for
    /// 73,728 contiguous bytes having already abandoned 36,864.
    #[must_use]
    pub(crate) fn new(hint: usize) -> Self {
        Self {
            resident: &[],
            promoted: Vec::new(),
            copies: Vec::new(),
            program: Vec::with_capacity(hint),
        }
    }

    /// A heap whose realm is resident, holding ids `0..resident.len()`.
    ///
    /// NOTE: `promoted` is sized here and never grows, so it is the one allocation a resident realm
    /// costs -- four bytes per object, against the 128 a slot costs.
    ///
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub(crate) fn with_resident(resident: &'static [Object]) -> Self {
        Self {
            resident,
            promoted: crate::vec![NOT_PROMOTED; resident.len()],
            copies: Vec::new(),
            program: Vec::new(),
        }
    }

    /// How many objects are addressable: the resident realm plus what the program allocated.
    ///
    /// **NOT how many objects are in RAM**, which is [`Self::ram_objects`]. Keeping the two names
    /// apart is deliberate: the whole point of a resident realm is that those two numbers differ,
    /// and a residency figure that used this one would report no saving at all.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.resident.len() + self.program.len()
    }

    /// How many objects the realm holds in flash. Zero until the tables exist.
    #[must_use]
    pub(crate) fn resident_len(&self) -> usize {
        self.resident.len()
    }

    /// How many resident objects have been promoted -- the number the mutation census prices.
    #[must_use]
    pub(crate) fn promoted_len(&self) -> usize {
        self.copies.len()
    }

    /// How many `Object` structs are actually in RAM: the promoted copies and the program's own.
    #[must_use]
    pub(crate) fn ram_objects(&self) -> usize {
        self.copies.len() + self.program.len()
    }

    /// The bytes the RAM-side object tables have RESERVED, which is what the allocator was made to
    /// hand over. Capacity rather than length, for the reason
    /// [`Object::property_store_bytes`] gives.
    #[must_use]
    pub(crate) fn ram_table_bytes(&self) -> usize {
        (self.copies.capacity() + self.program.capacity()) * core::mem::size_of::<Object>()
            + self.promoted.capacity() * core::mem::size_of::<u32>()
    }

    /// Whether this id is still being read from flash. False for a program object, and false for a
    /// resident object that has been promoted.
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub(crate) fn is_resident(&self, id: ObjectId) -> bool {
        let index = id.0 as usize;
        index < self.resident.len() && self.promoted[index] == NOT_PROMOTED
    }

    /// Whether this id belongs to the realm rather than to the running program.
    ///
    /// A range check, for the reason `realm_boundary` is an index rather than a flag on every
    /// object: the realm is built first and the id space is ordered, so the question is a
    /// comparison.
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub(crate) fn is_realm(&self, id: ObjectId) -> bool {
        (id.0 as usize) < self.resident.len()
    }

    pub(crate) fn allocate(&mut self, object: Object) -> ObjectId {
        self.program.push(object);
        ObjectId((self.resident.len() + self.program.len() - 1) as u32)
    }

    /// Reading. Incapable of promoting anything, on either side of the split.
    #[must_use]
    pub(crate) fn get(&self, id: ObjectId) -> &Object {
        let index = id.0 as usize;
        if index < self.resident.len() {
            let slot = self.promoted[index];
            return if slot == NOT_PROMOTED {
                &self.resident[index]
            } else {
                &self.copies[slot as usize]
            };
        }
        &self.program[index - self.resident.len()]
    }

    /// Writing, and **the only way to obtain a `&mut Object` in the engine.**
    ///
    /// A resident object is copied into RAM here, once, and every later write finds the copy. See
    /// the module header for why the barrier is one object-level promotion rather than a wrapper
    /// around each of the twenty slots.
    pub(crate) fn get_mut(&mut self, id: ObjectId) -> &mut Object {
        let index = id.0 as usize;
        if index < self.resident.len() {
            let mut slot = self.promoted[index];
            if slot == NOT_PROMOTED {
                self.copies.push(self.resident[index].clone());
                slot = (self.copies.len() - 1) as u32;
                self.promoted[index] = slot;
            }
            return &mut self.copies[slot as usize];
        }
        &mut self.program[index - self.resident.len()]
    }

    /// Every addressable object in id order, resolving each id the way [`Self::get`] would.
    ///
    /// Used by the residency census, which must see the EFFECTIVE object for each id: a promoted
    /// realm object's copy is what costs RAM, and its flash original costs none.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Object> {
        (0..self.resident.len())
            .map(move |index| {
                let slot = self.promoted[index];
                if slot == NOT_PROMOTED {
                    &self.resident[index]
                } else {
                    &self.copies[slot as usize]
                }
            })
            .chain(self.program.iter())
    }

    /// Every object that is in RAM, which is what a residency figure counts.
    ///
    /// NOTE: distinct from [`Self::iter`], and the distinction is the measurement -- `iter` walks all
    /// 462 objects whether they are resident or not, and this walks only what was paid for.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn iter_ram(&self) -> impl Iterator<Item = &Object> {
        self.copies.iter().chain(self.program.iter())
    }

    /// Moves a realm that is entirely in RAM into leaked memory, and reopens this heap on top of it
    /// with **every id preserved**.
    ///
    /// # A TEST INSTRUMENT, AND IT DOES NOT MEASURE THE BYTE SAVING
    ///
    /// Leaking at run time does not put anything in flash: the objects are still in RAM, merely
    /// unreachable and unfreed. **So this proves the ADDRESSING and the BARRIER against the real
    /// 462-object realm, and says nothing about residency.** The bytes need the build-time tables,
    /// and a figure taken here would describe the instrument rather than the design.
    ///
    /// What it buys is the thing unit fixtures cannot. With no tables yet, `resident` is empty in
    /// every build anyone makes, so the promoting arm of [`Self::get_mut`] would otherwise be
    /// exercised only by two-object fixtures -- a guard running in no build anyone makes, with a
    /// silent-wrong-answer failure behind it. This runs REAL JavaScript through the resident path.
    ///
    /// **Ids are preserved because `program[i]` had id `i`** -- true only while `resident` is empty,
    /// which is asserted rather than assumed. Everything outside the heap that holds an `ObjectId`
    /// (the intrinsics table, every property value, the global scope) therefore needs no rewriting,
    /// and an id space that shifted here would silently alias one object onto another.
    ///
    /// **BOTH HALVES MUST BE LEAKED, BECAUSE A STRUCT IN FLASH WITH A `Vec` HANGING OFF IT IS TWO
    /// ALLOCATIONS** -- and where the struct lives says nothing about where the other one does.
    /// Leaking only the `Vec<Object>` leaves the whole property-store figure unmoved, and property
    /// stores are the dominant cause of the realm's size.
    /// See [`Object::make_stores_static_for_test`].
    #[cfg(test)]
    pub(crate) fn make_resident_by_leaking(&mut self) {
        assert!(self.resident.is_empty(), "the realm is already resident");
        assert!(self.copies.is_empty(), "there is nothing to promote from yet");
        let mut objects = core::mem::take(&mut self.program);
        let count = objects.len();
        for object in &mut objects {
            object.make_stores_static_for_test();
        }
        let leaked: &'static [Object] = crate::Box::leak(objects.into_boxed_slice());
        *self = Self::with_resident(leaked);
        assert_eq!(self.len(), count, "an id was lost or invented");
        assert_eq!(self.ram_objects(), 0, "the realm is still in RAM");
    }
}

#[cfg(test)]
mod the_id_mapping {
    use super::*;
    use crate::object::{Property, PropertyKey};
    use crate::value::JsValue;

    /// A resident realm as a GENUINE `static`, which is exactly what a build-time table emits.
    ///
    /// # THIS IS THE EVIDENCE THAT THE TABLES ARE POSSIBLE AT ALL
    ///
    /// Nothing carrying a `JsString` could appear in a `static` while its units were a `Vec`, so this
    /// fixture was a `Box::leak` and every claim about flash residency rested on the borrowed ARM
    /// being the same one a `static` would produce. It no longer has to: **this really is in
    /// `.rodata`**, and the assertions below read a realm out of it.
    ///
    /// `KIND` is the key text as UTF-16 code units, because a property key is a String value. Writing
    /// it as bytes would build a key that compares equal to nothing -- which is why the constructor is
    /// named `from_static_units` rather than `from_static_str`.
    mod flash {
        use crate::object::{Property, PropertyKey, ResidentObject};
        use crate::value::{JsValue, ObjectId};

        const KIND: &[u16] = &[b'k' as u16, b'i' as u16, b'n' as u16, b'd' as u16];

        pub(super) static FIRST: [(PropertyKey, Property); 1] =
            [(PropertyKey::from_static_units(KIND), Property::data(JsValue::Number(1.0)))];
        pub(super) static SECOND: [(PropertyKey, Property); 1] =
            [(PropertyKey::from_static_units(KIND), Property::data(JsValue::Number(2.0)))];

        /// The two objects, built from the flash tables above.
        ///
        /// # WARNING: THE STRUCTS ARE BUILT HERE AND THE TABLES ARE NOT, AND THAT IS FORCED
        ///
        /// `static REALM: [Object; 2]` **does not compile**, and no amount of const-constructibility
        /// changes it: a `static` must be `Sync`, and an `Object` holds `arguments` and `generator`
        /// slots that reach a `Scope` -- an `Rc<RefCell<Environment>>`. `Rc` is not `Sync` and
        /// `RefCell` is not either, so the TYPE is not, whatever a particular object contains. Every
        /// realm object has `None` in both slots and it makes no difference; `Sync` is a property of
        /// the type.
        ///
        /// The property TABLES have no such problem -- a `PropertyKey` and a `Property` reach only
        /// `JsString`, `SymbolId` and `ObjectId`, all of which are `Sync` -- so **they are genuinely
        /// in `.rodata` above**, which is where the realm's bytes are. The structs are the cheap half.
        pub(super) fn realm() -> &'static [super::Object] {
            crate::Box::leak(crate::Box::new([
                super::Object::resident(&ResidentObject {
                    prototype: None,
                    named: &FIRST,
                    elements: &[],
                    callable: None,
                    is_array: false,
                    extensible: true,
                    primitive: None,
                    date: None,
                    error: false,
                }),
                super::Object::resident(&ResidentObject {
                    prototype: Some(ObjectId(0)),
                    named: &SECOND,
                    elements: &[],
                    callable: None,
                    is_array: false,
                    extensible: true,
                    primitive: None,
                    date: None,
                    error: false,
                }),
            ]))
        }
    }

    fn resident_realm() -> &'static [Object] {
        flash::realm()
    }

    /// A key built at run time must equal the same text read out of `.rodata`, or every lookup
    /// against a flash realm misses and the engine reports every built-in as absent.
    ///
    /// This is the assertion `Store`'s hand-written `PartialEq` exists for. A derived one compares
    /// the ARM first, so it would answer false here -- and the failure would look like "the realm has
    /// no such property" rather than like a broken comparison.
    #[test]
    fn a_key_from_flash_equals_the_same_key_built_at_run_time() {
        let built = PropertyKey::from_str("kind");
        let (from_flash, _) = &flash::FIRST[0];
        assert_eq!(&built, from_flash, "an arm-sensitive comparison would answer false here");

        let object = &resident_realm()[0];
        assert!(object.own(&built).is_some(), "a run-time key could not find a flash one");
        assert_eq!(object.property_store_bytes(), 0, "the table is in flash, not in the arena");
    }

    /// The realm's property text really is UTF-16 in `.rodata`, and a generator has to emit it that
    /// way -- six `u16`s for `"length"`, not six bytes.
    #[test]
    fn the_key_text_in_flash_is_utf16_code_units() {
        let (PropertyKey::String(text), _) = &flash::FIRST[0] else { panic!("not a string key") };
        assert_eq!(text.units(), &[0x6B, 0x69, 0x6E, 0x64], "k i n d as code units");
        assert_eq!(text.len(), 4);
        assert_eq!(text, &crate::string_value::JsString::from("kind"));
    }

    /// The point of the whole design: a resident realm occupies no `Object` slot in RAM.
    #[test]
    fn a_resident_realm_costs_no_object_slots() {
        let heap = Heap::with_resident(resident_realm());
        assert_eq!(heap.len(), 2, "both objects are addressable");
        assert_eq!(heap.ram_objects(), 0, "and neither is in RAM");
        assert_eq!(heap.resident_len(), 2);
        assert_eq!(heap.promoted_len(), 0);
    }

    /// Reading resolves through the table and must not promote -- on any path.
    #[test]
    fn reading_a_resident_object_does_not_promote() {
        let heap = Heap::with_resident(resident_realm());
        assert!(heap.is_resident(ObjectId(0)));
        assert_eq!(
            heap.get(ObjectId(1)).own(&PropertyKey::from_str("kind")).unwrap().data_value(),
            Some(&JsValue::Number(2.0))
        );
        assert_eq!(heap.get(ObjectId(1)).prototype, Some(ObjectId(0)));
        assert_eq!(heap.iter().count(), 2);
        assert!(heap.is_resident(ObjectId(0)) && heap.is_resident(ObjectId(1)), "a read promoted");
        assert_eq!(heap.ram_objects(), 0);
    }

    /// THE FAILURE THIS EXISTS TO PREVENT, AND THE ONE `Store` DOES NOT COVER: a write to a
    /// slot that is not a property store. `prototype` is written through a `&mut Object`, and on a
    /// flash object with no barrier here it would be discarded in silence.
    #[test]
    fn writing_a_non_property_slot_promotes_and_the_write_sticks() {
        let mut heap = Heap::with_resident(resident_realm());
        heap.get_mut(ObjectId(1)).prototype = None;

        assert!(!heap.is_resident(ObjectId(1)), "the write did not promote");
        assert_eq!(heap.get(ObjectId(1)).prototype, None, "the store did not stick");
        assert_eq!(heap.promoted_len(), 1);
        assert!(heap.is_resident(ObjectId(0)), "promotion spread to a neighbour");
    }

    /// Every one of the nineteen non-property slots is reached the same way, so one is not evidence
    /// about the others by construction -- but a slot NOBODY promotes through is. This asserts the
    /// barrier for a spread of them rather than for the one that was convenient.
    #[test]
    fn the_other_slots_promote_too() {
        for (name, write) in [
            ("extensible", (|o: &mut Object| o.extensible = false) as fn(&mut Object)),
            ("primitive", |o: &mut Object| o.primitive = Some(JsValue::Number(7.0))),
            ("is_array", |o: &mut Object| o.is_array = true),
            ("error", |o: &mut Object| o.error = true),
            ("date", |o: &mut Object| o.date = Some(0.0)),
            ("callable", |o: &mut Object| {
                o.callable = Some(crate::object::Callable::Native(0));
            }),
        ] {
            let mut heap = Heap::with_resident(resident_realm());
            write(heap.get_mut(ObjectId(0)));
            assert!(!heap.is_resident(ObjectId(0)), "writing {name} did not promote");
        }
    }

    /// THE COMPOSITION WITH `Store`, AND IT IS THE DIFFERENCE BETWEEN PROMOTING AN OBJECT AND
    /// PROMOTING A REALM. A `prototype` write must not drag the property table into RAM with it:
    /// cloning a `Store::Static` copies the flash reference, not the properties.
    #[test]
    fn promotion_keeps_the_property_stores_in_flash() {
        let mut heap = Heap::with_resident(resident_realm());
        heap.get_mut(ObjectId(0)).extensible = false;

        let promoted = heap.get(ObjectId(0));
        assert_eq!(promoted.property_store_bytes(), 0, "promoting an object copied its properties");
        assert!(promoted.own(&PropertyKey::from_str("kind")).is_some());

        heap.get_mut(ObjectId(0))
            .set_own(PropertyKey::from_str("kind"), Property::data(JsValue::Number(9.0)));
        assert!(heap.get(ObjectId(0)).property_store_bytes() > 0);
    }

    /// IDENTITY SURVIVES PROMOTION, WHICH IS WHY THE TABLE IS AN INDIRECTION AND NOT A POINTER.
    /// `Array.prototype.values === Array.prototype[Symbol.iterator]` is the requirement: two
    /// properties holding one id must still resolve to ONE object afterwards.
    ///
    /// NOTE: the id comparison is the WEAK half and is trivially preserved -- the ids are never
    /// rewritten, which is the point of the indirection. **The substantive check is that both ids
    /// resolve to the same storage**, because a design promoting into two copies would keep `===`
    /// true and lose the write, which is the silent shape rather than the loud one.
    #[test]
    fn two_references_to_one_id_stay_one_object_across_promotion() {
        let mut heap = Heap::with_resident(resident_realm());
        let holder = heap.allocate(Object::new(None));
        for key in ["values", "iterator"] {
            heap.get_mut(holder)
                .set_own(PropertyKey::from_str(key), Property::data(JsValue::Object(ObjectId(0))));
        }
        let read = |heap: &Heap, key: &str| match heap
            .get(holder)
            .own(&PropertyKey::from_str(key))
            .unwrap()
            .data_value()
        {
            Some(JsValue::Object(id)) => *id,
            other => panic!("not an object: {other:?}"),
        };
        assert_eq!(read(&heap, "values"), read(&heap, "iterator"), "=== was false before");
        assert!(core::ptr::eq(heap.get(read(&heap, "values")), heap.get(read(&heap, "iterator"))));

        heap.get_mut(read(&heap, "values"))
            .set_own(PropertyKey::from_str("mark"), Property::data(JsValue::Number(5.0)));

        assert_eq!(read(&heap, "values"), read(&heap, "iterator"), "=== stopped holding");
        assert!(
            core::ptr::eq(heap.get(read(&heap, "values")), heap.get(read(&heap, "iterator"))),
            "one id resolved to two objects -- promotion split it in two"
        );
        let through_the_other = heap.get(read(&heap, "iterator"));
        assert!(
            through_the_other.own(&PropertyKey::from_str("mark")).is_some(),
            "the second reference still sees the FLASH object -- the write was lost through it"
        );
        assert_eq!(heap.promoted_len(), 1, "one object, one copy");
    }

    /// Promotion happens once. A second write must find the copy rather than re-clone from flash,
    /// which would be correct and would silently discard the first write.
    #[test]
    fn promotion_happens_once_and_the_first_write_survives_the_second() {
        let mut heap = Heap::with_resident(resident_realm());
        heap.get_mut(ObjectId(0)).extensible = false;
        heap.get_mut(ObjectId(0)).is_array = true;

        assert_eq!(heap.promoted_len(), 1, "the object was cloned twice");
        assert!(!heap.get(ObjectId(0)).extensible, "the first write was discarded by the second");
        assert!(heap.get(ObjectId(0)).is_array);
    }

    /// Program ids start ABOVE the resident realm, and the two spaces must not overlap -- an
    /// overlap would make one id name two objects, which is the id-mapping equivalent of one
    /// property having two homes.
    #[test]
    fn program_ids_start_above_the_resident_realm() {
        let mut heap = Heap::with_resident(resident_realm());
        let first = heap.allocate(Object::new(None));
        let second = heap.allocate(Object::new(None));
        assert_eq!(first, ObjectId(2), "a program object landed on a realm id");
        assert_eq!(second, ObjectId(3));
        assert_eq!(heap.len(), 4);
        assert_eq!(heap.ram_objects(), 2);
        assert!(!heap.is_realm(first));
        assert!(heap.is_realm(ObjectId(1)));

        heap.get_mut(ObjectId(0)).extensible = false;
        assert_eq!(heap.len(), 4, "promotion changed the addressable count");
        assert_eq!(heap.allocate(Object::new(None)), ObjectId(4));
    }

    /// With no resident realm this type is the `Vec<Object>` it replaced, which is what lets it
    /// land before the tables. Every id is a program id and nothing can promote.
    #[test]
    fn an_empty_resident_realm_is_the_vec_it_replaced() {
        let mut heap = Heap::new(4);
        let first = heap.allocate(Object::new(None));
        assert_eq!(first, ObjectId(0));
        assert_eq!(heap.len(), 1);
        assert_eq!(heap.ram_objects(), 1);
        assert!(!heap.is_resident(first));
        assert!(!heap.is_realm(first));
        assert_eq!(heap.promoted_len(), 0);
        heap.get_mut(first).extensible = false;
        assert_eq!(heap.promoted_len(), 0, "a program object was promoted");
        assert!(!heap.get(first).extensible);
    }

    /// The census walks EFFECTIVE objects, and after a promotion the copy is the effective one.
    /// Reading the flash original there would report the realm as unwritten forever.
    #[test]
    fn iteration_yields_the_promoted_copy_rather_than_the_flash_original() {
        let mut heap = Heap::with_resident(resident_realm());
        heap.get_mut(ObjectId(0)).is_array = true;
        let arrays = heap.iter().filter(|object| object.is_array).count();
        assert_eq!(arrays, 1, "iteration read the flash original");
        assert_eq!(heap.iter().count(), 2);
        assert_eq!(heap.iter_ram().count(), 1);
    }

    /// The indirection is one `u32` per resident object, and that figure is the design's price.
    #[test]
    fn the_promotion_table_is_four_bytes_per_resident_object() {
        let heap = Heap::with_resident(resident_realm());
        assert_eq!(core::mem::size_of::<u32>(), 4);
        assert_eq!(heap.ram_table_bytes(), heap.promoted.capacity() * 4);
        assert!(heap.promoted.capacity() >= 2);
    }

    /// The trade this design makes, asserted rather than described: an id resolves through a table
    /// entry that is a small fraction of the slot it replaces. If `Object` ever shrank to near a
    /// `u32` the indirection would stop paying, and that is worth being told about.
    #[test]
    fn a_slot_costs_far_more_than_an_entry_in_the_table() {
        let slot = core::mem::size_of::<Object>();
        let entry = core::mem::size_of::<u32>();
        assert!(slot >= 32 * entry, "an Object is {slot} bytes and a table entry is {entry}");
    }
}

/// THE ADDRESSING AND THE BARRIER, AGAINST THE **REAL** REALM RATHER THAN A FIXTURE.
///
/// The module above tests two-object fixtures, and a two-object fixture has almost nothing for a
/// defect to damage. Worse, `resident` is empty in every build that exists, so without something
/// here the promoting arm of [`Heap::get_mut`] would run in NO build anyone makes while several
/// thousand tests reported green.
///
/// So these run real JavaScript through a realm made resident by leaking, and compare it against the
/// same programs on an ordinary interpreter. **The comparison is the instrument**: a differential
/// against the engine's own ordinary path, over programs chosen because they WRITE the realm.
///
/// It proves correctness and counts promotions. It does NOT measure bytes -- see
/// [`Heap::make_resident_by_leaking`].
#[cfg(test)]
mod a_resident_realm_runs_real_programs {
    use crate::interpreter::Interpreter;
    use crate::value::JsValue;

    /// An interpreter whose 462-object realm is resident, and whose ids are unchanged.
    fn resident() -> Interpreter {
        let mut interpreter = Interpreter::new();
        interpreter.make_realm_resident_by_leaking();
        interpreter
    }

    /// Programs whose ANSWER is a primitive, so the comparison is about the language rather than
    /// about object ids. Every one after the first four writes the realm.
    const PROBES: &[&str] = &[
        "var x = 1; x + 41",
        "[3,1,2].sort().join(',')",
        "'abc'.replace('b', '[$&]')",
        "JSON.stringify({ a: [1, 2], b: 'c' })",
        "Math.max(1, 2, 3)",
        "new Map([[1, 2]]).get(1)",
        "/a+/.exec('caaat')[0]",
        "[1,2,3].map(function (v) { return v * 2; }).join('')",
        "try { null.x } catch (e) { e.constructor.name }",
        "typeof Object.getOwnPropertyDescriptor(Array.prototype, 'join').value",
        "Array.prototype.values === Array.prototype[Symbol.iterator]",
        "Array.prototype.mine = 7; [].mine",
        "delete Object.prototype.toString; typeof ({}).toString",
        "Object.freeze(Math); Math.nope = 1; typeof Math.nope",
        "Object.defineProperty(Number.prototype, 'half', { get: function () { return 0.5; } }); (8).half",
        "Object.setPrototypeOf(Array.prototype, null); Object.getPrototypeOf(Array.prototype)",
        "Object.preventExtensions(Date.prototype); Object.isExtensible(Date.prototype)",
        "Array.prototype.x = 1; Array.prototype.values === Array.prototype[Symbol.iterator]",
    ];

    /// THE REALM STAYS INSIDE WHAT A FLASH DESCRIPTOR CAN EXPRESS, AND THIS IS THE GUARD FOR WORK
    /// THAT HAS NOT BEEN WRITTEN YET.
    ///
    /// `ResidentObject` names nine fields and deliberately cannot express the boxed exotic slots --
    /// an iterator's state, a collection, a promise, a proxy pair, a compiled pattern -- because a
    /// fresh realm has `None` in every one of them. **The day a built-in acquires one, a generator
    /// emitting descriptors would drop it in silence**, and the symptom would be a built-in that has
    /// lost its brand rather than a build that failed.
    ///
    /// The same holds for property VALUES: the emitter has to have an arm per `JsValue` variant, and a
    /// variant nobody knew was in the realm is a variant nobody wrote an arm for. Measured today --
    /// 462 objects, 1,429 properties, 1,389 string keys and 40 symbol keys, 465 object values, 452
    /// numbers, 452 strings, 31 accessors, 16 booleans, 12 symbols, 1 undefined, no nulls; 414
    /// callable objects, 3 with a boxed primitive, 1 array, 1 non-extensible, 2 with no prototype, and
    /// **no `date` and no `error`**.
    ///
    /// This asserts the part that would go wrong quietly rather than the counts, which move whenever a
    /// built-in lands and would make this a test about the calendar.
    #[test]
    fn no_realm_object_needs_a_slot_a_flash_descriptor_cannot_carry() {
        let interpreter = Interpreter::new();
        let count = interpreter.realm_census().objects;
        for id in 0..count {
            let object = interpreter.object(crate::value::ObjectId(id as u32));
            let unexpressable = [
                ("iterator_state", object.iterator_state.is_some()),
                ("collection", object.collection.is_some()),
                ("binary", object.binary.is_some()),
                ("promise", object.promise.is_some()),
                ("resolver", object.resolver.is_some()),
                ("combinator", object.combinator.is_some()),
                ("capability_parts", object.capability_parts.is_some()),
                ("arguments", object.arguments.is_some()),
                ("proxy", object.proxy.is_some()),
                ("generator", object.generator.is_some()),
                ("regexp", object.regexp.is_some()),
            ];
            for (slot, present) in unexpressable {
                assert!(
                    !present,
                    "realm object {id} holds `{slot}`, which a flash descriptor cannot carry -- \
                     widen `ResidentObject` before generating tables, or the slot is lost silently"
                );
            }
        }
    }

    /// A callable realm object reaches its body through an INDEX, never a function pointer, and that
    /// is what lets the descriptors be constant data at all.
    ///
    /// A `fn` pointer in a generated table would have to be a NAMEABLE path, and the engine's built-in
    /// bodies are items inside `builtins.rs` that no outside path reaches. `Callable::Native(u32)` is
    /// a number, so the descriptor carries it and the native TABLE stays something `install`
    /// registers. If a realm object ever carried a body directly, generation would stop being
    /// possible rather than become harder.
    #[test]
    fn a_callable_realm_object_carries_an_index_and_not_a_function_pointer() {
        use crate::object::Callable;
        let interpreter = Interpreter::new();
        let count = interpreter.realm_census().objects;
        let mut callables = 0;
        for id in 0..count {
            let Some(callable) = interpreter.object(crate::value::ObjectId(id as u32)).callable
            else {
                continue;
            };
            callables += 1;
            assert!(
                matches!(callable, Callable::Native(_) | Callable::Closure(_)),
                "realm object {id} is callable as {callable:?}, which a descriptor cannot express"
            );
        }
        assert!(callables > 400, "only {callables} callable objects -- the realm shrank");
    }

    /// The differential: a resident realm must compute exactly what a RAM realm computes.
    #[test]
    fn every_probe_answers_the_same_as_an_ordinary_realm() {
        for source in PROBES {
            let ordinary = Interpreter::new().eval_source(source);
            let resident = resident().eval_source(source);
            assert_eq!(
                format!("{ordinary:?}"),
                format!("{resident:?}"),
                "a resident realm answered differently for `{source}`"
            );
        }
    }

    /// AND THE DIFFERENTIAL ALONE IS NOT ENOUGH, BECAUSE TWO IDENTICAL FAILURES SCORE AS
    /// AGREEMENT. Every probe must also produce the answer the language specifies, or a resident
    /// realm that computed nothing at all would pass the test above against an ordinary realm that
    /// had broken the same way. Named rather than generated, so a wrong shared answer is visible.
    #[test]
    fn every_probe_answers_what_the_language_says() {
        let expected: &[&str] = &[
            "Ok(Number(42.0))",
            "Ok(String(JsString([49, 44, 50, 44, 51])))",
            "Ok(String(JsString([97, 91, 98, 93, 99])))",
            "Ok(String(JsString([123, 34, 97, 34, 58, 91, 49, 44, 50, 93, 44, 34, 98, 34, 58, 34, 99, 34, 125])))",
            "Ok(Number(3.0))",
            "Ok(Number(2.0))",
            "Ok(String(JsString([97, 97, 97])))",
            "Ok(String(JsString([50, 52, 54])))",
            "Ok(String(JsString([84, 121, 112, 101, 69, 114, 114, 111, 114])))",
            "Ok(String(JsString([102, 117, 110, 99, 116, 105, 111, 110])))",
            "Ok(Boolean(true))",
            "Ok(Number(7.0))",
            "Ok(String(JsString([117, 110, 100, 101, 102, 105, 110, 101, 100])))",
            "Ok(String(JsString([117, 110, 100, 101, 102, 105, 110, 101, 100])))",
            "Ok(Number(0.5))",
            "Ok(Null)",
            "Ok(Boolean(false))",
            "Ok(Boolean(true))",
        ];
        assert_eq!(PROBES.len(), expected.len(), "a probe was added without its expected answer");
        for (source, want) in PROBES.iter().zip(expected) {
            let got = format!("{:?}", resident().eval_source(source));
            assert_eq!(&got, want, "wrong answer for `{source}`");
        }
    }

    /// A fresh resident realm holds no `Object` in RAM at all, which is the state the whole design
    /// exists to reach.
    #[test]
    fn a_fresh_resident_realm_has_nothing_in_ram() {
        let interpreter = resident();
        let census = interpreter.realm_census();
        assert!(census.resident_objects > 400, "the realm is {} objects", census.resident_objects);
        assert_eq!(census.objects, census.resident_objects, "a realm object stayed in RAM");
        assert_eq!(census.promoted_objects, 0);
        assert_eq!(census.object_struct_bytes, 0, "an object slot was paid for");
        assert_eq!(census.property_store_bytes, 0, "a property store was paid for");
        assert_eq!(
            census.object_table_bytes,
            census.resident_objects * core::mem::size_of::<u32>(),
            "a resident realm costs the promotion table and nothing more"
        );
    }

    /// COUNTS PROMOTIONS RATHER THAN `&mut` HAND-OUTS, WHICH IS THE QUANTITY THAT ACTUALLY COSTS
    /// RAM. A `&mut` is not proof of a write, so counting hand-outs over-counts by construction; a
    /// promotion is the copy itself.
    ///
    /// **Every probe promotes 0 or 1, and the only ones that promote are the ones that write.** The
    /// bound is therefore 1 rather than a comfortable round number -- an assertion loose enough to
    /// pass whatever happens is not a measurement.
    #[test]
    fn no_probe_promotes_more_than_one_realm_object() {
        for source in PROBES {
            let mut interpreter = resident();
            interpreter.eval_source(source).expect("the probe runs");
            let promoted = interpreter.realm_census().promoted_objects;
            assert!(promoted <= 1, "`{source}` promoted {promoted} realm objects");
        }
    }

    /// WHAT `globalThis` BEING RAM BY CONSTRUCTION ACTUALLY COSTS, WHICH NOBODY HAD PRICED.
    ///
    /// The global is written by EVERY run -- declaring a variable writes it -- so it is never a
    /// candidate for the static side. **A constraint with no number beside it invites the reading
    /// that the global drags the realm into RAM with it.**
    ///
    /// **It does not.** `var x = 1` promotes exactly ONE object, and its property store is a small
    /// fraction of the realm's. The comparison is computed here rather than written down, so it
    /// cannot go stale as the realm grows.
    ///
    /// # THE BASELINE IS A REALM THE INSTALLERS BUILT
    ///
    /// `Interpreter::new` reports **zero** property-store bytes: a fresh realm's properties are
    /// read from the generated tables rather than allocated. So the quantity worth comparing
    /// against is what the realm costs when it IS built, which is what
    /// `Interpreter::with_installed_realm` produces.
    #[test]
    fn the_global_is_the_one_object_every_program_promotes_and_it_is_cheap() {
        let whole_realm =
            Interpreter::with_installed_realm().realm_census().property_store_bytes;
        assert!(whole_realm > 0, "a realm built by the installers holds its properties in RAM");
        assert_eq!(
            Interpreter::new().realm_census().property_store_bytes,
            0,
            "a table-built realm allocated property storage"
        );

        let mut interpreter = resident();
        interpreter.eval_source("var x = 1; x + 1").expect("the probe runs");
        let census = interpreter.realm_census();

        assert_eq!(census.promoted_objects, 1, "declaring a variable promoted more than the global");
        assert!(
            census.property_store_bytes * 10 < whole_realm,
            "the global's store is {} of the realm's {whole_realm} -- it is dragging the realm in",
            census.property_store_bytes
        );
    }

    /// Writing one realm object promotes ONE, rather than the realm. A design that promoted eagerly
    /// -- a whole prototype chain, say -- would be correct and would buy nothing.
    #[test]
    fn writing_one_realm_object_promotes_one() {
        let mut interpreter = resident();
        let before = interpreter.realm_census().promoted_objects;
        interpreter.eval_source("Array.prototype.mine = 7").expect("the probe runs");
        let after = interpreter.realm_census().promoted_objects;
        assert_eq!(after - before, 1, "promoted {} objects for one write", after - before);
    }

    /// Program objects land ABOVE the realm and at the SAME ids either way, which is what makes the
    /// leaked instrument a differential rather than an approximation.
    #[test]
    fn a_program_object_gets_the_same_id_on_both() {
        let ordinary = {
            let mut interpreter = Interpreter::new();
            interpreter.eval_source("({})").expect("runs")
        };
        let resident = {
            let mut interpreter = resident();
            interpreter.eval_source("({})").expect("runs")
        };
        match (ordinary, resident) {
            (JsValue::Object(a), JsValue::Object(b)) => {
                assert_eq!(a, b, "the id spaces diverged");
                assert!(a.0 > 400, "a program object landed inside the realm at {}", a.0);
            }
            other => panic!("not two objects: {other:?}"),
        }
    }
}
