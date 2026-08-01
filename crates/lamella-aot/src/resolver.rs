//! A [`CallResolver`] backed by a compiled assembly's metadata.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use lamella_cil::Operand;
use lamella_ir::{Function, MirType, StaticOwner, TypeHandle};
use lamella_metadata::tables::table;
use lamella_metadata::{
    Assembly, CharSet, Method, MethodKind, ResolvedMethod, SigType, TargetLayout, TypeDef,
    TypeName, exception_tag_for_name, fnv1a32,
};
use lamella_token::Token;

use crate::cil::{
    Array2DOp, ArrayElement, ArrayMDOp, CallInfo, CallResolver, CallTarget, CilError, Intrinsic,
    PInvokeCall, ReferenceLayout, lower_method_typed,
};

/// Reference-owned [`TypeHandle`]s ride table byte 0x03: a handle only ever carries a TypeRef
/// (0x01) or TypeDef (0x02) token today, so 0x03 marks "a REFERENCE's TypeDef" with the
/// reference ordinal in bits 20..23 and the owning row in bits 0..19 -- the handle itself says
/// WHICH reference owns the row, so two references' equal rows stay distinct identities end to
/// end, descriptor symbols included. This-assembly handles remain raw tokens. The whole encoding
/// stays under bit 27, clear of every backend symbol FLAG (`DESC_SYMBOL_FLAG` is bit 30 -- a
/// table-byte scheme above 0x07 would alias the flags when a handle rides a descriptor
/// reference word). Capacity: 16 references, 2^20 rows each -- both loudly asserted at mint.
pub const REFERENCE_HANDLE_TABLE: u32 = 0x03;
const REFERENCE_ORDINAL_SHIFT: u32 = 20;
const REFERENCE_ROW_MASK: u32 = 0x000f_ffff;

/// Decodes a reference-owned handle to `(reference ordinal, owning-assembly TypeDef token)`,
/// or `None` for a this-assembly handle.
#[must_use]
pub fn reference_handle_parts(handle: TypeHandle) -> Option<(usize, u32)> {
    (handle.0 >> 24 == REFERENCE_HANDLE_TABLE).then_some((
        ((handle.0 >> REFERENCE_ORDINAL_SHIFT) & 0xf) as usize,
        ((table::TYPE_DEF as u32) << 24) | (handle.0 & REFERENCE_ROW_MASK),
    ))
}

/// The qualified handle for `ordinal`'s type at `type_def_token` (see
/// [`REFERENCE_HANDLE_TABLE`]). Panics past the encoding's capacity rather than aliasing.
fn reference_handle(ordinal: usize, type_def_token: u32) -> TypeHandle {
    let row = type_def_token & 0x00ff_ffff;
    assert!(
        ordinal < 16 && row <= REFERENCE_ROW_MASK,
        "reference handle out of encoding range (ordinal {ordinal}, row {row})"
    );
    TypeHandle(
        (REFERENCE_HANDLE_TABLE << 24) | ((ordinal as u32) << REFERENCE_ORDINAL_SHIFT) | row,
    )
}

/// How descriptor SYMBOLS qualify by their OWNING assembly -- the N-reference identity scheme,
/// shared by every object-emitting backend (ARM32 and RISC-V name descriptors identically, so a
/// mixed-provenance link dedupes one canonical symbol per type). A build's OWN descriptors take
/// `own` (a library passes its fnv1a32 hash, the same identity its `L<hash>.` function prefix and
/// `__lamella_statics_<hash>` region use; a program stays unqualified); a REFERENCE-owned handle
/// (see [`REFERENCE_HANDLE_TABLE`]) takes `references[ordinal]`. Both sides of one type thus emit
/// the SAME symbol (the program's strong synthesized copy dedupes against the library's weak own
/// copy, identity-by-address preserved), while two DIFFERENT types never share a descriptor symbol.
#[derive(Default)]
pub struct DescQualifiers {
    /// The fnv1a32 hash (8 lowercase hex) qualifying THIS build's own descriptors; `None` = the
    /// program convention (plain `__lamella_typedesc_<token>`).
    pub own: Option<String>,
    /// Ordinal-indexed reference hashes, exactly the resolver's reference order.
    pub references: Vec<String>,
    /// `System.String`'s handle, when this build can name the type -- so a string LITERAL can be laid
    /// with an object header (`[obj - 4]` = the descriptor) like any other object.
    ///
    /// It rides here rather than as another parameter because it is descriptor IDENTITY, which is
    /// what this struct already carries to every object-emitting path. `None` (a build with no
    /// resolvable `System.String`) lays literals exactly as before.
    pub string: Option<u32>,
}

/// The canonical symbol for `handle`'s descriptor under `qualifiers` (see [`DescQualifiers`]).
/// A reference-owned handle names the OWNER's token, so the symbol matches what the owning
/// library's own build emits. Panics on a reference handle with no attached hash -- that is a
/// build wiring bug, never a program's fault.
pub(crate) fn descriptor_symbol(handle: u32, qualifiers: &DescQualifiers) -> String {
    if let Some((ordinal, owner_token)) = reference_handle_parts(TypeHandle(handle)) {
        let hash = qualifiers
            .references
            .get(ordinal)
            .unwrap_or_else(|| panic!("reference ordinal {ordinal} has no descriptor qualifier"));
        alloc::format!("{}{}_{}", lamella_elf::TYPE_DESC_PREFIX, hash, owner_token)
    } else if let Some(own) = &qualifiers.own {
        alloc::format!("{}{}_{}", lamella_elf::TYPE_DESC_PREFIX, own, handle)
    } else {
        alloc::format!("{}{}", lamella_elf::TYPE_DESC_PREFIX, handle)
    }
}

/// A type's namespace-qualified name: `Ns.Name`, or the bare name when the namespace is empty.
///
/// This is the text .NET's `Object.ToString()` produces -- it returns `GetType().ToString()`, which is
/// the FULL name -- so a global-namespace `Square` renders as `Square` and not as `.Square`.
pub(crate) fn qualified_type_name(namespace: &str, name: &str) -> alloc::boxed::Box<str> {
    if namespace.is_empty() {
        alloc::boxed::Box::from(name)
    } else {
        alloc::format!("{namespace}.{name}").into_boxed_str()
    }
}

/// Resolves an assembly's `call` and `ldstr` tokens against its metadata.
pub struct MetadataResolver<'a> {
    assembly: &'a Assembly<'a>,
    /// The REFERENCED assemblies, in reference order, for cross-assembly vtable-slot agreement:
    /// with them, a type extending a referenced base numbers its slots INCLUDING the base's
    /// inherited virtuals (as the referenced assembly itself numbers them), and a `callvirt` on a
    /// `MemberRef` resolves to that shared slot. Without any, numbering stays this-assembly-relative
    /// (self-consistent, but a referenced type's virtual dispatched on a this-assembly object
    /// static-devirtualizes). Resolution is BY NAME, first reference wins -- the same order the
    /// build was handed the assemblies (corlib first by convention).
    references: Vec<&'a Assembly<'a>>,
    /// For module lowering: each callee's `MethodDef` rid paired with its function index in
    /// the module. Empty for single-method lowering, where a call keeps its rid (a one-
    /// function lowering does not dispatch internal calls anyway).
    rid_to_index: Vec<(u32, u32)>,
}

impl<'a> MetadataResolver<'a> {
    /// Wraps an assembly to resolve the tokens of a single method (no inter-method calls).
    #[must_use]
    pub fn new(assembly: &'a Assembly<'a>) -> MetadataResolver<'a> {
        MetadataResolver {
            assembly,
            references: Vec::new(),
            rid_to_index: Vec::new(),
        }
    }

    /// Attaches a referenced assembly (corlib, or a further library), enabling cross-assembly slot
    /// numbering: inherited referenced-base virtuals occupy their referenced-assembly slots (filled
    /// with extern entries a library object exports), and `virtual_slot` resolves a `MemberRef`
    /// against that numbering. Repeated calls APPEND -- references resolve by name in the order
    /// attached.
    #[must_use]
    pub fn with_reference(mut self, reference: &'a Assembly<'a>) -> MetadataResolver<'a> {
        self.references.push(reference);
        self
    }

    /// [`with_reference`](Self::with_reference) for a whole reference list at once (the
    /// multi-assembly deploy shape: corlib + System.Device + a BSP + ...), preserving order.
    #[must_use]
    pub fn with_references(mut self, references: &[&'a Assembly<'a>]) -> MetadataResolver<'a> {
        self.references.extend_from_slice(references);
        self
    }

    /// The attached reference list, in order -- the object-build typing path threads it into the
    /// value-type/enum resolution family so a cross-assembly `ValueType` TypeRef (e.g. a driver
    /// method's `AdcChannelMode` parameter) resolves to its owning assembly before it is laid out.
    pub(crate) fn references(&self) -> &[&'a Assembly<'a>] {
        &self.references
    }

    /// The first attached reference declaring `namespace.name`, with its ordinal and `TypeDef` --
    /// the one cross-assembly name-resolution rule every consumer shares. Reference ORDER is the
    /// tie-break (a name declared by two references resolves to the earlier one, exactly as the
    /// build was handed them).
    fn find_reference_type(
        &self,
        namespace: &str,
        name: &str,
    ) -> Option<(usize, &'a Assembly<'a>, TypeDef<'a>)> {
        self.references.iter().enumerate().find_map(|(ordinal, reference)| {
            reference
                .find_type(namespace, name)
                .map(|td| (ordinal, *reference, td))
        })
    }

    /// The IDENTITY handle for a type token: a this-assembly TypeDef keeps its raw token, and a
    /// TypeRef that resolves through the attached references becomes the REFERENCE-OWNED handle
    /// (the 0x03 encoding: ordinal + the owner's TypeDef row) -- so every consumer of the type's
    /// descriptor (an `Alloc`, a `box`, a cast target's compare, an `unbox.any` check) names the
    /// OWNER's canonical descriptor, and one type stays ONE identity across the link. Without
    /// this, a `castclass`/`isinst`/`unbox.any` against a referenced type minted a 0x01
    /// TypeRef-keyed handle whose minimal descriptor was a SECOND identity: the address compare
    /// against a cross-assembly-allocated instance's (owner-keyed) header descriptor could never
    /// match. A TypeRef the references do not resolve keeps its raw token -- the reference-less
    /// single-assembly behavior, where the 0x01-keyed descriptor is the only identity anyone
    /// mints and every compare is self-consistent.
    fn qualified_type_handle(&self, token: Token) -> TypeHandle {
        if token.table() == table::TYPE_REF {
            if let Some(name) = self.assembly.type_token_name(token) {
                if let Some((ordinal, _, ref_td)) =
                    self.find_reference_type(name.namespace, name.name)
                {
                    return reference_handle(ordinal, ref_td.token().0);
                }
            }
        }
        TypeHandle(token.0)
    }

    /// Wraps an assembly to resolve calls among the methods of a module: `method_rids` are
    /// their `MethodDef` rids in lowering order, so a call between them resolves to the
    /// callee's function index (what [`crate::cil::CallTarget::Internal`] names).
    #[must_use]
    pub fn for_module(assembly: &'a Assembly<'a>, method_rids: &[u32]) -> MetadataResolver<'a> {
        let rid_to_index = method_rids
            .iter()
            .enumerate()
            .map(|(index, &rid)| (rid, index as u32))
            .collect();
        MetadataResolver {
            assembly,
            references: Vec::new(),
            rid_to_index,
        }
    }

    /// Maps a callee's `MethodDef` rid to its function index in the module, or passes the rid
    /// through for single-method lowering. `None` if the call names a method outside the
    /// module being lowered.
    fn function_index(&self, rid: u32) -> Option<u32> {
        if self.rid_to_index.is_empty() {
            Some(rid)
        } else {
            self.rid_to_index
                .iter()
                .find(|&&(r, _)| r == rid)
                .map(|&(_, index)| index)
        }
    }

    /// The `TypeDef` a `newobj` constructs, from its constructor token: the constructor's
    /// declaring type, found by name. Shared by the value-type and reference-type resolutions.
    fn newobj_type_def(&self, operand: &Operand) -> Option<TypeDef<'a>> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let declaring = self.assembly.resolve_method(*token)?.declaring_type?;
        self.assembly.find_type(declaring.namespace, declaring.name)
    }

    /// Whether `type_def` is a delegate -- its `extends` chain reaches `System.MulticastDelegate` (or
    /// `System.Delegate`). The bounded base-chain walk the catch-type and cast detection also use.
    /// The type token a metadata token names: a type token as-is (`TypeRef`/`TypeDef`/
    /// `TypeSpec`), or the declaring type of a constructor token -- a `MemberRef`'s parent (an
    /// external type like `System.Exception`), or a `MethodDef`'s owning type resolved by name
    /// (a this-module exception). `None` for any other token.
    fn type_token_of(&self, token: Token) -> Option<Token> {
        match token.table() {
            table::TYPE_REF | table::TYPE_DEF | table::TYPE_SPEC => Some(token),
            table::MEMBER_REF => Some(self.assembly.member_ref(token.row())?.parent()),
            table::METHOD_DEF => {
                let name = self.assembly.resolve_method(token)?.declaring_type?;
                Some(self.assembly.find_type(name.namespace, name.name)?.token())
            }
            _ => None,
        }
    }

    /// Whether `type_token` names an exception type, for the no-GC tag model's `newobj`/`catch`
    /// recognition: a `System.*Exception` (the BCL exceptions live in another assembly the tag
    /// model never needs to walk into, so they are matched by name), or a this-module type whose
    /// `extends` chain reaches one. The walk is bounded so a malformed cyclic base cannot loop.
    fn is_exception_type(&self, type_token: Token) -> bool {
        let mut current = type_token;
        for _ in 0..64 {
            let Some(name) = self.assembly.type_token_name(current) else {
                return false;
            };
            if name.namespace == "System"
                && (name.name == "Exception" || name.name.ends_with("Exception"))
            {
                return true;
            }
            if current.table() != table::TYPE_DEF {
                return false;
            }
            let Some(type_def) = self.assembly.type_def(current.row()) else {
                return false;
            };
            let base = type_def.extends();
            if base.row() == 0 {
                return false;
            }
            current = base;
        }
        false
    }

    /// The vtable of a type, slot by slot. Built ECMA-335 / `lamella-load::build_vtables`-style so the
    /// AOT and interpreter agree on slots: walk the bases root-first, inheriting their slots; a virtual
    /// whose `newslot` flag (II.23.1.10) is clear and whose NAME + PARAMETER IDENTITY match an inherited
    /// slot REPLACES it (an override), otherwise it APPENDS in MethodDef order. With a
    /// [`with_reference`](Self::with_reference) assembly attached, a root whose `extends` names a
    /// referenced type seeds the referenced base chain's virtuals FIRST -- numbered exactly as that
    /// assembly numbers them itself -- so a program type extending `System.Object` agrees with corlib on
    /// every inherited slot; an inherited-not-overridden slot's implementation is the referenced method,
    /// named by its stable extern symbol. Without a reference, numbering stays this-assembly-relative.
    fn vtable_methods(&self, type_def: TypeDef<'a>) -> Vec<VSlot<'a>> {
        let chain = assembly_base_chain(self.assembly, type_def);
        let mut slots: Vec<VSlot<'a>> = Vec::new();
        if let Some(root) = chain.last() {
            let base = root.extends();
            if base.row() != 0 && base.table() != table::TYPE_DEF {
                if let Some(base_name) = self.assembly.type_token_name(base) {
                    if let Some((_, reference, ref_td)) =
                        self.find_reference_type(base_name.namespace, base_name.name)
                    {
                        slots = reference_vtable_slots(&self.references, reference, ref_td);
                    }
                }
            }
        }
        for td in chain.into_iter().rev() {
            for method in td.methods() {
                if !method.is_virtual() {
                    continue;
                }
                let name = method.name();
                let params = method
                    .signature()
                    .map(|sig| sig.parameters)
                    .unwrap_or_default();
                let key = param_key(self.assembly, &params);
                let rid = method.rid();
                let newslot = method.flags() & 0x0100 != 0;
                if !newslot {
                    if let Some(entry) = slots
                        .iter_mut()
                        .find(|slot| slot.name == name && slot.key == key)
                    {
                        entry.impl_ = SlotImpl::Rid(rid);
                        continue;
                    }
                }
                slots.push(VSlot {
                    name,
                    key,
                    impl_: SlotImpl::Rid(rid),
                });
            }
        }
        slots
    }

    /// Every this-module type's vtable in slot order -- the backend emits this table before the type's
    /// TypeDesc so `callvirt` indexes it. A slot implemented in this module maps to its function index;
    /// a slot inherited from the [reference](Self::with_reference) assembly and not overridden stays an
    /// [`VtableEntry::Extern`] the linker resolves against the library object exporting it. A type whose
    /// vtable is empty, or any of whose local slots is not a module function (e.g. an abstract type,
    /// never instantiated), is omitted. Keyed by the type's handle (`TypeHandle(token.0)`), matching the
    /// handle its `Alloc`/TypeDesc use.
    #[must_use]
    pub fn vtables(&self) -> Vec<(TypeHandle, Vec<VtableEntry>)> {
        let mut result = Vec::new();
        for type_def in self.assembly.type_defs() {
            let methods = self.vtable_methods(type_def);
            if methods.is_empty() {
                continue;
            }
            let entries: Vec<VtableEntry> = methods
                .iter()
                .filter_map(|slot| match &slot.impl_ {
                    SlotImpl::Rid(rid) => self.function_index(*rid).map(VtableEntry::Func),
                    SlotImpl::Extern(symbol) => Some(VtableEntry::Extern(symbol.clone())),
                })
                .collect();
            if entries.len() == methods.len() {
                result.push((TypeHandle(type_def.token().0), entries));
            }
        }
        result
    }

    /// The slot index at which `type_def`'s vtable carries the PARAMETERLESS virtual method `name` --
    /// the index a `callvirt` of it dispatches through on a receiver of this type.
    ///
    /// It answers through [`vtable_methods`](Self::vtable_methods), the same walk
    /// [`vtables`](Self::vtables) builds the EMITTED table from, so the index cannot disagree with the
    /// table it indexes. That is the whole point of asking here rather than counting slots at the call
    /// site: a synthesized body placed at a hardcoded index is a wrong method dispatched under a right
    /// name, and nothing downstream can tell.
    ///
    /// The key is NAME plus the empty parameter list, which is what distinguishes `ToString()` from an
    /// overload of it. `None` when the type has no such slot (it inherits no virtuals -- e.g. an enum
    /// built with no corlib attached, whose base `System.Enum` cannot be resolved).
    #[must_use]
    pub fn nullary_vtable_slot(&self, type_def: TypeDef<'a>, name: &str) -> Option<usize> {
        self.vtable_methods(type_def)
            .iter()
            .position(|slot| slot.name == Some(name) && slot.key.is_empty())
    }

    /// Every this-module type's `type_tag` for the TypeDesc the AOT emits: `exception_tag_for_name`
    /// of its full name (the shared FNV-1a32 scheme, so an exception type's `type_tag` EQUALS its
    /// exception tag -- one tag space for all types). The interpreter computes the same from metadata,
    /// so a shared object's type is identified identically both ways -- the mixed-mode type-identity
    /// bridge. Keyed by `TypeHandle(token.0)`.
    #[must_use]
    pub fn type_tags(&self) -> Vec<(TypeHandle, u32)> {
        self.assembly
            .type_defs()
            .filter_map(|type_def| {
                let name = self.assembly.type_token_name(type_def.token())?;
                let tag = exception_tag_for_name(name.namespace, name.name);
                Some((TypeHandle(type_def.token().0), tag))
            })
            .collect()
    }

    /// The per-type emission metadata the backend's GC module path takes: `(handle, type_tag, vtable)`
    /// for every this-module type -- [`type_tags`](Self::type_tags) joined with
    /// [`vtables`](Self::vtables), and [`itables`](Self::itables), joined per type into a [`TypeMeta`].
    /// The backend appends the tag to each TypeDesc, lays the vtable before it, and the itable after.
    #[must_use]
    pub fn type_descriptors(&self) -> Vec<TypeMeta> {
        let vtables = self.vtables();
        let itables = self.itables();
        let bases = self.base_handles();
        let words: Vec<(TypeHandle, Box<[u32]>)> = self
            .assembly
            .type_defs()
            .filter_map(|type_def| {
                let name = self.assembly.type_token_name(type_def.token())?;
                let type_tag = exception_tag_for_name(name.namespace, name.name);
                let (size, reference_offsets) = match self.reference_layout_of(self.assembly, type_def) {
                    Some(layout) => (layout.size, layout.reference_offsets),
                    None => match primitive_value_size(name.namespace, name.name) {
                        Some(size) => (size, Vec::new()),
                        None => {
                            let layout = self
                                .assembly
                                .value_type_layout(type_def.token(), &TargetLayout::ilp32())
                                .ok()?;
                            (layout.size, layout.reference_offsets)
                        }
                    },
                };
                if name.namespace == "System" && name.name == "String" {
                    return Some((
                        TypeHandle(type_def.token().0),
                        string_descriptor_words(type_tag).to_vec().into_boxed_slice(),
                    ));
                }
                let mut w = alloc::vec![size, reference_offsets.len() as u32, type_tag, 0];
                w.extend(reference_offsets.iter().copied());
                Some((TypeHandle(type_def.token().0), w.into_boxed_slice()))
            })
            .collect();
        let names: Vec<(TypeHandle, alloc::boxed::Box<str>)> = self
            .assembly
            .type_defs()
            .filter_map(|type_def| {
                let name = self.assembly.type_token_name(type_def.token())?;
                Some((
                    TypeHandle(type_def.token().0),
                    qualified_type_name(name.namespace, name.name),
                ))
            })
            .collect();
        self.type_tags()
            .into_iter()
            .map(|(handle, type_tag)| {
                let vtable = vtables
                    .iter()
                    .find(|(h, _)| *h == handle)
                    .map(|(_, slots)| slots.clone())
                    .unwrap_or_default();
                let itable = itables
                    .iter()
                    .find(|(h, _)| *h == handle)
                    .map(|(_, entries)| entries.clone())
                    .unwrap_or_default();
                let base = bases
                    .iter()
                    .find(|(h, _)| *h == handle)
                    .and_then(|(_, b)| *b);
                let words = words.iter().find(|(h, _)| *h == handle).map(|(_, w)| w.clone());
                TypeMeta {
                    handle,
                    type_tag,
                    vtable,
                    itable,
                    base,
                    words,
                    exported: self
                        .assembly
                        .type_def(handle.0 & 0x00ff_ffff)
                        .is_some_and(|type_def| type_def.is_public() || type_def.is_nested()),
                    full_name: names
                        .iter()
                        .find(|(h, _)| *h == handle)
                        .map(|(_, n)| n.clone()),
                }
            })
            .collect()
    }

    /// `System.String`'s own [`TypeMeta`] -- the vtable every STRING dispatches through, and the
    /// identity every type test on one compares against.
    ///
    /// Everything that reaches a string through its header -- `s.ToString()`, `o.GetHashCode()`,
    /// `o.Equals(s)`, `o is string`, `"x" + n` -- resolves against this. The WORDS it is laid with
    /// are the array form, which is [`string_descriptor_words`]' decision, not this one's.
    ///
    /// Answers in both directions the emitter meets, exactly as [`system_array_meta`](Self::system_array_meta)
    /// does: this-assembly when building corlib, reference-owned in a program.
    #[must_use]
    pub fn string_type_meta(&self) -> Option<TypeMeta> {
        let own = self.assembly.type_defs().find(|type_def| {
            self.assembly
                .type_token_name(type_def.token())
                .is_some_and(|name| name.namespace == "System" && name.name == "String")
        });
        if let Some(type_def) = own {
            let handle = TypeHandle(type_def.token().0);
            return self
                .type_descriptors()
                .into_iter()
                .find(|meta| meta.handle == handle);
        }
        let (ordinal, _, type_def) = self.find_reference_type("System", "String")?;
        self.reference_type_meta(reference_handle(ordinal, type_def.token().0))
    }

    /// `System.Array`'s own [`TypeMeta`] -- the vtable EVERY array type dispatches through.
    ///
    /// An array type has no metadata row, so it has no meta of its own to find; but every array IS a
    /// `System.Array`, and a `callvirt` on an array receiver resolves its slot in `System.Array`'s
    /// numbering ([`virtual_slot`](Self::virtual_slot) asks the DECLARING type). So the slots an array
    /// descriptor must carry are exactly these, and the two agree because they come from one place.
    ///
    /// Answers for both directions the emitter meets: this-assembly (building corlib itself, where
    /// `System.Array` is a TypeDef) and reference-owned (a program, where the slots are corlib's
    /// exported symbols reached across the link -- the same shape an inherited vtable slot already
    /// takes). `None` without a resolvable `System.Array`, which leaves arrays exactly as they were.
    #[must_use]
    pub fn system_array_meta(&self) -> Option<TypeMeta> {
        let own = self.assembly.type_defs().find(|type_def| {
            self.assembly
                .type_token_name(type_def.token())
                .is_some_and(|name| name.namespace == "System" && name.name == "Array")
        });
        if let Some(type_def) = own {
            let handle = TypeHandle(type_def.token().0);
            return self
                .type_descriptors()
                .into_iter()
                .find(|meta| meta.handle == handle);
        }
        let (ordinal, _, type_def) = self.find_reference_type("System", "Array")?;
        self.reference_type_meta(reference_handle(ordinal, type_def.token().0))
    }

    /// The descriptor for a REFERENCED-assembly (corlib) type the program ALLOCATES but does not
    /// declare -- e.g. `new StringBuilder()`. Its vtable is numbered ENTIRELY within the reference,
    /// each slot the corlib's exported extern symbol, so a `callvirt` on a program-allocated corlib
    /// object dispatches to the corlib's most-derived override (and the descriptor's vtable relocs
    /// keep those library methods alive through gc). `base` carries the owner's OWN base as a
    /// reference-owned handle (resolved across the attached references when the owner extends a
    /// type from one of ITS references), so a `castclass`/`isinst` chain scan crosses the
    /// assembly boundary; interface dispatch on such a type is not yet threaded (empty itable).
    /// `None` without a reference, or if the handle is not a reference TypeDef.
    pub fn reference_type_meta(&self, handle: TypeHandle) -> Option<TypeMeta> {
        let (ordinal, token) = reference_handle_parts(handle)?;
        let reference = *self.references.get(ordinal)?;
        let type_def = reference.type_def(token & 0x00ff_ffff)?;
        let vtable: Vec<VtableEntry> = reference_vtable_slots(&self.references, reference, type_def)
            .into_iter()
            .map(|slot| match slot.impl_ {
                SlotImpl::Extern(symbol) => VtableEntry::Extern(symbol),
                SlotImpl::Rid(rid) => VtableEntry::Func(rid),
            })
            .collect();
        let name = reference.type_token_name(type_def.token())?;
        let extends = type_def.extends();
        let base = if extends.row() == 0 {
            None
        } else if extends.table() == table::TYPE_DEF {
            Some(reference_handle(ordinal, extends.0))
        } else {
            reference.type_token_name(extends).and_then(|base_name| {
                self.find_reference_type(base_name.namespace, base_name.name)
                    .map(|(base_ordinal, _, base_td)| {
                        reference_handle(base_ordinal, base_td.token().0)
                    })
            })
        };
        Some(TypeMeta {
            handle,
            type_tag: exception_tag_for_name(name.namespace, name.name),
            full_name: Some(qualified_type_name(name.namespace, name.name)),
            vtable,
            itable: self.reference_itable(reference, type_def),
            base,
            words: self
                .reference_layout_of(reference, type_def)
                .map(|layout| (layout.size, layout.reference_offsets))
                .or_else(|| {
                    primitive_value_size(name.namespace, name.name).map(|size| (size, Vec::new()))
                })
                .or_else(|| {
                    reference
                        .value_type_layout(type_def.token(), &TargetLayout::ilp32())
                        .ok()
                        .map(|layout| (layout.size, layout.reference_offsets))
                })
                .map(|(size, reference_offsets)| {
                    if name.namespace == "System" && name.name == "String" {
                        return string_descriptor_words(exception_tag_for_name(
                            name.namespace,
                            name.name,
                        ))
                        .to_vec()
                        .into_boxed_slice();
                    }
                    let mut w = alloc::vec![
                        size,
                        reference_offsets.len() as u32,
                        exception_tag_for_name(name.namespace, name.name),
                        0
                    ];
                    w.extend(reference_offsets.iter().copied());
                    w.into_boxed_slice()
                }),
            exported: true,
        })
    }

    /// The itable of a REFERENCE-owned type -- [`itables`](Self::itables)' derivation run from the
    /// OWNER's metadata instead of this assembly's, so a program that allocates a library type
    /// (`new IfaceLib.Sensor()`, `new Rp2350I2cDriver()`) emits a descriptor that can dispatch an
    /// interface method on it. The program declares neither the interface nor the `InterfaceImpl`
    /// rows, so nothing about the map is visible here: every row is read from the assembly that
    /// DECLARES the link, and each interface `TypeRef` resolves across the attached references by
    /// name ([`Self::find_reference_type`] -- the one rule everywhere), which is what carries the
    /// 3-assembly BSP shape (interface in one library, implementor in another).
    ///
    /// The chain is walked with [`Self::cross_class_chain`], so an interface implemented on a BASE
    /// -- across a further assembly boundary -- is found too (`Rp2350I2cDriver` inheriting
    /// `I2cDriver`'s `IDisposable`, the `using (var d = new Rp2350I2cDriver())` shape). The
    /// implementations come from [`reference_vtable_slots`], which already resolves each interface
    /// method to its MOST-DERIVED override and names it by its stable extern symbol -- so every
    /// entry is an [`VtableEntry::Extern`] the linker resolves against the owning library object,
    /// the same reloc family an inherited vtable slot rides.
    ///
    /// IMPLICIT IMPLEMENTATIONS ONLY: this reads the interface map and not `MethodImpl`, so a
    /// LIBRARY type implementing an interface member EXPLICITLY gets no entry here and dispatches
    /// nowhere. A caller across an assembly boundary must not rely on an explicit implementation.
    fn reference_itable(
        &self,
        reference: &'a Assembly<'a>,
        type_def: TypeDef<'a>,
    ) -> Vec<(u32, VtableEntry)> {
        let impls = reference_vtable_slots(&self.references, reference, type_def);
        let mut entries: Vec<(u32, VtableEntry)> = Vec::new();
        for (link_assembly, link) in self.cross_class_chain(reference, type_def) {
            for iface_token in link.interfaces() {
                let Some((iface_assembly, iface)) = (match iface_token.table() {
                    table::TYPE_DEF => link_assembly
                        .type_def(iface_token.row())
                        .map(|td| (link_assembly, td)),
                    table::TYPE_REF => link_assembly
                        .type_token_name(iface_token)
                        .and_then(|n| self.find_reference_type(n.namespace, n.name))
                        .map(|(_, owner, td)| (owner, td)),
                    _ => None,
                }) else {
                    continue;
                };
                let Some(iface_name) = iface_assembly.type_token_name(iface.token()) else {
                    continue;
                };
                for method in iface.methods() {
                    let Some(name) = method.name() else { continue };
                    let params = method
                        .signature()
                        .map(|sig| sig.parameters)
                        .unwrap_or_default();
                    let tag = interface_method_tag(&iface_name, name, &params);
                    let key = param_key(iface_assembly, &params);
                    let Some(slot) = impls
                        .iter()
                        .find(|slot| slot.name == Some(name) && slot.key == key)
                    else {
                        continue;
                    };
                    if entries.iter().any(|(t, _)| *t == tag) {
                        continue;
                    }
                    let entry = match &slot.impl_ {
                        SlotImpl::Extern(symbol) => VtableEntry::Extern(symbol.clone()),
                        SlotImpl::Rid(rid) => VtableEntry::Func(*rid),
                    };
                    entries.push((tag, entry));
                }
            }
        }
        entries
    }

    /// Each this-module type's immediate base, as `(handle, base_handle)`: a this-module TypeDef
    /// keeps its raw handle, and a TypeRef base that RESOLVES through the attached references
    /// becomes the reference-owned handle -- the cross-assembly base_ptr EDGE a
    /// `castclass`/`isinst` chain scan follows into the owner's descriptors. An UNRESOLVED
    /// TypeRef base (no references attached -- the single-assembly build) stays `None`, so the
    /// chain terminates at the assembly boundary exactly as before. The backend walks this to lay
    /// the TypeDesc base_ptr chain. Keyed by `TypeHandle(token.0)`, like
    /// [`type_tags`](Self::type_tags).
    #[must_use]
    pub fn base_handles(&self) -> Vec<(TypeHandle, Option<TypeHandle>)> {
        self.assembly
            .type_defs()
            .map(|type_def| {
                let base = type_def.extends();
                let base_handle = if base.row() == 0 {
                    None
                } else if base.table() == table::TYPE_DEF {
                    Some(TypeHandle(base.0))
                } else {
                    let qualified = self.qualified_type_handle(base);
                    reference_handle_parts(qualified).is_some().then_some(qualified)
                };
                (TypeHandle(type_def.token().0), base_handle)
            })
            .collect()
    }

    /// Every interface token `type_def` answers for, deduplicated: its own `InterfaceImpl` rows, its
    /// BASES' rows (a derived class inherits them), and the rows of each interface reached (an interface
    /// may extend others, and implementing IA implements what IA extends).
    ///
    /// Bounded against a malformed cyclic base or interface graph rather than trusting the metadata.
    fn interface_closure(&self, type_def: TypeDef<'a>) -> Vec<Token> {
        let mut queue: Vec<Token> = type_def.interfaces().collect();
        let mut base = type_def.extends();
        for _ in 0..64 {
            if base.row() == 0 || base.table() != table::TYPE_DEF {
                break;
            }
            let Some(base_def) = self.assembly.type_def(base.row()) else {
                break;
            };
            queue.extend(base_def.interfaces());
            base = base_def.extends();
        }
        let mut closed: Vec<Token> = Vec::new();
        let mut i = 0;
        let mut budget = 256;
        while let Some(&token) = queue.get(i) {
            i += 1;
            budget -= 1;
            if budget == 0 {
                break;
            }
            if closed.iter().any(|t| t.0 == token.0) {
                continue;
            }
            closed.push(token);
            if token.table() == table::TYPE_DEF {
                if let Some(iface) = self.assembly.type_def(token.row()) {
                    queue.extend(iface.interfaces());
                }
            }
        }
        closed
    }

    /// One `MethodImpl` row as an itable entry: `(interface_method_tag, implementation)`, or `None` if
    /// the row is not interface dispatch or the implementation is not a function of this module.
    ///
    /// `MethodImpl` covers BOTH explicit interface implementations and explicit overrides of a base
    /// CLASS's virtual, and only the first belongs here -- a class virtual is a vtable SLOT, and putting
    /// it in the itable would key a slot by a tag no `callvirt` derives. So the declaring type must
    /// actually be an interface, checked in this assembly first and then across the references, and
    /// anything undecidable is DECLINED rather than guessed.
    fn explicit_itable_entry(
        &self,
        body: Token,
        declaration: Token,
    ) -> Option<(u32, VtableEntry)> {
        let declared = self.assembly.resolve_method(declaration)?;
        let interface = declared.declaring_type?;
        let is_interface = match self.assembly.find_type(interface.namespace, interface.name) {
            Some(td) => td.is_interface(),
            None => {
                self.find_reference_type(interface.namespace, interface.name)?
                    .2
                    .is_interface()
            }
        };
        if !is_interface {
            return None;
        }
        let signature = declared.signature.as_ref()?;
        let tag = interface_method_tag(&interface, declared.name?, &signature.parameters);
        let MethodKind::Definition(rid) = self.assembly.resolve_method(body)?.kind else {
            return None;
        };
        Some((tag, VtableEntry::Func(self.function_index(rid)?)))
    }

    /// The per-type INTERFACE dispatch map: for each this-module type, the `(interface_method_tag,
    /// implementation function index)` pairs for every interface method it implements. The backend emits
    /// these as the type's itable; a `callvirt` on an interface method matches the tag in the receiver's
    /// itable to find the implementation. Implicit implementations only -- the implementing method is
    /// found by name + signature through the base chain ([`vtable_methods`](Self::vtable_methods), which
    /// already collects the virtual methods overrides included). Explicit (MethodImpl) and
    /// external-interface dispatch are unsupported.
    #[must_use]
    pub fn itables(&self) -> Vec<(TypeHandle, Vec<(u32, VtableEntry)>)> {
        let mut result = Vec::new();
        for type_def in self.assembly.type_defs() {
            let impls = self.vtable_methods(type_def);
            let mut entries: Vec<(u32, VtableEntry)> = Vec::new();
            for iface_token in self.interface_closure(type_def) {
                let Some((iface_assembly, iface)) = (match iface_token.table() {
                    table::TYPE_DEF => self
                        .assembly
                        .type_def(iface_token.row())
                        .map(|td| (self.assembly, td)),
                    table::TYPE_REF => self
                        .assembly
                        .type_token_name(iface_token)
                        .and_then(|n| self.find_reference_type(n.namespace, n.name))
                        .map(|(_, owner, td)| (owner, td)),
                    _ => None,
                }) else {
                    continue;
                };
                let Some(iface_name) = iface_assembly.type_token_name(iface.token()) else {
                    continue;
                };
                for method in iface.methods() {
                    let Some(name) = method.name() else { continue };
                    let params = method
                        .signature()
                        .map(|sig| sig.parameters)
                        .unwrap_or_default();
                    let tag = interface_method_tag(&iface_name, name, &params);
                    let key = param_key(iface_assembly, &params);
                    let Some(slot) = impls
                        .iter()
                        .find(|slot| slot.name == Some(name) && slot.key == key)
                    else {
                        continue;
                    };
                    if let SlotImpl::Rid(rid) = &slot.impl_ {
                        if let Some(func_index) = self.function_index(*rid) {
                            entries.push((tag, VtableEntry::Func(func_index)));
                        }
                    }
                }
            }
            for (body, declaration) in type_def.method_impls() {
                let Some(entry) = self.explicit_itable_entry(body, declaration) else {
                    continue;
                };
                match entries.iter_mut().find(|(tag, _)| *tag == entry.0) {
                    Some(slot) => slot.1 = entry.1,
                    None => entries.push(entry),
                }
            }
            if !entries.is_empty() {
                result.push((TypeHandle(type_def.token().0), entries));
            }
        }
        result
    }
}

/// One vtable slot during numbering: the method name, its assembly-independent parameter identity
/// (the extern-symbol parameter encoding, so signatures from two assemblies compare by NAME), and
/// where the most-derived implementation lives.
struct VSlot<'a> {
    name: Option<&'a str>,
    key: String,
    impl_: SlotImpl,
}

/// Where a vtable slot's most-derived implementation lives: a this-assembly `MethodDef` rid (a
/// module function), or a referenced-assembly method named by its stable extern symbol.
enum SlotImpl {
    Rid(u32),
    Extern(String),
}

/// The assembly-independent identity of a parameter list: each parameter's extern-symbol encoding
/// (a primitive one char, a class/value type its FULL NAME), so a referenced base's signature and a
/// this-assembly override's compare equal even though their `SigType` tokens index different
/// metadata tables.
fn param_key(assembly: &Assembly, params: &[SigType]) -> String {
    let mut key = String::new();
    for p in params {
        encode_type(
            p,
            &|token| assembly.type_token_name(token).map(|n| joined_full_name(&n)),
            &mut key,
        );
    }
    key
}

/// The `extends` chain of `type_def` WITHIN `assembly`, derived-first (self at index 0), stopping at
/// a non-TypeDef base (nil for `System.Object`, or a TypeRef into another assembly). Bounded against
/// a malformed cyclic `extends`.
fn assembly_base_chain<'x>(assembly: &'x Assembly<'x>, type_def: TypeDef<'x>) -> Vec<TypeDef<'x>> {
    let mut chain = Vec::new();
    let mut current = Some(type_def);
    for _ in 0..64 {
        let Some(td) = current else {
            break;
        };
        chain.push(td);
        let base = td.extends();
        current = if base.table() == table::TYPE_DEF && base.row() != 0 {
            assembly.type_def(base.row())
        } else {
            None
        };
    }
    chain
}

/// Whether `type_def` is a delegate type, judged within its OWN `assembly` (the program's, or the
/// referenced corlib's for a cross-assembly `new ThreadStart(...)`): its `extends` chain reaches
/// `System.MulticastDelegate`/`System.Delegate`. The walk is bounded so a malformed cyclic base
/// cannot loop.
fn is_delegate_type_of<'x>(assembly: &'x Assembly<'x>, type_def: &TypeDef<'x>) -> bool {
    let mut current = type_def.extends();
    for _ in 0..64 {
        if current.row() == 0 {
            return false;
        }
        let Some(name) = assembly.type_token_name(current) else {
            return false;
        };
        if name.namespace == "System" && matches!(name.name, "MulticastDelegate" | "Delegate") {
            return true;
        }
        if current.table() != table::TYPE_DEF {
            return false;
        }
        let Some(base_def) = assembly.type_def(current.row()) else {
            return false;
        };
        current = base_def.extends();
    }
    false
}

/// `type_def`'s vtable slots as `assembly` numbers them, INCLUDING inherited virtuals from a base
/// declared in a FURTHER referenced assembly. Same root-first newslot/override walk as
/// [`MetadataResolver::vtable_methods`], and -- the fix a 3-assembly inheritance chain forced -- the
/// SAME cross-assembly base-seed: when the chain's root `extends` a type in ANOTHER assembly, that
/// base's slots are numbered FIRST (recursively, resolved through `references`), so a referenced type
/// whose own base is itself a reference -- e.g. `[System.Device]AdcDriver : [corlib]Object` -- lays
/// Object's inherited virtuals as a prefix, exactly as the caller (which seeds via `vtable_methods`)
/// numbers them. Omitting that prefix under-numbered the type by its cross-assembly base's virtual
/// count, so a `callvirt` from the base's own assembly (numbered WITH the prefix) indexed a slot the
/// derived assembly laid LOWER -- the silent cross-assembly mis-dispatch a BSP driver
/// (`Rp2350AdcDriver : AdcDriver : Object`) hit only on real MMIO, where the wrong slot returned a
/// constant. Each slot's implementation is its stable extern symbol (what the owning library object
/// exports it as), ready to seed a derived type's numbering or answer a `MemberRef`'s slot. The base
/// walk is bounded like every other so a malformed cross-assembly cycle cannot loop.
fn reference_vtable_slots<'x>(
    references: &[&'x Assembly<'x>],
    assembly: &'x Assembly<'x>,
    type_def: TypeDef<'x>,
) -> Vec<VSlot<'x>> {
    reference_vtable_slots_seeded(references, assembly, type_def, 0)
}

fn reference_vtable_slots_seeded<'x>(
    references: &[&'x Assembly<'x>],
    assembly: &'x Assembly<'x>,
    type_def: TypeDef<'x>,
    depth: u32,
) -> Vec<VSlot<'x>> {
    let chain = assembly_base_chain(assembly, type_def);
    let mut slots: Vec<VSlot<'x>> = Vec::new();
    if depth < 64 {
        if let Some(root) = chain.last() {
            let base = root.extends();
            if base.row() != 0 && base.table() != table::TYPE_DEF {
                if let Some(base_name) = assembly.type_token_name(base) {
                    if let Some((owner, base_td)) = references.iter().find_map(|reference| {
                        reference
                            .find_type(base_name.namespace, base_name.name)
                            .map(|td| (*reference, td))
                    }) {
                        slots = reference_vtable_slots_seeded(references, owner, base_td, depth + 1);
                    }
                }
            }
        }
    }
    for td in chain.into_iter().rev() {
        let owner = assembly.type_token_name(td.token());
        let owner_namespace: String = owner.as_ref().map(|n| n.namespace.into()).unwrap_or_default();
        let owner_name: String = owner.as_ref().map(|n| n.name.into()).unwrap_or_default();
        for method in td.methods() {
            if !method.is_virtual() {
                continue;
            }
            let name = method.name();
            let params = method
                .signature()
                .map(|sig| sig.parameters)
                .unwrap_or_default();
            let key = param_key(assembly, &params);
            let symbol = extern_method_symbol(
                &owner_namespace,
                &owner_name,
                name.unwrap_or(""),
                &params,
                &|token| assembly.type_token_name(token).map(|n| joined_full_name(&n)),
            );
            let newslot = method.flags() & 0x0100 != 0;
            if !newslot {
                if let Some(entry) = slots
                    .iter_mut()
                    .find(|slot| slot.name == name && slot.key == key)
                {
                    entry.impl_ = SlotImpl::Extern(symbol);
                    continue;
                }
            }
            slots.push(VSlot {
                name,
                key,
                impl_: SlotImpl::Extern(symbol),
            });
        }
    }
    slots
}

/// Where a dispatched method's implementation lives -- a vtable slot's or an itable entry's emitted
/// form: a module FUNCTION INDEX (this-assembly implementation), or the stable extern symbol of a
/// referenced-assembly implementation the linker resolves cross-object (an inherited,
/// not-overridden base virtual -- e.g. `System.Object.ToString.` for a program type that never
/// overrides `ToString` -- or a library type's interface implementation, reached when the PROGRAM
/// allocates that type and dispatches through the interface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VtableEntry {
    /// A module function index (the this-assembly implementation).
    Func(u32),
    /// A referenced-assembly implementation, by its stable extern symbol.
    Extern(String),
}


/// Marks a descriptor as describing an ARRAY rather than a class, in the high bits of word 0; the
/// low bits carry the rank (1 = szarray). No real `payload_size` can collide: payloads are object
/// byte sizes, far below this.
pub const ARRAY_DESC_MARK: u32 = 0xA500_0000;

/// Whether this build stores string BYTES (either UTF-8 tier) rather than UTF-16 units -- the one
/// question the storage encoding asks, answered in ONE place and read by both the blob emission and
/// the descriptor below.
///
/// `cfg!` rather than `#[cfg]` so every backend compiles BOTH arms and only the value changes: a tier
/// switched on but never compiled is how a declared-and-unread storage feature came about, and how
/// the UTF-8 tiers came to be honored by one backend out of three.
pub const STORAGE_IS_BYTES: bool =
    cfg!(any(feature = "string-utf8", feature = "string-utf8-wtf8"));

/// `System.String`'s descriptor HEADER WORDS -- the ratified ARRAY form, conditioned on the storage
/// tier.
///
/// In the DEFAULT (UTF-16) build a Lamella string's payload is byte-identically a `char[]`:
/// `[unit_count][UTF-16LE units]`, which is the array layout a collector already knows how to size
/// and stride. So the descriptor is the array form with the frozen UTF-16-unit element code, and it
/// is not a new encoding -- it is the ratified one applied to a layout that already matches.
///
/// Under `string-utf8` / `string-utf8-wtf8` IT CANNOT BE, and no element width fixes it: the blob is
/// `[unit_count][byte_len][bytes]`, so the first payload word is a UNIT count while the storage is
/// BYTES, and there is a second header word besides. The array form would compute
/// `4 + unit_count * width` and be wrong in both directions. So the kind is `ELEMENT_KIND_OPAQUE`,
/// which is the format's existing "I cannot stride this" -- the same answer a struct element gives.
/// A collector must REFUSE to size it rather than guess, because a wrong footprint does not corrupt
/// one object, it desynchronizes the walk over every object above it.
///
/// The DISPATCH half is unaffected by the tier: word 2 and the vtable laid before the words are what
/// `s.ToString()` and `o is string` read, and those are right in all three tiers.
#[must_use]
pub fn string_descriptor_words(type_tag: u32) -> [u32; 5] {
    let element_kind = if STORAGE_IS_BYTES {
        ELEMENT_KIND_OPAQUE
    } else {
        ELEMENT_KIND_UTF16_UNIT
    };
    [ARRAY_DESC_MARK | 1, element_kind, type_tag, 0, 0]
}

/// The mask selecting [`ARRAY_DESC_MARK`] out of word 0; the remainder is the rank.
pub const ARRAY_DESC_MARK_MASK: u32 = 0xFF00_0000;

/// Element kind 0 -- the elements are REFERENCES (4-byte pointers a collector traces). Collision-free
/// by construction rather than by convention: the frozen primitive code space starts at 1.
pub const ELEMENT_KIND_REFERENCE: u32 = 0;

/// Element kind for a value type that is NOT one of the frozen primitives (a struct element). It
/// carries no width, so a consumer cannot stride by it -- deliberately: the point is that such an
/// array is NOT scannable by this scheme. A struct element holding a reference field would need
/// per-element offsets, which word 1 cannot express, so this code says "do not scan" rather than
/// inviting a wrong answer. Chosen outside the frozen code space so it can never be mistaken for one.
pub const ELEMENT_KIND_OPAQUE: u32 = 0xFF;

/// The frozen code for a UTF-16 code unit -- `U2`, the same code `System.Char` and `System.UInt16`
/// take. Named for the synthesized STRING blob, which is not a `char[]`: it is
/// `[u32 unit_count][UTF-16LE]`, and its descriptor's whole job is to say the elements are 2-byte
/// NON-references, so a collector strides past them instead of tracing code units as pointers.
/// Pinned against [`primitive_element_kind`] by test rather than restated as a literal.
pub const ELEMENT_KIND_UTF16_UNIT: u32 = 4;

/// The frozen primitive element code for a `System` primitive by name, or `None` if it is not one.
///
/// MIRRORS `lamella_cil_runtime::object::PrimKind` (`object.rs:195`), whose codes are FROZEN for the
/// baked-image format: `I1=1 U1=2 I2=3 U2=4 I4=5 I8=6 F4=7 F8=8`, and whose `byte_width` derives the
/// element size from the code alone -- which is what lets an untyped `System.Array` body compute a
/// byte range at run time.
///
/// It is mirrored rather than imported because `lamella-aot` does not depend on the interpreter
/// crate. That makes this a SECOND copy of a shared code space, which is the shape that bites later;
/// both crates already depend on `lamella-cil`, so that is where the enum belongs. Raised with the
/// runtime rather than moved unilaterally.
#[must_use]
pub fn primitive_element_kind(namespace: &str, name: &str) -> Option<u32> {
    if namespace != "System" {
        return None;
    }
    Some(match name {
        "SByte" => 1,
        "Byte" | "Boolean" => 2,
        "Int16" => 3,
        "UInt16" | "Char" => 4,
        "Int32" | "UInt32" => 5,
        "Int64" | "UInt64" => 6,
        "Single" => 7,
        "Double" => 8,
        _ => return None,
    })
}

/// The payload a value of a `System` primitive occupies -- boxed, or as one array element -- derived
/// from its FROZEN element code so this is a view of [`primitive_element_kind`] rather than a second
/// table beside it.
///
/// It has to exist because the metadata cannot answer: corlib declares `System.Int32` and its
/// siblings with consts and methods and NO INSTANCE FIELD, so their `value_type_layout` is ZERO
/// bytes. Everything that must agree about the width of a primitive -- what a `box` allocates, what
/// an array strides by, and what the boxed type's DESCRIPTOR says its payload is -- reads it here.
/// A descriptor answering 0 for `System.Int32` is not a cosmetic gap: `Array.GetValue` boxes an
/// element against exactly that number.
#[must_use]
pub fn primitive_value_size(namespace: &str, name: &str) -> Option<u32> {
    primitive_element_kind(namespace, name).map(|kind| match kind {
        1 | 2 => 1,
        3 | 4 => 2,
        6 | 8 => 8,
        _ => 4,
    })
}

/// Per-type emission metadata the backend's GC module path consumes: the type's identity tag (appended
/// to its TypeDesc for mixed mode), its vtable (function indices in slot order, laid BEFORE the TypeDesc),
/// and its itable (interface-method tag -> function index, laid AFTER). Produced by
/// [`MetadataResolver::type_descriptors`].
#[derive(Debug, Clone)]
pub struct TypeMeta {
    /// The type's handle, `TypeHandle(token.0)`.
    pub handle: TypeHandle,
    /// The FNV identity tag, appended to the TypeDesc.
    pub type_tag: u32,
    /// Virtual-method slots in order (empty if the type has no virtuals): a module function index,
    /// or the extern symbol of an inherited referenced-assembly implementation.
    pub vtable: Vec<VtableEntry>,
    /// Interface dispatch entries -- `(interface_method_tag, implementation)`. The implementation is
    /// a module function for a this-assembly type, and a referenced-assembly EXTERN for a
    /// reference-owned one (a library type the program allocates: its interface implementations are
    /// the library's, reached across the link exactly as an inherited vtable slot is).
    pub itable: Vec<(u32, VtableEntry)>,
    /// The immediate in-program base type's handle, or `None` at the chain's end (a BCL base or
    /// System.Object). The backend lays this as the TypeDesc base_ptr@12 a `castclass` scan walks.
    pub base: Option<TypeHandle>,
    /// The descriptor's own header WORDS -- `[payload, nrefs, tag, base_ptr=0]` then the ref_offsets,
    /// the SAME shape an `Alloc`'s `TypeDescLiteral` carries -- or `None` for a value type. Populated
    /// by [`type_descriptors`](Self::type_descriptors) (from [`reference_layout_of`](Self::reference_layout_of),
    /// the one layout source the `newobj` path also sizes by). It lets a LIBRARY object lay a rich
    /// descriptor for an exported type it never allocates or type-tests -- so a consumer that derives
    /// from that type across the assembly boundary (its base_ptr edge naming the type's canonical
    /// symbol) links against ONE authoritative copy, in its owner, rather than a byte-fragile copy the
    /// consumer would have to reconstruct. `--gc-sections` drops an unreferenced one for zero flash, so
    /// this only grows the reached set. Left `None` on the reference-owned metas that a consumer's
    /// [`reference_type_meta`](Self::reference_type_meta) stages (that path never lays the base itself).
    pub words: Option<Box<[u32]>>,
    /// Whether another assembly can NAME this type -- public, or nested in something public. Only an
    /// exported type can be a cross-assembly base or array element, so only an exported type's
    /// descriptor is worth a library laying proactively; an internal one still carries its
    /// [`words`](Self::words) here, for the emitter to lay locally when something in THIS build
    /// reaches it.
    pub exported: bool,
    /// The type's namespace-qualified name -- `Ns.Outer` for a namespaced type, the bare name for one
    /// in the global namespace -- which is what `Object.ToString()` answers with and what the
    /// descriptor's NAME word points at. `None` for a handle no metadata row names (a synthetic array
    /// handle, a minimal stand-in), whose descriptor then carries a name word of 0.
    ///
    /// It rides here, beside the vtable and itable, because it is per-TYPE data every object-emitting
    /// backend already receives -- the alternative, a parallel table threaded to each `lower_object_*`
    /// entry point, is a second source keyed by the same handle.
    pub full_name: Option<alloc::boxed::Box<str>>,
}

impl<'a> MetadataResolver<'a> {
    /// The reference layout of `type_def` (declared in `owner`) -- payload size and
    /// reference-field offsets; `None` for a value type. Used for a `newobj` of either a
    /// this-assembly class or a referenced-assembly class. The payload spans the WHOLE extends
    /// chain, base blocks first ([`Self::cross_class_chain`]) -- INCLUDING a base declared in
    /// another assembly. A derived class's own TypeDef often declares NO fields
    /// (`AutoResetEvent : WaitHandle` same-assembly; a BSP's `Rp2350I2cDriver : I2cDriver`
    /// cross-assembly, where the base carries `_probeScratch`), and sizing it by the visible
    /// portion alone allocated OVERLAPPING objects -- the first write through an inherited
    /// field then rewrote the NEXT object's header. Each block computes from its OWNING
    /// assembly's metadata, so both sides of a boundary agree on every offset.
    fn reference_layout_of(
        &self,
        owner: &'a Assembly<'a>,
        type_def: TypeDef<'a>,
    ) -> Option<ReferenceLayout> {
        if type_def.is_value_type() {
            return None;
        }
        let mut size = 0u32;
        let mut reference_offsets = Vec::new();
        for (link_assembly, link) in self.cross_class_chain(owner, type_def) {
            let layout = link_assembly
                .value_type_layout(link.token(), &TargetLayout::ilp32())
                .ok()?;
            for offset in layout.reference_offsets {
                reference_offsets.push(size + offset);
            }
            size = (size + layout.size).next_multiple_of(4);
        }
        Some(ReferenceLayout {
            handle: TypeHandle(type_def.token().0),
            size,
            reference_offsets,
        })
    }

    /// The EXTENDS chain of `type_def` (declared in `owner`), BASE-FIRST (the System.Object-most
    /// ancestor first, `type_def` itself last), each link paired with its OWNING assembly. A
    /// TypeDef base continues in the same assembly; a TypeRef base hops through the resolver's
    /// reference list ([`Self::find_reference_type`] -- name-based, first declarer wins, the one
    /// cross-assembly rule) and continues in the owner it resolves to. Stops at an absent or
    /// unresolvable base; bounded against a malformed cyclic chain like `subtype_tags`' walk.
    fn cross_class_chain(
        &self,
        owner: &'a Assembly<'a>,
        type_def: TypeDef<'a>,
    ) -> Vec<(&'a Assembly<'a>, TypeDef<'a>)> {
        let mut assembly = owner;
        let mut chain = alloc::vec![(assembly, type_def)];
        let mut current = type_def.extends();
        for _ in 0..64 {
            if current.row() == 0 {
                break;
            }
            let base = match current.table() {
                table::TYPE_DEF => match assembly.type_def(current.row()) {
                    Some(base) => base,
                    None => break,
                },
                table::TYPE_REF => {
                    let Some(name) = assembly.type_token_name(current) else {
                        break;
                    };
                    let Some((_, base_owner, base)) =
                        self.find_reference_type(name.namespace, name.name)
                    else {
                        break;
                    };
                    assembly = base_owner;
                    base
                }
                _ => break,
            };
            chain.push((assembly, base));
            current = base.extends();
        }
        chain.reverse();
        chain
    }

    /// The payload offset where `type_def`'s OWN field block starts: the word-aligned sum of
    /// every base block before it -- [`Self::reference_layout_of`]'s accumulation, stopped
    /// before `type_def` (the chain's LAST entry by construction).
    fn class_block_start(&self, owner: &'a Assembly<'a>, type_def: TypeDef<'a>) -> Option<u32> {
        let chain = self.cross_class_chain(owner, type_def);
        let mut start = 0u32;
        for (link_assembly, link) in &chain[..chain.len() - 1] {
            let layout = link_assembly
                .value_type_layout(link.token(), &TargetLayout::ilp32())
                .ok()?;
            start = (start + layout.size).next_multiple_of(4);
        }
        Some(start)
    }
}

/// The AOT's interface-method identity tag: FNV-1a32 of the interface's full name, the method name, and
/// a byte per parameter type, with the high bit set (the shared type/exception tag space). A
/// `callvirt IFoo::Bar(args)` and every implementing type's itable entry for it derive the SAME tag, so
/// dispatch needs no shared registry; the interpreter computes it identically to make its signature-key
/// interface dispatch tag-equivalent.
#[must_use]
pub fn interface_method_tag(interface: &TypeName, method: &str, params: &[SigType]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    if !interface.namespace.is_empty() {
        hash = fnv1a32(hash, interface.namespace.as_bytes());
        hash = fnv1a32(hash, b".");
    }
    hash = fnv1a32(hash, interface.name.as_bytes());
    hash = fnv1a32(hash, b".");
    hash = fnv1a32(hash, method.as_bytes());
    for param in params {
        hash = fnv1a32(hash, &[sig_element_byte(param)]);
    }
    hash | 0x8000_0000
}

/// The ECMA-335 element-type byte for a parameter type, folded into an interface-method tag to
/// distinguish overloads. Reference/value types contribute their kind byte, not their name, so
/// overloads differing only by user-defined parameter type are not distinguished.
fn sig_element_byte(ty: &SigType) -> u8 {
    match ty {
        SigType::Void => 0x01,
        SigType::Boolean => 0x02,
        SigType::Char => 0x03,
        SigType::I1 => 0x04,
        SigType::U1 => 0x05,
        SigType::I2 => 0x06,
        SigType::U2 => 0x07,
        SigType::I4 => 0x08,
        SigType::U4 => 0x09,
        SigType::I8 => 0x0a,
        SigType::U8 => 0x0b,
        SigType::R4 => 0x0c,
        SigType::R8 => 0x0d,
        SigType::String => 0x0e,
        SigType::ValueType(_) => 0x11,
        SigType::Class(_) => 0x12,
        SigType::Array { .. } => 0x14,
        SigType::TypedByRef => 0x16,
        SigType::IntPtr => 0x18,
        SigType::UIntPtr => 0x19,
        SigType::Object => 0x1c,
        SigType::SzArray(_) => 0x1d,
        _ => 0x00,
    }
}

impl CallResolver for MetadataResolver<'_> {
    fn resolve(&self, operand: &Operand) -> Option<CallInfo> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let method = self.assembly.resolve_method(*token)?;
        let signature = method.signature.as_ref()?;
        let args = signature.parameters.len() + usize::from(signature.has_this);
        let has_result = !matches!(signature.return_type, SigType::Void);
        let result_type = has_result
            .then(|| {
                mir_type(
                    &signature.return_type,
                    self.assembly,
                    &TargetLayout::ilp32(),
                )
            })
            .flatten();
        let target = match method.kind {
            MethodKind::Definition(_) | MethodKind::Reference if is_int32_tostring(&method) => {
                CallTarget::Intrinsic(Intrinsic::IntToString)
            }
            MethodKind::Definition(rid) => {
                CallTarget::Internal(self.function_index(rid).unwrap_or(rid))
            }
            MethodKind::Reference if is_debug_writeline(&method) => {
                CallTarget::Intrinsic(Intrinsic::DebugWriteLine)
            }
            MethodKind::Reference if is_console_writeline_int(&method) => {
                CallTarget::Intrinsic(Intrinsic::ConsoleWriteLineInt)
            }
            MethodKind::Reference if is_string_op_equality(&method) => {
                CallTarget::Intrinsic(Intrinsic::StringEquals)
            }
            MethodKind::Reference if is_string_concat(&method) => {
                CallTarget::Intrinsic(Intrinsic::StringConcat)
            }
            MethodKind::Reference if is_noop_base_ctor(&method) => {
                CallTarget::Intrinsic(Intrinsic::ObjectCtor)
            }
            MethodKind::Reference if is_array_getlength(&method) => {
                CallTarget::Intrinsic(Intrinsic::ArrayGetLength)
            }
            MethodKind::Reference => {
                let (namespace, type_name) = method
                    .declaring_type
                    .as_ref()
                    .map_or(("", ""), |t| (t.namespace, t.name));
                let name = method.name.unwrap_or("");
                CallTarget::External(
                    extern_method_symbol(
                        namespace,
                        type_name,
                        name,
                        &signature.parameters,
                        &|token| self.assembly.type_token_name(token).map(|n| joined_full_name(&n)),
                    )
                    .into(),
                )
            }
        };
        Some(CallInfo {
            args,
            has_result,
            result_type,
            target,
        })
    }

    fn user_string(&self, operand: &Operand) -> Option<Box<[u16]>> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let raw = self.assembly.image().user_strings().get(token.row()).ok()?;
        Some(decode_user_string(raw).into_boxed_slice())
    }

    fn field_offset(&self, operand: &Operand) -> Option<u32> {
        let Operand::Token(token) = operand else {
            return None;
        };
        match token.table() {
            table::MEMBER_REF => {
                let member = self.assembly.member_ref(token.row())?;
                if !member.is_field() {
                    return None;
                }
                let parent = self.assembly.type_token_name(member.parent())?;
                let field_name = member.name()?;
                let (_, owner, type_def) =
                    self.find_reference_type(parent.namespace, parent.name)?;
                let field = type_def
                    .fields()
                    .find(|f| f.name() == Some(field_name))
                    .filter(|f| !f.is_static())?;
                let block = owner.field_offset(field.token(), &TargetLayout::ilp32())?;
                Some(self.class_block_start(owner, type_def)? + block)
            }
            _ => {
                let block = self.assembly.field_offset(*token, &TargetLayout::ilp32())?;
                let declaring = self
                    .assembly
                    .type_defs()
                    .find(|type_def| type_def.fields().any(|field| field.token() == *token))?;
                Some(self.class_block_start(self.assembly, declaring)? + block)
            }
        }
    }

    fn field_type(&self, operand: &Operand) -> Option<MirType> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let signature = match token.table() {
            table::MEMBER_REF => self.assembly.member_ref(token.row())?.field_type()?,
            _ => self.assembly.field_signature(*token)?,
        };
        mir_type(&signature, self.assembly, &TargetLayout::ilp32())
    }

    fn field_narrow(&self, operand: &Operand) -> Option<(u8, bool)> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let signature = match token.table() {
            table::MEMBER_REF => self.assembly.member_ref(token.row())?.field_type()?,
            _ => self.assembly.field_signature(*token)?,
        };
        match signature {
            SigType::Boolean | SigType::U1 => Some((1, false)),
            SigType::I1 => Some((1, true)),
            SigType::Char | SigType::U2 => Some((2, false)),
            SigType::I2 => Some((2, true)),
            _ => None,
        }
    }

    fn value_type_size(&self, operand: &Operand) -> Option<u32> {
        let Operand::Token(token) = operand else {
            return None;
        };
        self.assembly
            .value_type_layout(*token, &TargetLayout::ilp32())
            .ok()
            .map(|layout| layout.size)
    }

    fn field_on_reference_type(&self, operand: &Operand) -> bool {
        let Operand::Token(token) = operand else {
            return false;
        };
        let declaring = match token.table() {
            table::MEMBER_REF => self
                .assembly
                .member_ref(token.row())
                .map(|m| m.parent())
                .and_then(|parent| match parent.table() {
                    table::TYPE_DEF => Some((self.assembly, self.assembly.type_def(parent.row())?)),
                    table::TYPE_REF => {
                        let name = self.assembly.type_token_name(parent)?;
                        self.find_reference_type(name.namespace, name.name)
                            .map(|(_, owner, type_def)| (owner, type_def))
                    }
                    _ => None,
                }),
            table::FIELD => self
                .assembly
                .type_defs()
                .find(|type_def| type_def.fields().any(|field| field.token() == *token))
                .map(|type_def| (self.assembly, type_def)),
            _ => None,
        };
        let Some((owner, type_def)) = declaring else {
            return false;
        };
        !owner.type_token_name(type_def.extends()).is_some_and(|name| {
            name.namespace == "System" && matches!(name.name, "ValueType" | "Enum")
        })
    }

    fn newobj_value_type(&self, operand: &Operand) -> Option<MirType> {
        let type_def = self.newobj_type_def(operand)?;
        if !type_def.is_value_type() {
            return None;
        }
        let layout = self
            .assembly
            .value_type_layout(type_def.token(), &TargetLayout::ilp32())
            .ok()?;
        Some(MirType::ValueType {
            handle: TypeHandle(type_def.token().0),
            size: layout.size,
        })
    }

    fn newobj_reference_layout(&self, operand: &Operand) -> Option<ReferenceLayout> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let declaring = self.assembly.resolve_method(*token)?.declaring_type?;
        if let Some(type_def) = self.assembly.find_type(declaring.namespace, declaring.name) {
            return self.reference_layout_of(self.assembly, type_def);
        }
        let (ordinal, reference, type_def) =
            self.find_reference_type(declaring.namespace, declaring.name)?;
        let mut layout = self.reference_layout_of(reference, type_def)?;
        layout.handle = reference_handle(ordinal, type_def.token().0);
        Some(layout)
    }

    fn newobj_delegate(&self, operand: &Operand) -> Option<ReferenceLayout> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let declaring = self.assembly.resolve_method(*token)?.declaring_type?;
        let (ordinal, owner, type_def) =
            match self.assembly.find_type(declaring.namespace, declaring.name) {
                Some(type_def) => (None, self.assembly, type_def),
                None => {
                    let (ordinal, owner, type_def) =
                        self.find_reference_type(declaring.namespace, declaring.name)?;
                    (Some(ordinal), owner, type_def)
                }
            };
        if !is_delegate_type_of(owner, &type_def) {
            return None;
        }
        Some(ReferenceLayout {
            handle: match ordinal {
                Some(ordinal) => reference_handle(ordinal, type_def.token().0),
                None => TypeHandle(type_def.token().0),
            },
            size: crate::cil::DELEGATE_SIZE,
            reference_offsets: alloc::vec![
                crate::cil::DELEGATE_TARGET_OFFSET,
                crate::cil::DELEGATE_INVOCATION_LIST_OFFSET,
            ],
        })
    }

    fn delegate_invoke_args(&self, operand: &Operand) -> Option<(usize, bool)> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let method = self.assembly.resolve_method(*token)?;
        if method.name != Some("Invoke") {
            return None;
        }
        let declaring = method.declaring_type?;
        let (owner, type_def) = match self.assembly.find_type(declaring.namespace, declaring.name) {
            Some(type_def) => (self.assembly, type_def),
            None => {
                let (_, owner, type_def) =
                    self.find_reference_type(declaring.namespace, declaring.name)?;
                (owner, type_def)
            }
        };
        if !is_delegate_type_of(owner, &type_def) {
            return None;
        }
        let sig = method.signature?;
        Some((sig.parameters.len(), sig.return_type != SigType::Void))
    }

    fn pinvoke_call(&self, operand: &Operand) -> Option<PInvokeCall> {
        let Operand::Token(token) = operand else {
            return None;
        };
        if token.table() != table::METHOD_DEF {
            return None;
        }
        let import = self.assembly.pinvoke_import(token.row())?;
        let sig = self.assembly.resolve_method(*token)?.signature?;
        let result_type = if sig.return_type == SigType::Void {
            None
        } else {
            mir_type(&sig.return_type, self.assembly, &TargetLayout::ilp32())
        };
        let param_is_string = sig
            .parameters
            .iter()
            .map(|p| *p == SigType::String)
            .collect();
        let charset = match self.assembly.pinvoke_charset(token.row()) {
            Some(CharSet::Unicode) => 1,
            _ => 0,
        };
        Some(PInvokeCall {
            import: import.into(),
            param_is_string,
            result_type,
            result_is_bool: sig.return_type == SigType::Boolean,
            charset,
        })
    }

    fn array_element(&self, operand: &Operand) -> Option<ArrayElement> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let definition = match token.table() {
            table::TYPE_DEF => self
                .assembly
                .type_def(token.row())
                .map(|type_def| (self.assembly, type_def)),
            table::TYPE_REF => self
                .assembly
                .type_token_name(*token)
                .and_then(|name| self.find_reference_type(name.namespace, name.name))
                .map(|(_, owner, type_def)| (owner, type_def)),
            _ => None,
        };
        let value_type_size = || {
            definition
                .filter(|(_, type_def)| type_def.is_value_type())
                .and_then(|(owner, type_def)| {
                    owner
                        .value_type_layout(type_def.token(), &TargetLayout::ilp32())
                        .ok()
                        .map(|layout| layout.size)
                })
        };
        let element_size = self
            .assembly
            .type_token_name(*token)
            .and_then(|name| primitive_value_size(name.namespace, name.name))
            .or_else(value_type_size)
            .unwrap_or(4);
        let element_kind = match self.assembly.type_token_name(*token) {
            Some(name) => primitive_element_kind(name.namespace, name.name).unwrap_or({
                match definition {
                    Some((_, type_def)) if type_def.is_value_type() => ELEMENT_KIND_OPAQUE,
                    _ => ELEMENT_KIND_REFERENCE,
                }
            }),
            None => ELEMENT_KIND_OPAQUE,
        };
        let element = self
            .assembly
            .type_token_name(*token)
            .map(|_| self.qualified_type_handle(*token));
        Some(ArrayElement {
            handle: lamella_ir::array_handle(element.unwrap_or(TypeHandle(token.0))),
            element,
            element_size,
            element_kind,
        })
    }

    fn array_2d_op(&self, operand: &Operand) -> Option<Array2DOp> {
        let Operand::Token(token) = operand else {
            return None;
        };
        if token.table() != table::MEMBER_REF {
            return None;
        }
        let member = self.assembly.member_ref(token.row())?;
        let parent = member.parent();
        let SigType::Array { element, rank } = self.assembly.type_spec_signature(parent)? else {
            return None;
        };
        if rank != 2 {
            return None;
        }
        let (element_size, signed) = array_element_size(&element);
        match member.name()? {
            ".ctor" => Some(Array2DOp::New {
                handle: TypeHandle(parent.0),
                element_size,
            }),
            "Get" => Some(Array2DOp::Get {
                element_size,
                signed,
                element_type: mir_type(&element, self.assembly, &TargetLayout::ilp32())
                    .unwrap_or(MirType::I32),
            }),
            "Set" => Some(Array2DOp::Set { element_size }),
            _ => None,
        }
    }

    fn array_md_op(&self, operand: &Operand) -> Option<ArrayMDOp> {
        let Operand::Token(token) = operand else {
            return None;
        };
        if token.table() != table::MEMBER_REF {
            return None;
        }
        let member = self.assembly.member_ref(token.row())?;
        let parent = member.parent();
        let SigType::Array { element, rank } = self.assembly.type_spec_signature(parent)? else {
            return None;
        };
        if rank < 3 {
            return None;
        }
        let rank = rank as usize;
        let (element_size, signed) = array_element_size(&element);
        match member.name()? {
            ".ctor" => Some(ArrayMDOp::New {
                handle: TypeHandle(parent.0),
                element_size,
                rank,
            }),
            "Get" => Some(ArrayMDOp::Get {
                element_size,
                signed,
                element_type: mir_type(&element, self.assembly, &TargetLayout::ilp32())
                    .unwrap_or(MirType::I32),
                rank,
            }),
            "Set" => Some(ArrayMDOp::Set { element_size, rank }),
            _ => None,
        }
    }

    fn static_field_offset(&self, operand: &Operand) -> Option<(StaticOwner, u32)> {
        let Operand::Token(token) = operand else {
            return None;
        };
        match token.table() {
            table::FIELD => static_field_slots(self.assembly)
                .into_iter()
                .find(|(row, _, _)| *row == token.row())
                .map(|(_, slot, _)| (StaticOwner::Own, slot * 4)),
            table::MEMBER_REF => {
                let member = self.assembly.member_ref(token.row())?;
                if !member.is_field() {
                    return None;
                }
                let parent = self.assembly.type_token_name(member.parent())?;
                let field_name = member.name()?;
                let (ordinal, owner, type_def) =
                    self.find_reference_type(parent.namespace, parent.name)?;
                let field_row = type_def
                    .fields()
                    .find(|f| f.name() == Some(field_name))?
                    .token()
                    .row();
                let slot = static_field_slots(owner)
                    .into_iter()
                    .find(|(row, _, _)| *row == field_row)
                    .map(|(_, slot, _)| slot)?;
                Some((StaticOwner::Reference(u8::try_from(ordinal).ok()?), slot * 4))
            }
            _ => None,
        }
    }

    fn exception_tag(&self, operand: &Operand) -> Option<u32> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let type_token = self.type_token_of(*token)?;
        if !self.is_exception_type(type_token) {
            return None;
        }
        let tag = self.assembly.exception_tag(type_token);
        (tag != 0).then_some(tag)
    }

    fn is_catch_all_type(&self, operand: &Operand) -> bool {
        let Operand::Token(token) = operand else {
            return false;
        };
        self.type_token_of(*token)
            .and_then(|type_token| self.assembly.type_token_name(type_token))
            .is_some_and(|name| {
                name.namespace == "System" && matches!(name.name, "Exception" | "Object")
            })
    }

    fn builtin_exception_tag(&self, namespace: &str, name: &str) -> Option<u32> {
        Some(exception_tag_for_name(namespace, name))
    }

    fn subtype_tags(&self, operand: &Operand) -> Vec<u32> {
        let Operand::Token(token) = operand else {
            return Vec::new();
        };
        let Some(catch_token) = self.type_token_of(*token) else {
            return Vec::new();
        };
        let Some(catch_name) = self.assembly.type_token_name(catch_token) else {
            return Vec::new();
        };
        let mut tags = Vec::new();
        tags.push(self.assembly.exception_tag(catch_token));
        for type_def in self.assembly.type_defs() {
            let mut current = type_def.extends();
            for _ in 0..64 {
                if current.row() == 0 {
                    break;
                }
                let Some(name) = self.assembly.type_token_name(current) else {
                    break;
                };
                if name.namespace == catch_name.namespace && name.name == catch_name.name {
                    let tag = self.assembly.exception_tag(type_def.token());
                    if tag != 0 && !tags.contains(&tag) {
                        tags.push(tag);
                    }
                    break;
                }
                if current.table() != table::TYPE_DEF {
                    break;
                }
                let Some(base_def) = self.assembly.type_def(current.row()) else {
                    break;
                };
                current = base_def.extends();
            }
        }
        let catch_tag = self.assembly.exception_tag(catch_token);
        for type_ref in self.assembly.type_refs() {
            let Some(chain) = self.assembly.exception_base_chain(type_ref.token()) else {
                continue;
            };
            if chain.contains(&catch_tag) {
                if let Some(&leaf) = chain.first() {
                    if leaf != 0 && !tags.contains(&leaf) {
                        tags.push(leaf);
                    }
                }
            }
        }
        if catch_name.namespace == "System" && catch_name.name == "SystemException" {
            for trap in [
                "IndexOutOfRangeException",
                "NullReferenceException",
                "InvalidCastException",
            ] {
                let tag = exception_tag_for_name("System", trap);
                if !tags.contains(&tag) {
                    tags.push(tag);
                }
            }
        }
        tags
    }

    fn catch_binding_layout(&self, operand: &Operand) -> Option<ReferenceLayout> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let target = self.type_token_of(*token)?;
        if !self.is_exception_type(target) {
            return None;
        }
        if target.table() == table::TYPE_DEF {
            let type_def = self.assembly.type_def(target.row())?;
            return self.reference_layout_of(self.assembly, type_def);
        }
        let name = self.assembly.type_token_name(target)?;
        let (ordinal, reference, type_def) = self.find_reference_type(name.namespace, name.name)?;
        let mut layout = self.reference_layout_of(reference, type_def)?;
        layout.handle = reference_handle(ordinal, type_def.token().0);
        Some(layout)
    }

    fn cast_subtype_handles(&self, operand: &Operand) -> Vec<TypeHandle> {
        let Operand::Token(token) = operand else {
            return Vec::new();
        };
        let Some(target) = self.type_token_of(*token) else {
            return Vec::new();
        };
        let Some(target_name) = self.assembly.type_token_name(target) else {
            return Vec::new();
        };
        let mut handles = Vec::new();
        handles.push(self.qualified_type_handle(target));
        for type_def in self.assembly.type_defs() {
            let mut current = type_def.extends();
            for _ in 0..64 {
                if current.row() == 0 {
                    break;
                }
                let Some(name) = self.assembly.type_token_name(current) else {
                    break;
                };
                if name.namespace == target_name.namespace && name.name == target_name.name {
                    let handle = TypeHandle(type_def.token().0);
                    if !handles.contains(&handle) {
                        handles.push(handle);
                    }
                    break;
                }
                if current.table() != table::TYPE_DEF {
                    break;
                }
                let Some(base_def) = self.assembly.type_def(current.row()) else {
                    break;
                };
                current = base_def.extends();
            }
        }
        handles
    }

    fn cast_interface_tag(&self, operand: &Operand) -> Option<u32> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let target = self.type_token_of(*token)?;
        let name = self.assembly.type_token_name(target)?;
        let (interface_assembly, interface): (&Assembly, TypeDef) =
            match self.assembly.find_type(name.namespace, name.name) {
                Some(td) => (self.assembly, td),
                None => {
                    let (_, owner, td) = self.find_reference_type(name.namespace, name.name)?;
                    (owner, td)
                }
            };
        if !interface.is_interface() {
            return None;
        }
        let _ = interface_assembly;
        interface.methods().find_map(|method| {
            let signature = method.signature()?;
            Some(interface_method_tag(&name, method.name()?, &signature.parameters))
        })
    }

    fn cast_target_chain(&self, operand: &Operand) -> Option<TypeHandle> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let target = self.type_token_of(*token)?;
        if target.table() == table::TYPE_DEF {
            return Some(TypeHandle(target.0));
        }
        let qualified = self.qualified_type_handle(target);
        reference_handle_parts(qualified).is_some().then_some(qualified)
    }

    fn boxed_layout(&self, operand: &Operand) -> Option<ReferenceLayout> {
        let Operand::Token(token) = operand else {
            return None;
        };
        let handle = self.qualified_type_handle(*token);
        if let Some(size) = self
            .assembly
            .type_token_name(*token)
            .and_then(|name| primitive_value_size(name.namespace, name.name))
        {
            return Some(ReferenceLayout {
                handle,
                size,
                reference_offsets: Vec::new(),
            });
        }
        let layout = self
            .assembly
            .value_type_layout(*token, &TargetLayout::ilp32())
            .ok()?;
        Some(ReferenceLayout {
            handle,
            size: layout.size,
            reference_offsets: layout.reference_offsets,
        })
    }

    fn boxed_value_type(&self, operand: &Operand) -> Option<MirType> {
        let Operand::Token(token) = operand else {
            return None;
        };
        if let Some(name) = self.assembly.type_token_name(*token) {
            if name.namespace == "System" {
                match name.name {
                    "Boolean" | "SByte" | "Byte" | "Int16" | "UInt16" | "Char" | "Int32"
                    | "UInt32" => return Some(MirType::I32),
                    "Single" => return Some(MirType::F32),
                    "Int64" | "UInt64" => return Some(MirType::I64),
                    "Double" => return Some(MirType::F64),
                    _ => {}
                }
            }
        }
        let layout = self
            .assembly
            .value_type_layout(*token, &TargetLayout::ilp32())
            .ok()?;
        Some(MirType::ValueType {
            handle: TypeHandle(token.0),
            size: layout.size,
        })
    }

    fn type_operand_mir(&self, operand: &Operand) -> Option<MirType> {
        let Operand::Token(token) = operand else {
            return None;
        };
        if let Some(name) = self.assembly.type_token_name(*token) {
            if name.namespace == "System" {
                match name.name {
                    "Boolean" | "SByte" | "Byte" | "Int16" | "UInt16" | "Char" | "Int32"
                    | "UInt32" => return Some(MirType::I32),
                    "Single" => return Some(MirType::F32),
                    "Int64" | "UInt64" => return Some(MirType::I64),
                    "Double" => return Some(MirType::F64),
                    "IntPtr" | "UIntPtr" => return Some(MirType::NativeInt),
                    _ => {}
                }
            }
        }
        if let Some(underlying) =
            enum_underlying(self.assembly, *token, self.references(), &TargetLayout::ilp32())
        {
            return Some(underlying);
        }
        if let Ok(layout) = self
            .assembly
            .value_type_layout(*token, &TargetLayout::ilp32())
        {
            return Some(MirType::ValueType {
                handle: TypeHandle(token.0),
                size: layout.size,
            });
        }
        Some(MirType::ObjectRef)
    }

    fn virtual_slot(&self, operand: &Operand) -> Option<usize> {
        let Operand::Token(token) = operand else {
            return None;
        };
        match token.table() {
            table::METHOD_DEF => {
                let type_token = self.type_token_of(*token)?;
                if type_token.table() != table::TYPE_DEF {
                    return None;
                }
                let type_def = self.assembly.type_def(type_token.row())?;
                if type_def.is_interface() {
                    return None;
                }
                let rid = token.row();
                self.vtable_methods(type_def)
                    .iter()
                    .position(|slot| matches!(&slot.impl_, SlotImpl::Rid(r) if *r == rid))
            }
            table::MEMBER_REF => {
                let method = self.assembly.resolve_method(*token)?;
                let declaring = method.declaring_type.as_ref()?;
                let (_, reference, ref_td) =
                    self.find_reference_type(declaring.namespace, declaring.name)?;
                if ref_td.is_interface() {
                    return None;
                }
                let signature = method.signature.as_ref()?;
                let key = param_key(self.assembly, &signature.parameters);
                reference_vtable_slots(&self.references, reference, ref_td)
                    .iter()
                    .position(|slot| slot.name == method.name && slot.key == key)
            }
            _ => None,
        }
    }

    fn interface_call_tag(&self, operand: &Operand) -> Option<u32> {
        let Operand::Token(token) = operand else {
            return None;
        };
        match token.table() {
            table::METHOD_DEF => {
                let type_token = self.type_token_of(*token)?;
                if type_token.table() != table::TYPE_DEF {
                    return None;
                }
                let type_def = self.assembly.type_def(type_token.row())?;
                if !type_def.is_interface() {
                    return None;
                }
                let method = type_def.methods().find(|m| m.rid() == token.row())?;
                let name = method.name()?;
                let params = method
                    .signature()
                    .map(|sig| sig.parameters)
                    .unwrap_or_default();
                let iface_name = self.assembly.type_token_name(type_token)?;
                Some(interface_method_tag(&iface_name, name, &params))
            }
            table::MEMBER_REF => {
                let method = self.assembly.resolve_method(*token)?;
                let declaring = method.declaring_type?;
                let (_, _, ref_td) =
                    self.find_reference_type(declaring.namespace, declaring.name)?;
                if !ref_td.is_interface() {
                    return None;
                }
                let signature = method.signature.as_ref()?;
                Some(interface_method_tag(
                    &declaring,
                    method.name?,
                    &signature.parameters,
                ))
            }
            _ => None,
        }
    }

    fn constrained_call(&self, constrained: &Operand, method: &Operand) -> Option<CallInfo> {
        let (Operand::Token(type_token), Operand::Token(method_token)) = (constrained, method)
        else {
            return None;
        };
        if type_token.table() != table::TYPE_DEF {
            return None;
        }
        let type_def = self.assembly.type_def(type_token.row())?;
        let target = self.assembly.resolve_method(*method_token)?;
        let name = target.name?;
        let signature = target.signature.as_ref()?;
        let key = param_key(self.assembly, &signature.parameters);
        let own = type_def.methods().find(|m| {
            m.is_virtual()
                && m.name() == Some(name)
                && param_key(
                    self.assembly,
                    &m.signature().map(|sig| sig.parameters).unwrap_or_default(),
                ) == key
        })?;
        let has_result = !matches!(signature.return_type, SigType::Void);
        Some(CallInfo {
            args: signature.parameters.len() + 1,
            has_result,
            result_type: has_result
                .then(|| {
                    mir_type(
                        &signature.return_type,
                        self.assembly,
                        &TargetLayout::ilp32(),
                    )
                })
                .flatten(),
            target: CallTarget::Internal(
                self.function_index(own.rid()).unwrap_or_else(|| own.rid()),
            ),
        })
    }
}

/// The byte width and signedness of a primitive 2-D array element (a sub-word `Get` sign- or
/// zero-extends per the flag); references and unhandled element types fall back to a 4-byte slot.
fn array_element_size(element: &SigType) -> (u32, bool) {
    match element {
        SigType::I1 => (1, true),
        SigType::Boolean | SigType::U1 => (1, false),
        SigType::I2 => (2, true),
        SigType::Char | SigType::U2 => (2, false),
        SigType::I4 => (4, true),
        SigType::U4 | SigType::R4 => (4, false),
        SigType::I8 | SigType::U8 | SigType::R8 => (8, false),
        _ => (4, false),
    }
}

/// Maps a metadata [`SigType`] to the MIR type the AOT lowers it as. `None` for `void` and
/// for types the backend does not lower yet (a value type in another assembly, arrays).
fn mir_type(sig: &SigType, assembly: &Assembly, target: &TargetLayout) -> Option<MirType> {
    Some(match sig {
        SigType::Boolean
        | SigType::Char
        | SigType::I1
        | SigType::U1
        | SigType::I2
        | SigType::U2
        | SigType::I4
        | SigType::U4 => MirType::I32,
        SigType::I8 | SigType::U8 => MirType::I64,
        SigType::R4 => MirType::F32,
        SigType::R8 => MirType::F64,
        SigType::IntPtr | SigType::UIntPtr => MirType::NativeInt,
        SigType::Pointer(_) => MirType::NativeInt,
        SigType::Class(_) | SigType::Object | SigType::String => MirType::ObjectRef,
        SigType::SzArray(_) | SigType::Array { .. } => MirType::ObjectRef,
        SigType::ValueType(token) => match enum_underlying(assembly, *token, &[], target) {
            Some(underlying) => underlying,
            None => MirType::ValueType {
                handle: TypeHandle(token.0),
                size: assembly.value_type_layout(*token, target).ok()?.size,
            },
        },
        _ => return None,
    })
}

/// The DENSE static-field layout of one assembly: every static, non-literal Field row paired with
/// its region slot, in metadata order, slots numbered from 1 -- slot 0 (region offset 0) is
/// RESERVED, because offset 0 is the MIR-level EH-tag marker (`cil::G_EXCEPTION_TAG_OFFSET`) and a
/// field slot there would alias every throw/catch. Literal (`const`) fields have no runtime
/// storage (ECMA-335 II.16.1.2) and are skipped -- a compiler inlines their values, and an
/// `ldsfld` naming one fails the offset lookup LOUD rather than reading a phantom slot. This is
/// the ONE source for both the `ldsfld`/`stsfld` lowering ([`CallResolver::static_field_offset`])
/// and the mode-2 statics stack-map record (`build::assembly_statics`) -- the two must never
/// drift, or the collector walks the wrong words. Each entry carries its WIDTH IN WORDS: an
/// `int64`/`double` static reserves TWO, everything else one.
///
/// The width predicate is [`mir_type`] -- the SAME function [`CallResolver::field_type`] types the
/// `ldsfld`/`stsfld` value with -- so the reservation and the lowering cannot disagree about which
/// static is 64-bit. That matters in three directions, and each is safe:
/// * an OWN-assembly static reads the same signature blob through the same function, so the two
///   answers are identical by construction (this covers an enum with a `long` underlying type,
///   which `mir_type` folds to `I64` for both);
/// * a CROSS-assembly `long`/`double` encodes as the same primitive in the MemberRef blob, so the
///   two agree there too;
/// * a CROSS-assembly ENUM static resolves in the owner but not through the referencing
///   assembly's TypeRef, where `field_type` already answers `None` and the lowering refuses LOUD --
///   so the owner reserving two words leaves a HOLE, never an overlap.
///
/// A struct-typed static still gets ONE word and its lowering still moves one: that truncation is
/// unchanged here and wants the multi-word static copy the two backends do not emit yet.
///
/// Slot 0 (region offset 0) is RESERVED, because offset 0 is the MIR-level EH-tag marker
/// (`cil::G_EXCEPTION_TAG_OFFSET`) and a field slot there would alias every throw/catch.
pub(crate) fn static_field_slots(assembly: &Assembly) -> Vec<(u32, u32, u32)> {
    let mut slots = Vec::new();
    let mut next = 1u32;
    for type_def in assembly.type_defs() {
        for field in type_def.fields() {
            if field.is_static() && !field.is_literal() {
                let words = match field
                    .signature()
                    .and_then(|sig| mir_type(&sig, assembly, &TargetLayout::ilp32()))
                {
                    Some(MirType::I64 | MirType::F64) => 2,
                    _ => 1,
                };
                slots.push((field.token().row(), next, words));
                next += words;
            }
        }
    }
    slots
}

/// The word count one assembly's static region spans, INCLUDING the reserved word 0 -- the one
/// derivation of its size, so a region and the offsets written into it come from the same walk.
/// Gated with its only caller (`build::assembly_statics`): the WASM path places its statics at a
/// fixed base and emits no region record, so a wasm-only build would carry this unused.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
pub(crate) fn static_region_words(assembly: &Assembly) -> u32 {
    static_field_slots(assembly)
        .iter()
        .map(|(_, slot, words)| slot + words)
        .max()
        .unwrap_or(1)
}

/// A type's dotted full name (`namespace.name`, or just `name` in the global namespace) -- what a
/// `Class`/`ValueType` parameter contributes to an extern method symbol.
pub(crate) fn joined_full_name(name: &TypeName) -> String {
    if name.namespace.is_empty() {
        name.name.into()
    } else {
        alloc::format!("{}.{}", name.namespace, name.name)
    }
}

/// A stable cross-assembly symbol for a managed method -- its dotted full name plus an encoding of each
/// parameter type, so every overload gets a distinct symbol. A primitive is one char; a `Class`/
/// `ValueType` contributes its FULL TYPE NAME (`O<name>;` / `V<name>;`), so overloads differing only by
/// a user-defined parameter type stay distinct; an array/byref/pointer is a marker plus its element's
/// encoding. `type_full_name` resolves a type token to its dotted name (`None` -> "?", so the symbol is
/// still stable if a token cannot be resolved). A cross-assembly extern call and the defining library
/// object mangle identically, so the own linker pairs them: "System.Math.Max.ii" (int,int) vs ".ll"
/// (long,long) vs "System.DateTime.op_Subtraction.VSystem.DateTime;VSystem.TimeSpan;".
pub fn extern_method_symbol(
    namespace: &str,
    type_name: &str,
    method: &str,
    params: &[SigType],
    type_full_name: &dyn Fn(Token) -> Option<String>,
) -> String {
    let mut codes = String::new();
    for p in params {
        encode_type(p, type_full_name, &mut codes);
    }
    if namespace.is_empty() {
        alloc::format!("{type_name}.{method}.{codes}")
    } else {
        alloc::format!("{namespace}.{type_name}.{method}.{codes}")
    }
}

/// Appends `sig`'s parameter encoding to `out` (see [`extern_method_symbol`]) -- one char for a
/// primitive, a full type name for a `Class`/`ValueType` (terminated by `;`), and a marker plus a
/// recursive element encoding for an array/byref/pointer. Injective: no primitive code is a digit, so an
/// `Array`'s decimal rank ends unambiguously where its element encoding begins.
fn encode_type(sig: &SigType, type_full_name: &dyn Fn(Token) -> Option<String>, out: &mut String) {
    match sig {
        SigType::Boolean => out.push('z'),
        SigType::Char => out.push('c'),
        SigType::I1 => out.push('b'),
        SigType::U1 => out.push('B'),
        SigType::I2 => out.push('s'),
        SigType::U2 => out.push('S'),
        SigType::I4 => out.push('i'),
        SigType::U4 => out.push('I'),
        SigType::I8 => out.push('l'),
        SigType::U8 => out.push('L'),
        SigType::R4 => out.push('f'),
        SigType::R8 => out.push('d'),
        SigType::String => out.push('q'),
        SigType::Object => out.push('o'),
        SigType::IntPtr => out.push('n'),
        SigType::UIntPtr => out.push('N'),
        SigType::TypedByRef => out.push('t'),
        SigType::Class(token) => {
            out.push('O');
            out.push_str(&type_full_name(*token).unwrap_or_else(|| String::from("?")));
            out.push(';');
        }
        SigType::ValueType(token) => {
            out.push('V');
            out.push_str(&type_full_name(*token).unwrap_or_else(|| String::from("?")));
            out.push(';');
        }
        SigType::SzArray(element) => {
            out.push('a');
            encode_type(element, type_full_name, out);
        }
        SigType::Array { element, rank } => {
            out.push('A');
            out.push_str(&alloc::format!("{rank}"));
            encode_type(element, type_full_name, out);
        }
        SigType::Pointer(element) => {
            out.push('p');
            encode_type(element, type_full_name, out);
        }
        SigType::ByRef(element) => {
            out.push('r');
            encode_type(element, type_full_name, out);
        }
        _ => out.push('x'),
    }
}

/// A `ValueType` signature token resolved to the assembly that DECLARES it plus its `TypeDef`: a
/// this-assembly `TypeDef` stays put; a `TypeRef` into a referenced assembly (a parameter/field/
/// local typed as another assembly's enum or struct) resolves BY NAME through `references` to its
/// owner -- the value-type twin of [`MetadataResolver::find_reference_type`] and the cross-assembly
/// base-chain walk. `None` for a token that is neither, or a name no reference declares.
fn resolve_value_type_def<'x>(
    assembly: &'x Assembly<'x>,
    token: Token,
    references: &[&'x Assembly<'x>],
) -> Option<(&'x Assembly<'x>, TypeDef<'x>)> {
    match token.table() {
        table::TYPE_DEF => Some((assembly, assembly.type_def(token.row())?)),
        table::TYPE_REF => {
            let name = assembly.type_token_name(token)?;
            references.iter().find_map(|reference| {
                reference
                    .find_type(name.namespace, name.name)
                    .map(|type_def| (*reference, type_def))
            })
        }
        _ => None,
    }
}

/// If `token` names an enum (a value type whose base is `System.Enum`), the MirType of its underlying
/// integer. An enum is erased to that integer for codegen, so its values are scalars, not structs.
/// The token is resolved through [`resolve_value_type_def`] FIRST -- so a CROSS-ASSEMBLY enum (a
/// `TypeRef` into a referenced assembly, e.g. a BSP driver method's `AdcChannelMode` parameter)
/// erases the same as a this-assembly one; before, a `TypeRef` fell straight to `None` and the enum
/// parameter became a size-0 value type the emit verify rejected (`NotWellFormed`). `None` for a
/// real struct or an unresolvable token.
pub(crate) fn enum_underlying<'x>(
    assembly: &'x Assembly<'x>,
    token: Token,
    references: &[&'x Assembly<'x>],
    target: &TargetLayout,
) -> Option<MirType> {
    let (owner, type_def) = resolve_value_type_def(assembly, token, references)?;
    let base = owner.type_token_name(type_def.extends())?;
    if base.namespace != "System" || base.name != "Enum" {
        return None;
    }
    let underlying = type_def
        .fields()
        .find(|field| !field.is_static())?
        .signature()?;
    mir_type(&underlying, owner, target)
}

/// Lowers the given methods of an `assembly` to MIR as one module: a call from one of them
/// to another resolves to the callee's index in `methods` (so pass them in the order you
/// will give a module lowering such as [`crate::arm32::lower_module`], the entry first), and
/// each method's arguments and locals are typed from its signature.
///
/// Errors if a method has no CIL body, or if a body cannot be lowered.
pub fn lower_methods(assembly: &Assembly, methods: &[Method]) -> Result<Vec<Function>, CilError> {
    lower_methods_with_references(assembly, methods, &[])
}

/// As [`lower_methods`], but with the REFERENCED assemblies attached -- which is what the object
/// build does, and therefore what a diagnostic must do to reproduce the build's typing.
///
/// Without them the resolver types a program as though corlib did not exist, so a cross-assembly
/// `ValueType`/enum resolves differently and a whole family of type answers changes. That makes the
/// reference-less form capable of FINDING a verifier error and incapable of certifying one is
/// absent -- which is a distinction a tool built to explain refusals has to get right.
pub fn lower_methods_with_references<'a>(
    assembly: &'a Assembly<'a>,
    methods: &[Method<'a>],
    references: &[&'a Assembly<'a>],
) -> Result<Vec<Function>, CilError> {
    let rids: Vec<u32> = methods.iter().map(Method::rid).collect();
    let resolver = MetadataResolver::for_module(assembly, &rids).with_references(references);
    let target = TargetLayout::ilp32();
    methods
        .iter()
        .map(|method| {
            let body = method.body().ok_or(CilError::MissingBody)?;
            let (arg_types, local_types) = slot_types(assembly, method, &target);
            lower_method_typed(&body, &resolver, &arg_types, &local_types).map(|(func, _)| func)
        })
        .collect()
}

/// Like [`lower_methods`], but also returns each method's [`crate::cil::CilSourceMap`] (the MIR-block
/// to CIL-offset map a debug line table is built from). So a whole multi-method program lowers WITH
/// debug info and its CROSS-METHOD CALLS RESOLVE -- unlike single-method `cil::lower_method_debug`,
/// which `UnresolvedCall`-panics on a call to another method. Pair with `arm32::lower_module_debug`.
pub fn lower_methods_debug(
    assembly: &Assembly,
    methods: &[Method],
) -> Result<(Vec<Function>, Vec<crate::cil::CilSourceMap>), CilError> {
    let rids: Vec<u32> = methods.iter().map(Method::rid).collect();
    let resolver = MetadataResolver::for_module(assembly, &rids);
    let target = TargetLayout::ilp32();
    let mut funcs = Vec::with_capacity(methods.len());
    let mut maps = Vec::with_capacity(methods.len());
    for method in methods {
        let body = method.body().ok_or(CilError::MissingBody)?;
        let (arg_types, local_types) = slot_types(assembly, method, &target);
        let (func, map) = lower_method_typed(&body, &resolver, &arg_types, &local_types)?;
        funcs.push(func);
        maps.push(map);
    }
    Ok((funcs, maps))
}

/// A method's argument and local MIR types, from its signature and local-variable
/// signature; a type the backend does not lower yet falls back to `int32`.
fn slot_types(
    assembly: &Assembly,
    method: &Method,
    target: &TargetLayout,
) -> (Vec<MirType>, Vec<MirType>) {
    let mut arg_types = Vec::new();
    if let Some(signature) = method.signature() {
        if signature.has_this {
            arg_types.push(MirType::ManagedPtr);
        }
        for param in &signature.parameters {
            arg_types.push(mir_type(param, assembly, target).unwrap_or(MirType::I32));
        }
    }
    let local_types = method
        .local_variables()
        .iter()
        .map(|local| mir_type(local, assembly, target).unwrap_or(MirType::I32))
        .collect();
    (arg_types, local_types)
}

/// The ONE table of `call` targets this backend FOLDS into an [`Intrinsic`] instead of emitting a
/// call to the method's own body. Keyed on exactly the data both a call site and the
/// `[RuntimeProvided]` seam census hold -- the declaring type's name, the method's name, and its
/// parameter signature -- so the two cannot answer differently about the same method. That matters
/// because a folded target's body is UNREACHABLE by any call: a seam left as a silent placeholder is
/// not a live wrong answer if every call to it was folded here, and a census that does not ask this
/// question reports it as one. (`synthesized_seam_body` is the same discipline for the other half of
/// a seam's fate: one place decides, and the report asks THAT place rather than restating it.)
///
/// The fold is only reachable for the call KINDS each arm of [`MetadataResolver::call_info`] admits;
/// this answers what the table claims, not which call sites reach it.
#[must_use]
pub fn folded_intrinsic(
    namespace: &str,
    type_name: &str,
    method_name: Option<&str>,
    parameters: &[SigType],
) -> Option<Intrinsic> {
    Some(match (namespace, type_name, method_name?) {
        ("System.Diagnostics", "Debug", "WriteLine") => Intrinsic::DebugWriteLine,
        ("System", "Console", "WriteLine") if matches!(parameters, [SigType::I4]) => {
            Intrinsic::ConsoleWriteLineInt
        }
        ("System", "String", "op_Equality")
            if matches!(parameters, [SigType::String, SigType::String]) =>
        {
            Intrinsic::StringEquals
        }
        ("System", "Object" | "Attribute", ".ctor") => Intrinsic::ObjectCtor,
        ("System", "Array", "GetLength") => Intrinsic::ArrayGetLength,
        ("System", "String", "Concat")
            if (2..=4).contains(&parameters.len())
                && parameters.iter().all(|p| matches!(p, SigType::String)) =>
        {
            Intrinsic::StringConcat
        }
        ("System", "Int32", "ToString") if parameters.is_empty() => Intrinsic::IntToString,
        _ => return None,
    })
}

/// [`folded_intrinsic`] asked of a resolved call target -- the form the lowering's own arms use, so
/// every predicate below is a view of the one table rather than a second copy of it.
fn intrinsic_of(method: &ResolvedMethod) -> Option<Intrinsic> {
    let declaring = method.declaring_type?;
    folded_intrinsic(
        declaring.namespace,
        declaring.name,
        method.name,
        method
            .signature
            .as_ref()
            .map_or(&[][..], |sig| sig.parameters.as_slice()),
    )
}

/// Whether a resolved method is `System.Diagnostics.Debug.WriteLine`.
fn is_debug_writeline(method: &ResolvedMethod) -> bool {
    matches!(intrinsic_of(method), Some(Intrinsic::DebugWriteLine))
}

/// Whether a resolved method is `System.Console.WriteLine(int)` -- the single-`int` overload,
/// distinguished from the many other `WriteLine` overloads by its parameter type.
fn is_console_writeline_int(method: &ResolvedMethod) -> bool {
    matches!(intrinsic_of(method), Some(Intrinsic::ConsoleWriteLineInt))
}

/// Whether a resolved method is a parameterless base-class constructor the lowering treats as a no-op:
/// `System.Object::.ctor()` (the universal base a derived ctor chains to) or `System.Attribute::.ctor()`
/// (so a user-defined attribute class -- e.g. a clean-room `[UnmanagedCallersOnly]` -- lowers; an
/// attribute's ctor is never run on this target, attributes being pure metadata).
fn is_noop_base_ctor(method: &ResolvedMethod) -> bool {
    matches!(intrinsic_of(method), Some(Intrinsic::ObjectCtor))
}

/// Whether a resolved method is `System.Array::GetLength(int)` -- the per-dimension length accessor
/// (used to loop over an array, including `int[,]`); the lowering reads it from the array header.
fn is_array_getlength(method: &ResolvedMethod) -> bool {
    matches!(intrinsic_of(method), Some(Intrinsic::ArrayGetLength))
}

/// Whether a resolved method is `System.String::op_Equality(string, string)` (the `==` operator).
fn is_string_op_equality(method: &ResolvedMethod) -> bool {
    matches!(intrinsic_of(method), Some(Intrinsic::StringEquals))
}

/// Whether a resolved method is a fixed-arity `System.String::Concat(string, ...)` -- the 2-, 3-, or
/// 4-string overloads `a + b`, `a + b + c`, `a + b + c + d` emit. The front end chains it pairwise.
/// (The `Concat(string[])` params-array and `Concat(object...)` overloads are not yet recognized.)
fn is_string_concat(method: &ResolvedMethod) -> bool {
    matches!(intrinsic_of(method), Some(Intrinsic::StringConcat))
}

/// Whether a resolved method is `System.Int32::ToString()` -- the no-argument decimal formatter
/// (`i.ToString()`). The receiver is a managed pointer to the int. (The format-string and
/// `IFormatProvider` overloads are not recognized.)
fn is_int32_tostring(method: &ResolvedMethod) -> bool {
    matches!(intrinsic_of(method), Some(Intrinsic::IntToString))
}

/// Decodes a `#US` entry (UTF-16 code units plus a trailing flag byte) to its CODE UNITS.
///
/// Units, not a [`String`], and that is the whole point of this function's shape. It used to end in
/// `String::from_utf16_lossy` and hand back text, which the `ldstr` lowering then re-encoded to UTF-16
/// -- a round trip through a type that CANNOT HOLD a lone surrogate. So `"a\u{D800}b"` reached every
/// backend as `"a\u{FFFD}b"`, in EVERY tier including the default one whose storage is UTF-16 and can
/// hold it perfectly well.
///
/// The `#US` heap is UTF-16 and a Lamella string is UTF-16 at the managed level, so text is not on the
/// path at all; the one consumer that genuinely wants bytes (a semihosting console write) converts for
/// itself, where the loss is in a diagnostic rather than in the program's data.
fn decode_user_string(raw: &[u8]) -> Vec<u16> {
    let units = raw.len().saturating_sub(1) / 2;
    (0..units)
        .map(|i| u16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_user_string() {
        assert_eq!(decode_user_string(&[0x48, 0x00, 0x69, 0x00, 0x00]), [0x48, 0x69]);
        assert_eq!(
            decode_user_string(&[0x61, 0x00, 0x00, 0xD8, 0x62, 0x00, 0x00]),
            [0x0061, 0xD800, 0x0062]
        );
    }

    #[test]
    fn extern_symbol_encodes_param_types_for_overloads() {
        let none = |_t: Token| -> Option<String> { None };
        assert_eq!(
            extern_method_symbol("System", "Math", "Max", &[SigType::I4, SigType::I4], &none),
            "System.Math.Max.ii"
        );
        assert_eq!(
            extern_method_symbol("System", "Math", "Max", &[SigType::I8, SigType::I8], &none),
            "System.Math.Max.ll"
        );
        assert_eq!(
            extern_method_symbol("", "MathLib", "Answer", &[], &none),
            "MathLib.Answer."
        );
    }

    #[test]
    fn extern_symbol_encodes_type_identity_of_reference_and_value_params() {
        let names = |t: Token| match t.0 {
            1 => Some(String::from("System.DateTime")),
            2 => Some(String::from("System.TimeSpan")),
            _ => None,
        };
        let dt = SigType::ValueType(Token(1));
        let ts = SigType::ValueType(Token(2));
        assert_eq!(
            extern_method_symbol(
                "System",
                "DateTime",
                "op_Subtraction",
                &[dt.clone(), dt.clone()],
                &names
            ),
            "System.DateTime.op_Subtraction.VSystem.DateTime;VSystem.DateTime;"
        );
        assert_eq!(
            extern_method_symbol("System", "DateTime", "op_Subtraction", &[dt, ts], &names),
            "System.DateTime.op_Subtraction.VSystem.DateTime;VSystem.TimeSpan;"
        );
        assert_eq!(
            extern_method_symbol(
                "System",
                "Array",
                "Sort",
                &[SigType::SzArray(Box::new(SigType::I4))],
                &names
            ),
            "System.Array.Sort.ai"
        );
        assert_eq!(
            extern_method_symbol(
                "System",
                "Int32",
                "TryParse",
                &[SigType::String, SigType::ByRef(Box::new(SigType::I4))],
                &names
            ),
            "System.Int32.TryParse.qri"
        );
    }



    #[test]
    fn the_frozen_primitive_element_codes_match_the_runtime_enum() {
        for (name, code) in [
            ("SByte", 1),
            ("Byte", 2),
            ("Boolean", 2),
            ("Int16", 3),
            ("UInt16", 4),
            ("Char", 4),
            ("Int32", 5),
            ("UInt32", 5),
            ("Int64", 6),
            ("UInt64", 6),
            ("Single", 7),
            ("Double", 8),
        ] {
            assert_eq!(
                primitive_element_kind("System", name),
                Some(code),
                "System.{name} must carry the frozen code {code}"
            );
        }
        assert_eq!(primitive_element_kind("System", "String"), None);
        assert_eq!(primitive_element_kind("System", "Object"), None);
        assert_eq!(primitive_element_kind("MyApp", "Int32"), None);
    }

    #[test]
    fn the_fold_table_answers_the_seam_census_and_the_lowering_alike() {
        use SigType::{I4, Object, String as Str};
        assert_eq!(
            folded_intrinsic("System", "String", Some("Concat"), &[Str, Str]),
            Some(Intrinsic::StringConcat)
        );
        assert_eq!(
            folded_intrinsic("System", "String", Some("Concat"), &[Str, Str, Str, Str]),
            Some(Intrinsic::StringConcat)
        );
        assert_eq!(
            folded_intrinsic("System", "String", Some("Concat"), &[Object, Object]),
            None
        );
        assert_eq!(
            folded_intrinsic("System", "String", Some("Concat"), &[Str]),
            None
        );
        assert_eq!(
            folded_intrinsic("System", "Array", Some("GetLength"), &[I4]),
            Some(Intrinsic::ArrayGetLength)
        );
        assert_eq!(
            folded_intrinsic("System", "Int32", Some("ToString"), &[]),
            Some(Intrinsic::IntToString)
        );
        assert_eq!(
            folded_intrinsic("System", "Int32", Some("ToString"), &[Str]),
            None,
            "the format-string overload is a real call"
        );
        assert_eq!(
            folded_intrinsic("System", "String", Some("op_Equality"), &[Str, Str]),
            Some(Intrinsic::StringEquals)
        );
        assert_eq!(
            folded_intrinsic("System", "Object", Some(".ctor"), &[]),
            Some(Intrinsic::ObjectCtor)
        );
        assert_eq!(
            folded_intrinsic("System", "Console", Some("WriteLine"), &[I4]),
            Some(Intrinsic::ConsoleWriteLineInt)
        );
        assert_eq!(
            folded_intrinsic("System", "Console", Some("WriteLine"), &[Str]),
            None,
            "only the single-int overload is folded"
        );
        assert_eq!(
            folded_intrinsic("System.Diagnostics", "Debug", Some("WriteLine"), &[Str]),
            Some(Intrinsic::DebugWriteLine)
        );
        assert_eq!(
            folded_intrinsic("MyApp", "String", Some("Concat"), &[Str, Str]),
            None
        );
        assert_eq!(folded_intrinsic("System", "String", None, &[]), None);
    }

    #[test]
    fn the_array_marker_cannot_collide_with_a_payload_or_an_element_kind() {
        assert!(
            ARRAY_DESC_MARK > 0x0010_0000,
            "the marker must sit far above any real payload size"
        );
        assert_eq!(ARRAY_DESC_MARK & ARRAY_DESC_MARK_MASK, ARRAY_DESC_MARK);
        for rank in 1u32..=8 {
            let word = ARRAY_DESC_MARK | rank;
            assert_eq!(word & ARRAY_DESC_MARK_MASK, ARRAY_DESC_MARK, "rank {rank}");
            assert_eq!(word & !ARRAY_DESC_MARK_MASK, rank, "rank {rank} round-trips");
        }
        assert_eq!(ELEMENT_KIND_REFERENCE, 0);
        for (name, _) in [("SByte", ()), ("Double", ())] {
            assert_ne!(
                primitive_element_kind("System", name),
                Some(ELEMENT_KIND_REFERENCE)
            );
        }
        assert!(
            !(1..=8).contains(&ELEMENT_KIND_OPAQUE),
            "OPAQUE must not alias a frozen primitive code"
        );
        assert_ne!(ELEMENT_KIND_OPAQUE, ELEMENT_KIND_REFERENCE);
        assert_eq!(
            ELEMENT_KIND_UTF16_UNIT,
            primitive_element_kind("System", "Char").expect("Char is a frozen primitive"),
            "the string blob's element kind must be the frozen UTF-16 code unit"
        );
        assert_eq!(
            primitive_element_kind("System", "UInt16"),
            Some(ELEMENT_KIND_UTF16_UNIT),
            "Char and UInt16 share one frozen code"
        );
    }
}
